//! Nous 匿名免费档：身份创建 / JWT 换取 + 进程级缓存。
//!
//! ── 匿名档是什么（规格 B）────────────────────────────────────
//! 「一个账号都没有」时，网关透明地以匿名身份打**免费面**（`welcome-api`），
//! 只认模型 `nous/welcome` —— 与官方 `hermes-agent` 的行为一致。两步：
//! ```text
//!   ① POST {portal}/api/anonymous/create
//!        header: User-Agent: hermes-agent/<ver> (cli; <os>; <arch>)  ← 门禁，错则 403
//!        body:   {}
//!      → { user_id, org_id, token: "anon_…", idle_ttl_days }
//!   ② POST {portal}/api/anonymous/token
//!        body:   { "token": "anon_…",
//!                  "client": { name, version, surface, platform, capabilities:[…] },
//!                  "purpose": "inference" }
//!      → { access_token (JWT ~900s), expires_in, inference_base_url, challenges:[…] }
//! ```
//! 推理只打免费面（见 `endpoints::WELCOME_BASE_URL`）。
//!
//! ── JWT 只有 ~15 分钟：什么时候重换 ──────────────────────────
//! `anon_…` 身份本身有 ~14 天闲置 TTL，但换出来的 JWT 只有 ~900 秒。因此：
//!   - 正常路径：JWT 快过期（余量见 `JWT_REFRESH_MARGIN_MS`）时**重换**一次
//!     （用缓存里的 `anon_…` 身份再 POST 一次 token 端点，成本极低）；
//!   - 401/404：身份可能已被回收，**重建**一次身份再换（规格：只在 404/401 时重建）。
//!
//! ── 为什么缓存放进程内存 ────────────────────────────────────
//! 匿名身份不是账号（不进账号库）：它只是「没有账号时的一条兜底通道」。
//! 进程重启后重建一次即可（一次 POST），不落盘 —— 避免在用户机器上留下一条
//! 与任何账号都无关的长期凭证。并发换 JWT 用 `refresh_flight` 单飞去重。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::refresh_flight::{Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::endpoints;

/// JWT 余量：剩余寿命低于它就先重换（避免「刚好在途中过期」）
const JWT_REFRESH_MARGIN_MS: i64 = 60_000;

/// 匿名身份 + 当前 JWT 的进程级缓存
struct AnonymousState {
    /// `anon_…` 身份（重建前一直复用）
    token: String,
    /// 当前 JWT（免费面 Bearer）
    jwt: String,
    /// JWT 过期时刻（毫秒）
    jwt_expires_at: i64,
    /// 匿名 user id（展示 / 排障用）
    user_id: String,
    /// 匿名 org id
    org_id: String,
}

static STATE: OnceLock<Mutex<Option<AnonymousState>>> = OnceLock::new();

/// 单飞表：并发换 JWT 只发一次真实请求（key 恒为 `anonymous`）
static FLIGHTS: OnceLock<Table<String>> = OnceLock::new();

fn state() -> &'static Mutex<Option<AnonymousState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn lock() -> MutexGuard<'static, Option<AnonymousState>> {
    state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 取当前仍有效的 JWT（余量不足返回 None）
fn cached_valid() -> Option<String> {
    let guard = lock();
    let entry = guard.as_ref()?;
    let now = logging::now_ms();
    if !entry.jwt.is_empty() && entry.jwt_expires_at - now > JWT_REFRESH_MARGIN_MS {
        Some(entry.jwt.clone())
    } else {
        None
    }
}

/// 缓存里的匿名身份串（可能为空 = 还没建过）
fn cached_identity() -> String {
    let guard = lock();
    guard.as_ref().map(|entry| entry.token.clone()).unwrap_or_default()
}

/// 取一个可用的匿名 JWT（免费面 Bearer）。
///
/// `force = true` 时无视缓存重换一次（401 之后的强制路径）。
pub async fn ensure_token(force: bool) -> Result<String, GatewayError> {
    if !force {
        if let Some(jwt) = cached_valid() {
            return Ok(jwt);
        }
    }
    match FLIGHTS.get_or_init(Table::new).join("anonymous") {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh().await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 打一次「（必要时重建身份）+ 换 JWT」并回写缓存（单飞的 leader 走这一段）。
async fn refresh() -> Result<String, GatewayError> {
    let identity = cached_identity();
    let (token, user_id, org_id) = if identity.is_empty() {
        let created = create_identity().await?;
        (created.token, created.user_id, created.org_id)
    } else {
        // 复用身份时 user/org 已知（缓存里那份）
        let (user_id, org_id) = {
            let guard = lock();
            guard
                .as_ref()
                .map(|entry| (entry.user_id.clone(), entry.org_id.clone()))
                .unwrap_or_default()
        };
        (identity, user_id, org_id)
    };
    let bundle = match exchange(&token).await {
        Ok(bundle) => bundle,
        Err(error) if error.status_code == 401 || error.status_code == 404 => {
            // 身份被回收：重建一次再换（规格：只在 401/404 时重建）
            let created = create_identity().await?;
            let bundle = exchange(&created.token).await?;
            write_state(&created.token, &created.user_id, &created.org_id, &bundle);
            return Ok(bundle.access_token);
        }
        Err(error) => return Err(error),
    };
    write_state(&token, &user_id, &org_id, &bundle);
    Ok(bundle.access_token)
}

/// 写回缓存
fn write_state(token: &str, user_id: &str, org_id: &str, bundle: &JwtBundle) {
    let now = logging::now_ms();
    let mut guard = lock();
    *guard = Some(AnonymousState {
        token: token.to_string(),
        jwt: bundle.access_token.clone(),
        jwt_expires_at: now + bundle.expires_in.saturating_mul(1000),
        user_id: user_id.to_string(),
        org_id: org_id.to_string(),
    });
}

/// 创建匿名身份
struct CreatedIdentity {
    token: String,
    user_id: String,
    org_id: String,
}

async fn create_identity() -> Result<CreatedIdentity, GatewayError> {
    let headers = endpoints::anonymous_headers();
    let body = json!({});
    let response = send_raw(
        "POST",
        &endpoints::portal_url(endpoints::ANONYMOUS_CREATE_PATH),
        Some(&body),
        &headers,
        None,
        Some(20_000),
    )
    .await
    .map_err(|error| {
        GatewayError::with_status(502, format!("Nous 匿名身份创建失败：{error}"))
    })?;
    let payload = response.payload.unwrap_or(Value::Null);
    if !response.ok {
        let message = payload
            .get("message")
            .or_else(|| payload.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(GatewayError::with_status(
            502,
            format!(
                "Nous 匿名身份创建被拒（HTTP {}）{}：请确认本机出网正常",
                response.status,
                if message.is_empty() {
                    String::new()
                } else {
                    format!("：{message}")
                }
            ),
        ));
    }
    let token = payload
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(502, "Nous 匿名身份创建响应缺少 token")
        })?
        .to_string();
    Ok(CreatedIdentity {
        token,
        user_id: payload
            .get("user_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        org_id: payload
            .get("org_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// 换到的 JWT 包
struct JwtBundle {
    access_token: String,
    expires_in: i64,
}

async fn exchange(anon_token: &str) -> Result<JwtBundle, GatewayError> {
    let headers = endpoints::anonymous_headers();
    let body = json!({
        "token": anon_token,
        "client": {
            "name": endpoints::ANONYMOUS_CLIENT_NAME,
            "version": endpoints::ANONYMOUS_CLIENT_VERSION,
            "surface": endpoints::ANONYMOUS_CLIENT_SURFACE,
            "platform": endpoints::platform(),
            "capabilities": endpoints::ANONYMOUS_CAPABILITIES,
        },
        "purpose": "inference",
    });
    let response = send_raw(
        "POST",
        &endpoints::portal_url(endpoints::ANONYMOUS_TOKEN_PATH),
        Some(&body),
        &headers,
        None,
        Some(20_000),
    )
    .await
    .map_err(|error| GatewayError::with_status(502, format!("Nous 匿名令牌换取失败：{error}")))?;
    let payload = response.payload.unwrap_or(Value::Null);
    if !response.ok {
        let message = payload
            .get("message")
            .or_else(|| payload.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(GatewayError::with_status(
            response.status as i32,
            format!(
                "Nous 匿名令牌换取被拒（HTTP {}）{}",
                response.status,
                if message.is_empty() {
                    String::new()
                } else {
                    format!("：{message}")
                }
            ),
        ));
    }
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "Nous 匿名令牌响应缺少 access_token"))?
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(900);
    Ok(JwtBundle {
        access_token,
        expires_in,
    })
}

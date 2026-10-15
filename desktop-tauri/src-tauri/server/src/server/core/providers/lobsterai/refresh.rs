//! LobsterAI 令牌续期：`POST {base}/api/auth/refresh`（keyfrom 信封 + refreshToken）。
//!
//! ── 上游形态 ────────────────────────────────────────────────
//! ```text
//! POST {base}/api/auth/refresh
//!   body: { firstKeyfrom, latestKeyfrom, version:"0.1.0", uuid?, userId?, refreshToken }
//!   → { accessToken, refreshToken, expiresIn }
//! ```
//! access token 约 30 天、refresh token 约 180 天（契约）。
//!
//! ── 单飞 + 比较再写（与 Antigravity / Nous / OfficeAce 同一纪律）──
//! 同一凭证的并发刷新只发一次真实请求（`refresh_flight`）；回写走
//! `update_lobsterai_credentials_if_current`（刷新前那份 refresh token 仍一致才
//! 写，否则 [`CredentialWrite::Stale`]）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::OnceLock;

use serde_json::Value;

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{credentials, endpoints, envelope};

/// 单次请求超时
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 单飞表（键 = 账号文件 + 账号 id + refresh_token 指纹）
static FLIGHTS: OnceLock<Table<credentials::LobsterAiCredentials>> = OnceLock::new();

/// 一次续期读到的新凭证
#[derive(Clone, Debug)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: i64,
}

/// 打一次 `POST /api/auth/refresh`。
pub async fn refresh_tokens(
    cred: &credentials::LobsterAiCredentials,
) -> Result<RefreshedTokens, GatewayError> {
    let body = cred.keyfrom_json(true);
    let response = send_raw(
        "POST",
        &endpoints::url(endpoints::REFRESH_PATH),
        Some(&body),
        &[(
            "User-Agent".to_string(),
            format!("LobsterAI/{}", endpoints::real_version()),
        )],
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        GatewayError::with_status(502, format!("LobsterAI 令牌续期请求失败：{error}"))
    })?;
    if !response.ok {
        let message = upstream_message(&response.payload);
        return Err(GatewayError::with_status(
            response.status as i32,
            if message.is_empty() {
                format!("LobsterAI 令牌续期被拒（HTTP {}）", response.status)
            } else {
                format!("LobsterAI 令牌续期被拒：{message}")
            },
        ));
    }
    let payload = response.payload.unwrap_or(Value::Null);
    // 上游统一信封 `{code, message, data}`：业务失败看 `code != 0`，成功数据在 `data`
    if let Some(message) = envelope::business_error(&payload) {
        return Err(GatewayError::with_status(
            502,
            format!("LobsterAI 令牌续期被拒：{message}"),
        ));
    }
    let data = envelope::data(&payload);
    let access_token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "LobsterAI 续期响应缺少 accessToken"))?
        .to_string();
    let expires_in = data
        .get("expiresIn")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(30 * 24 * 3600);
    Ok(RefreshedTokens {
        access_token,
        refresh_token: data
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        expires_in,
    })
}

/// 取可用账号凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
///
/// `account_id` 为空 = 本家的队首可用账号（点名的账号取不到直接报错，不回落队首）。
pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<credentials::LobsterAiCredentials, GatewayError> {
    let record = read_record(store, account_id)?;
    let cred = credentials::from_record(Some(&record))?;
    if !force && !cred.expiring() {
        return Ok(cred);
    }
    if !cred.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "LobsterAI 账号缺少 refresh token，无法续期：请重新登录或重新粘贴令牌",
        ));
    }
    let key = format!(
        "{}:{}:{}",
        store.file_string(),
        cred.id,
        refresh_flight::fingerprint(&cred.refresh_token)
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(store, &cred, force).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 打一次刷新 + 回写（单飞的 leader 走这一段）。
async fn refresh_and_save(
    store: &AccountStore,
    cred: &credentials::LobsterAiCredentials,
    force: bool,
) -> Result<credentials::LobsterAiCredentials, GatewayError> {
    let now = logging::now_ms();
    // 非强制路径下：仍然新鲜就直接用（另一个并发 leader 刚刷完）
    if !force && !cred.expiring() {
        return Ok(cred.clone());
    }
    let refreshed = refresh_tokens(cred).await?;
    let stored_refresh = refreshed
        .refresh_token
        .clone()
        .unwrap_or_else(|| cred.refresh_token.clone());
    let expires_at = now + refreshed.expires_in.saturating_mul(1000);
    let write = store.update_lobsterai_credentials_if_current(
        &cred.id,
        &cred.refresh_token,
        &refreshed.access_token,
        Some(stored_refresh.as_str()),
        expires_at,
        now,
    );
    match write {
        Ok(CredentialWrite::Written) => {}
        Ok(CredentialWrite::Stale) => {
            logging::log(
                "[LobsterAI]",
                "⚠️ 续期结果未能写回账号记录（期间被改动过），本次请求用新凭证，下次以记录为准",
            );
        }
        Err(reason) => return Err(GatewayError::with_status(500, reason)),
    }
    logging::verbose(
        "[LobsterAI]",
        &format!(
            "令牌续期成功（access token {}，有效期至 {expires_at}）",
            credentials::masked(&refreshed.access_token)
        ),
    );
    Ok(credentials::LobsterAiCredentials {
        access_token: refreshed.access_token,
        refresh_token: stored_refresh,
        expires_at,
        latest_keyfrom: now,
        ..cred.clone()
    })
}

/// 读账号记录（点名 vs 队首）。
fn read_record(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    if let Some(record) = store.lobsterai_account_record(account_id) {
        return Ok(record);
    }
    if account_id.trim().is_empty() {
        Err(GatewayError::with_status(
            401,
            "没有可用的 LobsterAI 账号：请在账号页粘贴令牌或走浏览器登录添加",
        ))
    } else {
        Err(GatewayError::with_status(
            401,
            format!("指定的 LobsterAI 账号不存在或不可用：{account_id}"),
        ))
    }
}

/// 从上游错误体里读一条可读文案（message / msg / error.message）
fn upstream_message(payload: &Option<Value>) -> String {
    let Some(payload) = payload else {
        return String::new();
    };
    payload
        .get("message")
        .or_else(|| payload.get("msg"))
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

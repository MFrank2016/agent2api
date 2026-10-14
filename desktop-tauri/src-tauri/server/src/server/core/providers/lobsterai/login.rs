//! LobsterAI 登录：浏览器 OAuth 授权码换令牌 + 粘贴令牌归一化。
//!
//! ── 浏览器 OAuth（本家唯一无 SMS 的登录方式）────────────────────
//! ```text
//! ① 打开 {portal}/portal#/login?source=electron&redirect_uri=<loopback>&state=<state>
//! ② 门户（用户在浏览器里**手动**过滑块验证码）完成后 302 回本机 loopback：
//!      http://127.0.0.1:{port}/auth/callback?code=..&state=..
//! ③ POST {base}/api/auth/exchange
//!      body: { authCode, firstKeyfrom, latestKeyfrom, uuid, version:"0.1.0" }
//!      （**无 Bearer**，UA "LobsterAI/0.1.0"）
//!   → { accessToken, refreshToken, expiresIn, user:{id,yid,userId,nickname}, quota }
//! ```
//! loopback 回调落在**本网关自己的端口**上（`/auth/callback` 的 public 路由，
//! 见 `http.rs` 与 `api::session::login_lobsterai_callback`），与 Antigravity /
//! Accio 同一形态：授权地址由本模块本地拼（`build_authorize_url`），端口从
//! [`endpoints::login_redirect_uri`] 取。
//!
//! ── state 校验 ──────────────────────────────────────────────
//! `state` 是本地生成的一次性随机串（`new_login_state`，复用项目既有的
//! `new_request_id`：没有 `rand` / `uuid` 依赖，为一个 state 引进它们不合算）。
//! 逐字比对在 `core::login::submit_login_callback` 的 LobsterAI 分支里做。
//!
//! ── 粘贴令牌 ────────────────────────────────────────────────
//! 桌面端把登录态落在 `%APPDATA%/LobsterAI/lobsterai.sqlite` 的
//! `kv.auth_tokens = {accessToken, refreshToken}`。用户可以直接把这两个令牌
//! （或整段 JSON）粘进来，由 [`normalize_paste`] 归一。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::core::upstream::request::new_request_id;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::endpoints;

/// exchange 请求超时
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 生成一次登录的 `state`（不可预测的一次性随机串）。
pub fn new_login_state() -> String {
    new_request_id()
}

/// 从回调 URL 的查询串里取授权码（逐字比对 `state`）。
pub fn parse_callback_code(callback_url: &str, state: &str) -> Result<String, GatewayError> {
    let parsed = url::Url::parse(callback_url)
        .map_err(|_| GatewayError::with_status(400, "LobsterAI 登录回调地址无效"))?;
    let params: std::collections::HashMap<String, String> = parsed
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if let Some(error) = params.get("error").filter(|value| !value.trim().is_empty()) {
        return Err(GatewayError::with_status(
            400,
            format!("授权被拒绝（{error}）"),
        ));
    }
    let callback_state = params.get("state").map(String::as_str).unwrap_or("");
    if callback_state.trim() != state {
        return Err(GatewayError::with_status(
            400,
            "回调地址与当前 LobsterAI 登录任务不匹配",
        ));
    }
    let code = params
        .get("code")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::with_status(400, "登录回调缺少授权码（code）"))?;
    Ok(code.to_string())
}

/// 授权码换令牌并落账号（返回账号 id）。
///
/// `state` 已由调用方逐字比对过；这里再做一次非空校验作为深度防御。
pub async fn exchange(
    store: &AccountStore,
    code: &str,
    state: &str,
) -> Result<String, GatewayError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "登录回调缺少授权码（code）"));
    }
    if state.trim().is_empty() {
        return Err(GatewayError::with_status(400, "登录回调缺少 state"));
    }
    // 首次交换：keyfrom 两端都取当前时刻，设备 id 新生成一枚（后续随账号持久化）
    let now = logging::now_ms();
    let device_id = new_request_id();
    let body = json!({
        "authCode": code,
        "firstKeyfrom": now,
        "latestKeyfrom": now,
        "uuid": device_id,
        "version": endpoints::PROTOCOL_VERSION,
    });
    // 契约：exchange **不带 Bearer**，UA 固定 `LobsterAI/0.1.0`
    let headers = vec![(
        "User-Agent".to_string(),
        format!("LobsterAI/{}", endpoints::PROTOCOL_VERSION),
    )];
    let response = send_raw(
        "POST",
        &endpoints::url(endpoints::EXCHANGE_PATH),
        Some(&body),
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| GatewayError::with_status(502, format!("LobsterAI 授权码交换失败：{error}")))?;
    if !response.ok {
        let message = upstream_message(&response.payload);
        return Err(GatewayError::with_status(
            response.status as i32,
            if message.is_empty() {
                format!("LobsterAI 授权码交换被拒（HTTP {}）", response.status)
            } else {
                format!("LobsterAI 授权码交换被拒：{message}")
            },
        ));
    }
    let payload = response.payload.unwrap_or(Value::Null);
    let access_token = payload
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "LobsterAI 交换响应缺少 accessToken"))?
        .to_string();
    let refresh_token = payload
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let expires_in = payload
        .get("expiresIn")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(30 * 24 * 3600);
    let user = payload.get("user").cloned().unwrap_or(Value::Null);
    let user_id = user
        .get("userId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let nickname = user
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let account_payload = json!({
        "accessToken": access_token,
        "refreshToken": refresh_token,
        "expiresAt": now + expires_in.saturating_mul(1000),
        "userId": user_id,
        "deviceId": device_id,
        "firstKeyfrom": now,
        "latestKeyfrom": now,
        "user": user,
    });
    let account = store
        .add_lobsterai_account(&account_payload, nickname.as_deref(), "web")
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
    Ok(account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

/// 粘贴令牌归一化：抽出 accessToken / refreshToken。
///
/// 形态来源：桌面端 `%APPDATA%/LobsterAI/lobsterai.sqlite` 的
/// `kv.auth_tokens = {accessToken, refreshToken}`。用户可能：
///   - 直接填 `accessToken` / `refreshToken` 两个字段；
///   - 把整段 `{"accessToken":..,"refreshToken":..}` JSON 粘进**一个**输入框
///     （字段名可能是 token / content / 任意）。
/// 因此这里在多个候选对象里找第一对非空令牌，并把结果规范成
/// `{accessToken, refreshToken}`（其余顶层字段原样保留）。
pub fn normalize_paste(payload: &Value) -> Result<Value, GatewayError> {
    let mut candidates: Vec<Value> = Vec::new();
    if let Some(text) = payload.as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(text.trim()) {
            candidates.push(parsed);
        }
    } else if let Some(object) = payload.as_object() {
        candidates.push(payload.clone());
        // 某个字段的值本身是一段 JSON（整段 auth_tokens 粘进一个输入框）
        for value in object.values() {
            if let Some(text) = value.as_str() {
                if let Ok(parsed) = serde_json::from_str::<Value>(text.trim()) {
                    if parsed.is_object() {
                        candidates.push(parsed);
                    }
                }
            }
        }
    }
    let access_token = candidates
        .iter()
        .map(|candidate| pick(candidate, &["accessToken", "access_token"]))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    let refresh_token = candidates
        .iter()
        .map(|candidate| pick(candidate, &["refreshToken", "refresh_token"]))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "缺少 access token：请粘贴桌面端 kv.auth_tokens 里的 accessToken（与 refreshToken）",
        ));
    }
    let mut normalized = if payload.is_object() {
        payload.clone()
    } else {
        json!({})
    };
    if let Some(object) = normalized.as_object_mut() {
        object.insert("accessToken".to_string(), Value::String(access_token));
        if !refresh_token.is_empty() {
            object.insert("refreshToken".to_string(), Value::String(refresh_token));
        }
    }
    Ok(normalized)
}

/// 从取值链里挑第一个非空字符串
fn pick(payload: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = payload
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 从上游错误体里读一条可读文案
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

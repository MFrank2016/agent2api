//! LobsterAI 每日签到（+100 积分）。
//!
//! ── 三步时序（契约）─────────────────────────────────────────
//! ```text
//! ① GET  {base}/api/client-activities/slot?placement=desktop_sidebar
//!        &clientVersion=<REALVER>&containerApiVersion=2&platform=win32   (Bearer)
//!      → { slotState, activity: { activityCode, configRevision } }
//! ② GET  {base}/api/client-activities/{code}/context?configRevision={rev} (Bearer)
//!      → { state: { claimedToday }, actions: [ … ] }
//! ③ POST {base}/api/client-activities/{code}/actions/check_in              (Bearer)
//!      body: { configRevision, idempotencyKey: <uuid>, payload: {} }
//!      → { replayed, result: { creditsGranted, … }, context }
//! ④ 再拉一次 ②；`claimedToday` 翻转或 `creditsGranted > 0` 才算成功。
//! ```
//! 已领取（`claimedToday == true`）是**幂等成功**（`alreadyCompleted`）。
//!
//! ── `clientVersion` 必须是真实版本 ───────────────────────────
//! slot 用**过时的**版本（如 `"0.1.0"`）会返回 `slotState=empty`（活动被隐藏）。
//! 因此这里先 best-effort 刷新真实版本（[`endpoints::refresh_real_version`]），
//! 再用 [`endpoints::real_version`] 填 `clientVersion`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::core::upstream::request::new_request_id;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{credentials, endpoints};

/// 签到请求超时
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 一次带 Bearer + 能力头的上游调用，返回解析后的 JSON（失败给可读错误）。
async fn call(
    method: &str,
    url: &str,
    token: &str,
    body: Option<&Value>,
) -> Result<Value, GatewayError> {
    let mut headers = endpoints::capability_headers();
    headers.extend(endpoints::auth_headers(token));
    let response = send_raw(method, url, body, &headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| GatewayError::with_status(502, format!("LobsterAI 签到请求失败：{error}")))?;
    if response.status == 401 || response.status == 403 {
        return Err(GatewayError::with_status(
            401,
            "LobsterAI 登录态已失效，无法签到（请重新登录）",
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("LobsterAI 签到接口返回 HTTP {}", response.status),
        ));
    }
    Ok(response.payload.unwrap_or(Value::Null))
}

/// 从 `activity` 里取 `activityCode` / `configRevision`（数字或字符串都收）。
fn activity_of(slot: &Value) -> Option<(String, String)> {
    let activity = slot.get("activity")?;
    let code = activity
        .get("activityCode")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let revision = activity
        .get("configRevision")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    Some((code, revision))
}

/// `state.claimedToday`（布尔或 "true"）
fn claimed_today(context: &Value) -> bool {
    match context.pointer("/state/claimedToday") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.trim() == "true",
        _ => false,
    }
}

/// 领今天的签到奖励（`billing::checkin_for` 的 `{success, msg}` 契约）。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = store.lobsterai_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(
            404,
            format!("LobsterAI 账号 {account_id} 不存在（请重新添加）"),
        ));
    }
    let cred = credentials::from_record(record.as_ref())?;
    if cred.access_token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "LobsterAI 账号缺少 access token，无法签到（请重新登录）",
        ));
    }
    // slot 的 clientVersion 必须是真实版本（过时版本 → slotState=empty）
    endpoints::refresh_real_version().await;
    let version = endpoints::real_version();

    // ① 槽位
    let slot_url = format!(
        "{}?placement=desktop_sidebar&clientVersion={}&containerApiVersion=2&platform=win32",
        endpoints::url(endpoints::ACTIVITY_SLOT_PATH),
        crate::server::core::auth::urlencoding(&version),
    );
    let slot = call("GET", &slot_url, &cred.access_token, None).await?;
    let Some((activity_code, revision)) = activity_of(&slot) else {
        // 没有可参与的活动（含版本过时被判成 empty）：中性未领取，不算失败
        return Ok(json!({
            "success": false,
            "msg": "今天没有可领取的签到活动",
        }));
    };

    // ② 上下文
    let context_url = format!(
        "{}/api/client-activities/{}/context?configRevision={}",
        endpoints::base_url(),
        crate::server::core::auth::urlencoding(&activity_code),
        crate::server::core::auth::urlencoding(&revision),
    );
    let context = call("GET", &context_url, &cred.access_token, None).await?;
    if claimed_today(&context) {
        return Ok(json!({
            "success": true,
            "alreadyCompleted": true,
            "msg": "今天已签到",
        }));
    }

    // ③ 领取
    let checkin_url = format!(
        "{}/api/client-activities/{}/actions/check_in",
        endpoints::base_url(),
        crate::server::core::auth::urlencoding(&activity_code),
    );
    let body = json!({
        "configRevision": revision,
        "idempotencyKey": new_request_id(),
        "payload": {},
    });
    let result = call("POST", &checkin_url, &cred.access_token, Some(&body)).await?;
    let credits = result
        .pointer("/result/creditsGranted")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    // ④ 回读确认（claimedToday 翻转 或 creditsGranted > 0 才算成功）
    let after = call("GET", &context_url, &cred.access_token, None)
        .await
        .unwrap_or(Value::Null);
    let flipped = claimed_today(&after);
    if flipped || credits > 0 {
        let msg = if credits > 0 {
            format!("签到成功，获得 {credits} 积分")
        } else {
            "签到成功".to_string()
        };
        logging::log("[Checkin]", &format!("LobsterAI {msg}"));
        Ok(json!({ "success": true, "msg": msg }))
    } else {
        logging::verbose("[Checkin]", "LobsterAI 签到未确认到账（claimedToday 未翻转、creditsGranted=0）");
        Ok(json!({
            "success": false,
            "msg": "签到未确认到账（请稍后重试）",
        }))
    }
}

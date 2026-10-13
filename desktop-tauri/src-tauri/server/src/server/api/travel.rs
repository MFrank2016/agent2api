//! 派猫猫旅行的两个 per-account 端点。
//!
//!   GET  /api/accounts/{id}/travel   只读状态（卡片惰性查询，零写）
//!   POST /api/accounts/{id}/travel   跑一次 travel_auto（手动派发/领取）
//!
//! 只服务 CN WorkBuddy 账号；其它 provider / 国际版返回 400（无此活动）。

use axum::response::Response;
use serde_json::{json, Value};

use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::ServerState;

/// 取账号 session；非 CN WorkBuddy 账号返回 Err(message)。
fn cn_session(state: &ServerState, account_id: &str) -> Result<Value, String> {
    let Some(entry) = state.store().get_session_by_id(account_id) else {
        return Err("没有可用凭证".to_string());
    };
    let edition = entry.session.get("edition").and_then(Value::as_str).unwrap_or("");
    if edition == "intl" {
        return Err("国际版没有旅行活动".to_string());
    }
    Ok(entry.session)
}

pub async fn status(state: &ServerState, account_id: &str) -> Response {
    let session = match cn_session(state, account_id) {
        Ok(session) => session,
        Err(message) => return management_error(400, message),
    };
    let status = match state.billing().travel_status(&session).await {
        Ok(status) => status,
        Err(error) => return management_error(error.status_code, error.message),
    };
    let config = state.billing().travel_config(&session).await.unwrap_or(Value::Null);
    let auto = state.auto_checkin().state().get("travel").cloned().unwrap_or(Value::Null);
    ok_json(json!({
        "status": {
            "available": status.available,
            "state": status.state,
            "rewardCredit": status.reward_credit,
            "dailyLimitReached": status.daily_limit_reached,
            "location": status.location,
            "arriveAt": status.arrive_at,
            "serverNow": status.server_now,
            "letter": status.letter,
            "useDeeplink": status.use_deeplink,
        },
        "config": { "locations": config.get("locations").cloned().unwrap_or(Value::Null) },
        "auto": auto,
    }))
}

pub async fn run(state: &ServerState, account_id: &str, body: &Value) -> Response {
    let session = match cn_session(state, account_id) {
        Ok(session) => session,
        Err(message) => return management_error(400, message),
    };
    let opts = crate::server::core::billing::TravelOpts {
        location_id: body.get("locationId").and_then(Value::as_i64),
    };
    let result = state.billing().travel_auto(&session, opts).await;
    ok_json(result)
}

//! LobsterAI 积分余额：`GET {base}/api/user/profile-summary`。
//!
//! ── 上游形态 ────────────────────────────────────────────────
//! ```text
//! GET {base}/api/user/profile-summary     header: Authorization: Bearer <accessToken>
//!   → { code, data: { totalCreditsRemaining,
//!                     creditItems: [ { type, creditsRemaining, expiresAt } ] } }
//! ```
//! 归一成 `ProviderAdapter::query_usage` 契约的形状（`{available, unit, wallets, raw}`）：
//! 每个 `creditItem` 一个钱包，`available` 取 `totalCreditsRemaining`（缺失时对钱包求和）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;

use super::{credentials, endpoints};

/// 余额查询超时（比 SSE 的默认 600 秒短得多，避免前端转圈到天荒地老）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 钱包类型的展示名（上游 `type` → 中文名；未知类型回显原值）
fn display_name(kind: &str) -> String {
    match kind {
        "daily" | "daily_points" => "每日积分".to_string(),
        "bonus" | "gift" => "赠送积分".to_string(),
        "subscription" | "plan" => "订阅积分".to_string(),
        other => other.to_string(),
    }
}

/// 从 `data` 里取一个数字（字段缺失 / 非法时给 None；兼容字符串数字）
fn pick_number(data: &Value, key: &str) -> Option<f64> {
    let value = data.get(key)?;
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value.as_str().and_then(|text| text.trim().parse::<f64>().ok())
}

/// 查询某 LobsterAI 账号的积分余额。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
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
            "LobsterAI 账号缺少 access token，无法查询积分（请重新登录）",
        ));
    }
    let mut headers = endpoints::capability_headers();
    headers.extend(endpoints::auth_headers(&cred.access_token));
    let response = send_raw(
        "GET",
        &endpoints::url(endpoints::PROFILE_SUMMARY_PATH),
        None,
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        GatewayError::with_status(502, format!("LobsterAI 积分查询请求失败：{error}"))
    })?;
    if response.status == 401 || response.status == 403 {
        return Err(GatewayError::with_status(
            401,
            "LobsterAI 登录态已失效，无法查询积分（请重新登录）",
        ));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("LobsterAI 积分查询失败（HTTP {}）", response.status),
        ));
    }
    let payload = response.payload.unwrap_or(Value::Null);
    let data = payload.get("data").cloned().unwrap_or(Value::Null);

    let wallets: Vec<Value> = data
        .get("creditItems")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let kind = item
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let balance = pick_number(item, "creditsRemaining").unwrap_or(0.0);
                    json!({
                        "type": kind,
                        "displayName": display_name(&kind),
                        "balance": balance,
                        "expiresAt": item.get("expiresAt").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let sum: f64 = wallets
        .iter()
        .filter_map(|wallet| wallet.get("balance").and_then(Value::as_f64))
        .sum();
    let available = pick_number(&data, "totalCreditsRemaining").unwrap_or(sum);

    Ok(json!({
        "available": available,
        "unit": "积分",
        "wallets": wallets,
        "raw": payload,
    }))
}

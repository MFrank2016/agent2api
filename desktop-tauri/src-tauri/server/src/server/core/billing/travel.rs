//! WorkBuddy「派猫猫旅行」（国内版运营活动）。
//!
//! 接口与计费接口**不同域**且**不带 /v2**：
//!   GET  {base}/activity/growth/buddy/travel/status   旅行状态
//!   GET  {base}/activity/growth/buddy/travel/config   可选地点
//!   POST {base}/activity/growth/buddy/travel/claim     领取到达奖励
//!   POST {base}/activity/growth/buddy/travel/depart    派发（body {"location_id": N}）
//!
//! base 按 edition 派生：cn → www.workbuddy.cn，intl → www.workbuddy.ai（实测 intl
//! 返回空 data = 无活动）。旅行只需 Bearer token，无 user-id / cookie / 设备指纹。
//! 所有失败收敛进结果字段（见 travel_auto），不打断调度。

use serde_json::{json, Value};

use super::request::{BillingCall, BillingSpec, CallOptions};
use super::{BillingError, BillingService};
use crate::server::logging;

pub const TRAVEL_CN_BASE: &str = "https://www.workbuddy.cn";
pub const TRAVEL_INTL_BASE: &str = "https://www.workbuddy.ai";
pub const TRAVEL_STATUS_PATH: &str = "/activity/growth/buddy/travel/status";
pub const TRAVEL_CONFIG_PATH: &str = "/activity/growth/buddy/travel/config";
pub const TRAVEL_CLAIM_PATH: &str = "/activity/growth/buddy/travel/claim";
pub const TRAVEL_DEPART_PATH: &str = "/activity/growth/buddy/travel/depart";

fn empty_body() -> Value { json!({}) }

const TRAVEL_STATUS_SPEC: BillingSpec = BillingSpec { method: "GET", path: TRAVEL_STATUS_PATH, body: empty_body, whitelist_headers: false };
const TRAVEL_CONFIG_SPEC: BillingSpec = BillingSpec { method: "GET", path: TRAVEL_CONFIG_PATH, body: empty_body, whitelist_headers: false };
const TRAVEL_CLAIM_SPEC: BillingSpec = BillingSpec { method: "POST", path: TRAVEL_CLAIM_PATH, body: empty_body, whitelist_headers: false };
const TRAVEL_DEPART_SPEC: BillingSpec = BillingSpec { method: "POST", path: TRAVEL_DEPART_PATH, body: empty_body, whitelist_headers: false };

/// 旅行 host（按 edition 派生）。测试可经 session.travelBaseOverride 注入 mock 地址。
fn travel_base(session: &Value) -> &'static str {
    if let Some(base) = session.get("travelBaseOverride").and_then(Value::as_str) {
        return Box::leak(base.to_string().into_boxed_str());
    }
    if session.get("edition").and_then(Value::as_str) == Some("intl") {
        TRAVEL_INTL_BASE
    } else {
        TRAVEL_CN_BASE
    }
}

#[derive(Clone, Debug)]
pub struct TravelStatus {
    pub available: bool,
    pub state: String,
    pub reward_credit: i64,
    pub daily_limit_reached: bool,
    pub location: Value,
    pub arrive_at: i64,
    pub server_now: i64,
    pub letter: Value,
    pub use_deeplink: String,
}

impl TravelStatus {
    fn unavailable() -> Self {
        Self { available: false, state: String::new(), reward_credit: 0,
               daily_limit_reached: false, location: Value::Null, arrive_at: 0,
               server_now: 0, letter: Value::Null, use_deeplink: String::new() }
    }
}

impl BillingService {
    async fn travel_call(&self, spec: BillingSpec, session: &Value) -> Result<BillingCall, BillingError> {
        self.call_billing(spec, CallOptions {
            session: Some(session),
            base_url: Some(travel_base(session)),
            expect_code_ok: false,
            ..Default::default()
        }).await
    }

    pub async fn travel_status(&self, session: &Value) -> Result<TravelStatus, BillingError> {
        let result = self.travel_call(TRAVEL_STATUS_SPEC, session).await?;
        let data = &result.data;
        let ok = result.code == Some(crate::server::core::endpoints::RESPONSE_CODE_OK);
        if !ok || !data.is_object() || data.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            return Ok(TravelStatus::unavailable());
        }
        Ok(TravelStatus {
            available: true,
            state: data.get("state").and_then(Value::as_str).unwrap_or("").to_string(),
            reward_credit: data.get("reward_credit").and_then(Value::as_i64).unwrap_or(0),
            daily_limit_reached: data.get("daily_limit_reached").and_then(Value::as_bool).unwrap_or(false),
            location: data.get("location").cloned().unwrap_or(Value::Null),
            arrive_at: data.get("arrive_at").and_then(Value::as_i64).unwrap_or(0),
            server_now: data.get("server_now").and_then(Value::as_i64).unwrap_or(0),
            letter: data.get("letter").cloned().unwrap_or(Value::Null),
            use_deeplink: data.get("use_deeplink").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    pub async fn travel_config(&self, session: &Value) -> Result<Value, BillingError> {
        Ok(self.travel_call(TRAVEL_CONFIG_SPEC, session).await?.data)
    }

    pub async fn travel_claim(&self, session: &Value) -> Result<BillingCall, BillingError> {
        self.travel_call(TRAVEL_CLAIM_SPEC, session).await
    }

    pub async fn travel_depart(&self, session: &Value, location_id: i64) -> Result<BillingCall, BillingError> {
        self.call_billing(TRAVEL_DEPART_SPEC, CallOptions {
            session: Some(session),
            base_url: Some(travel_base(session)),
            body: Some(&json!({ "location_id": location_id })),
            expect_code_ok: false,
            ..Default::default()
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::billing::BillingService;

    fn cn_session(base: &str) -> Value {
        json!({ "endpoint": "https://copilot.tencent.com", "edition": "cn",
                "auth": { "accessToken": "t" }, "account": { "uid": "u1" },
                "travelBaseOverride": base })
    }

    fn service(label: &str) -> (BillingService, crate::server::db::test_temp::TempDb) {
        use crate::server::core::account_store::AccountStore;
        let (db, guard) = crate::server::db::test_temp::TempDb::open(label);
        let store = AccountStore::with_db(Some(db));
        (BillingService::new(crate::server::core::auth::AuthService::for_store(store)), guard)
    }

    async fn mock(status_json: &'static str, config_json: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .route(TRAVEL_STATUS_PATH, axum::routing::get(move || async move {
                (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], status_json) }))
            .route(TRAVEL_CONFIG_PATH, axum::routing::get(move || async move {
                (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], config_json) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, task)
    }

    #[tokio::test]
    async fn status_parses_arrived_fields() {
        let (billing, _g) = service("travel-status-arrived");
        let (base, _t) = mock(
            r#"{"code":0,"msg":"OK","data":{"state":"arrived","reward_credit":8,"daily_limit_reached":true,"location":{"id":4,"name":"古镇客栈"},"arrive_at":100,"server_now":50}}"#,
            r#"{"code":0,"data":{"locations":[]}}"#).await;
        let status = billing.travel_status(&cn_session(&base)).await.unwrap();
        assert!(status.available);
        assert_eq!(status.state, "arrived");
        assert_eq!(status.reward_credit, 8);
        assert!(status.daily_limit_reached);
    }

    #[tokio::test]
    async fn empty_data_is_unavailable() {
        let (billing, _g) = service("travel-status-empty");
        let (base, _t) = mock(r#"{"code":0,"data":{}}"#, r#"{"code":0,"data":{}}"#).await;
        let status = billing.travel_status(&cn_session(&base)).await.unwrap();
        assert!(!status.available, "空 data（intl 无活动）必须判为 unavailable");
    }
}

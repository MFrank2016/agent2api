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

#[derive(Clone, Copy, Debug, Default)]
pub struct TravelOpts {
    pub location_id: Option<i64>,
}

fn travel_result(available: bool, state: &str, claimed: bool, reward_credit: i64,
                 daily_limit_reached: bool, location: Value, message: &str) -> Value {
    json!({
        "available": available, "state": state, "claimed": claimed,
        "rewardCredit": reward_credit, "dailyLimitReached": daily_limit_reached,
        "location": location, "message": message,
    })
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

    fn pick_location(config: &Value, fixed: Option<i64>) -> Option<i64> {
        let ids: Vec<i64> = config.get("locations").and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|i| i.get("id").and_then(Value::as_i64)).collect())
            .unwrap_or_default();
        if let Some(id) = fixed { if ids.contains(&id) { return Some(id); } }
        if ids.is_empty() { return None; }
        let mut buf = [0u8; 8];
        let _ = getrandom::getrandom(&mut buf);
        Some(ids[(u64::from_le_bytes(buf) % ids.len() as u64) as usize])
    }

    /// 状态机（先领后派；daily_limit_reached 在 state 之前判）。所有失败收敛，不返回 Err。
    pub async fn travel_auto(&self, session: &Value, opts: TravelOpts) -> Value {
        let mut status = match self.travel_status(session).await {
            Ok(s) if s.available => s,
            Ok(_) => return travel_result(false, "", false, 0, false, Value::Null, "旅行活动未开放"),
            Err(error) => {
                logging::verbose("[Travel]", &format!("旅行状态查询失败: {}", error.message));
                return travel_result(false, "", false, 0, false, Value::Null, "查询旅行状态失败，已跳过全部写操作");
            }
        };
        let mut claimed = false;
        let mut reward = 0i64;
        let mut message = String::new();

        if status.state == "arrived" {
            match self.travel_claim(session).await {
                Ok(call) => {
                    claimed = true;
                    reward = call.data.get("reward_credit").and_then(Value::as_i64).unwrap_or(0);
                    message = format!("已领取旅行积分 +{reward}");
                    if let Ok(fresh) = self.travel_status(session).await { if fresh.available { status = fresh; } }
                }
                Err(error) => {
                    let msg = format!("行程已到达但领取失败: {}", error.message);
                    logging::verbose("[Travel]", &msg);
                    return travel_result(true, &status.state, false, 0, status.daily_limit_reached, status.location.clone(), &msg);
                }
            }
        }

        if status.daily_limit_reached {
            if message.is_empty() { message = "今日派遣次数已用完，跳过派遣".to_string(); }
        } else if status.state.is_empty() || status.state == "idle" {
            let config = self.travel_config(session).await.unwrap_or(Value::Null);
            match Self::pick_location(&config, opts.location_id) {
                Some(location_id) => match self.travel_depart(session, location_id).await {
                    Ok(_) => {
                        let extra = "已派出 Buddy 旅行".to_string();
                        message = if message.is_empty() { extra } else { format!("{message}；{extra}") };
                        if let Ok(fresh) = self.travel_status(session).await { if fresh.available { status = fresh; } }
                    }
                    Err(error) => {
                        let extra = format!("派发失败: {}", error.message);
                        message = if message.is_empty() { extra } else { format!("{message}；{extra}") };
                    }
                },
                None => {
                    let extra = "未能获取可选地点列表，已跳过派遣".to_string();
                    message = if message.is_empty() { extra } else { format!("{message}；{extra}") };
                }
            }
        } else {
            let extra = "Buddy 正在旅行中，无需派遣".to_string();
            message = if message.is_empty() { extra } else { format!("{message}；{extra}") };
        }

        travel_result(true, &status.state, claimed, reward, status.daily_limit_reached, status.location.clone(), &message)
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

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn mock_script(status_sequence: Vec<&'static str>, config_json: &'static str)
        -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let seq = Arc::new(std::sync::Mutex::new(status_sequence));
        let claims = Arc::new(AtomicUsize::new(0));
        let departs = Arc::new(AtomicUsize::new(0));
        let c = claims.clone();
        let d = departs.clone();
        let app = axum::Router::new()
            .route(TRAVEL_STATUS_PATH, axum::routing::get(move || {
                let seq = seq.clone();
                async move {
                    let body = { let mut g = seq.lock().unwrap();
                        if g.len() > 1 { g.remove(0) } else { g[0] } };
                    (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], body)
                }
            }))
            .route(TRAVEL_CONFIG_PATH, axum::routing::get(move || async move {
                (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], config_json) }))
            .route(TRAVEL_CLAIM_PATH, axum::routing::post(move || { let c = c.clone(); async move {
                c.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
                 r#"{"code":0,"data":{"state":"idle","reward_credit":8}}"#) } }))
            .route(TRAVEL_DEPART_PATH, axum::routing::post(move || { let d = d.clone(); async move {
                d.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
                 r#"{"code":0,"data":{"location":{"id":1}}}"#) } }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, claims, departs, task)
    }

    #[tokio::test]
    async fn arrived_is_claimed_then_no_depart_when_limited() {
        let (billing, _g) = service("travel-auto-arrived");
        let (base, claims, departs, _t) = mock_script(vec![
            r#"{"code":0,"data":{"state":"arrived","reward_credit":8,"daily_limit_reached":true,"location":{"id":4}}}"#,
            r#"{"code":0,"data":{"state":"idle","daily_limit_reached":true}}"#,
        ], r#"{"code":0,"data":{"locations":[{"id":1}]}}"#).await;
        let out = billing.travel_auto(&cn_session(&base), TravelOpts::default()).await;
        assert_eq!(out.get("claimed").and_then(Value::as_bool), Some(true));
        assert_eq!(out.get("rewardCredit").and_then(Value::as_i64), Some(8));
        assert_eq!(claims.load(Ordering::SeqCst), 1);
        assert_eq!(departs.load(Ordering::SeqCst), 0, "已达上限不得派发");
    }

    #[tokio::test]
    async fn idle_departs() {
        let (billing, _g) = service("travel-auto-idle");
        let (base, claims, departs, _t) = mock_script(
            vec![r#"{"code":0,"data":{"state":"idle","daily_limit_reached":false}}"#],
            r#"{"code":0,"data":{"locations":[{"id":1},{"id":2}]}}"#).await;
        let out = billing.travel_auto(&cn_session(&base), TravelOpts::default()).await;
        assert_eq!(out.get("claimed").and_then(Value::as_bool), Some(false));
        assert_eq!(claims.load(Ordering::SeqCst), 0);
        assert_eq!(departs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn traveling_is_skipped() {
        let (billing, _g) = service("travel-auto-traveling");
        let (base, claims, departs, _t) = mock_script(
            vec![r#"{"code":0,"data":{"state":"traveling","arrive_at":100,"server_now":50,"daily_limit_reached":false}}"#],
            r#"{"code":0,"data":{"locations":[{"id":1}]}}"#).await;
        let out = billing.travel_auto(&cn_session(&base), TravelOpts::default()).await;
        assert_eq!(claims.load(Ordering::SeqCst), 0);
        assert_eq!(departs.load(Ordering::SeqCst), 0);
        assert_eq!(out.get("state").and_then(Value::as_str), Some("traveling"));
    }

    #[tokio::test]
    async fn unavailable_status_does_no_writes() {
        let (billing, _g) = service("travel-auto-unavailable");
        let (base, claims, departs, _t) = mock_script(
            vec![r#"{"code":0,"data":{}}"#], r#"{"code":0,"data":{"locations":[]}}"#).await;
        let out = billing.travel_auto(&cn_session(&base), TravelOpts::default()).await;
        assert_eq!(out.get("available").and_then(Value::as_bool), Some(false));
        assert_eq!(claims.load(Ordering::SeqCst), 0);
        assert_eq!(departs.load(Ordering::SeqCst), 0);
    }
}

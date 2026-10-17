# WorkBuddy「派猫猫旅行」接入 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 agent2api 在每日自动签到里附带执行 WorkBuddy 国内版的「派猫猫旅行」（派发 → 到达 → 领取积分），并在签到中心展示状态与手动按钮。

**Architecture:** 新增 `core/billing/travel.rs`（4 个端点 + 状态机），复用 `call_billing` 并给它加一个 `CallOptions.base_url` 覆盖项（旅行接口与计费接口不同域）。调度挂在现有 `core/auto_checkin.rs`（每日两轮：主轮派发、`claimTime` 轮领取），配置存 `config.json` 的 `autoCheckin.travel`。手动动作与卡片状态走新的 `api::travel` 两个端点（`GET`/`POST /api/accounts/{id}/travel`）。

**Tech Stack:** Rust（axum 内嵌服务 + reqwest）、serde_json、tokio；前端原生 TS/React islands；测试用 `#[cfg(test)]` 内联手写 axum mock（不引入 wiremock）+ `crate::server::db::test_temp::TempDb`。

**Spec:** `docs/superpowers/specs/2026-10-13-workbuddy-buddy-travel-design.md`

## Global Constraints

- 仅支持 WorkBuddy **国内版**（`provider == "workbuddy"`，`edition == "cn"`）；国际版一律 no-op（不报错、不展示卡片）。
- 旅行 host 按 edition 派生：`cn → https://www.workbuddy.cn`，`intl → https://www.workbuddy.ai`；**绝不硬编码**单一 host。
- 旅行路径**不带 `/v2`**：`/activity/growth/buddy/travel/{status,config,claim,depart}`。
- 旅行请求头仅 `Authorization: Bearer <accessToken>`（POST 另加 `Content-Type: application/json`）；无 user-id / cookie / 设备指纹。
- `autoCheckin.travel.enabled` 默认 `false`；缺 `travel` 键 = 关闭。升级不得让老配置突然多打请求。
- 现有计费调用一律不传 `base_url`（`None`），行为逐字不变。
- 所有上游失败收敛进结果字段，绝不打断调度（对齐 `billing/activity.rs` 的纪律）。
- 「自然日」一律北京时间（`core::beijing`）。
- 不新增 `scheduledTasks` 条目；旅行配置与状态都在 `autoCheckin` 下。
- 提交信息用仓库既有中文风格（`新增：` / `修复：` / `优化：` 前缀可选）。

## Review Focus

- **`call_billing` 的 host 覆盖不能污染既有调用**：任何一条现有计费调用（余额/签到/企业额度/banner/大使）的 URL 必须逐字不变。测试：Task 1 的回归用例断言不传 `base_url` 时仍打 `session.endpoint`。
- **两轮调度的去重与并发**：同一天重复 tick 不得重复派发/领取；`fire` 与 `fire_travel` 不得并发跑同一账号。测试：Task 4/5 的去重用例 + 复用 `running` 守卫。
- **`travel_auto` 状态机对「已达上限」的顺序**：`daily_limit_reached` 必须在判断 state **之前**检查（达上限时连 idle 也不派发），但**不**阻止领取已到达的行程。测试：Task 3。
- **intl 账号零副作用**：国际版账号不产生任何写请求、不报错。测试：Task 2（空 `data` → `available=false`）。
- **`travel` 配置对象的合并写回**：更新 `lastDepartDate` 时不得丢掉 `enabled`/`claimTime`/`locationId`/`lastClaimDate`。测试：Task 4。

## Deviations from Spec

1. **旅行不挂在 `checkin_for` 的结果行上**（spec §5.3）。原因：`billing` 层不能依赖 `auto_checkin` 的配置（反向依赖），且卡片状态已由 `GET /api/accounts/{id}/travel` 惰性获取（spec §5.5），行内再挂一份冗余。旅行改为 `auto_checkin` 里的独立轮次；`checkin.rs` **完全不改**。
2. **`lastDepartDate` 的去重绑在主轮触发时刻**（与 `autoCheckin.time` 同时刻），不另设 `departTime` 配置项 —— 与 spec §6 配置表一致。

---

### Task 1: `CallOptions.base_url` + `call_billing` host 覆盖

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/billing/request.rs`（`CallOptions` + `Default`）
- Modify: `desktop-tauri/src-tauri/server/src/server/core/billing/mod.rs`（`call_billing` endpoint 解析；末尾新增 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: 无
- Produces: `CallOptions { …, base_url: Option<&'a str> }`；`call_billing` 在 `base_url` 为 `Some` 时用它（经 `normalize_endpoint`）取代 `session.endpoint`。

- [ ] **Step 1: 给 `CallOptions` 加字段**

`request.rs` 的 `CallOptions<'a>` 在 `locale: Option<&'a str>,` 之后加：

```rust
    /// 本次调用的 host 覆盖（只含 scheme+host，不含路径）。None = 用
    /// `session.endpoint`（现状，所有既有调用都是 None）。
    /// 只有「派猫猫旅行」用得到（它在另一个域名），见 billing::travel。
    pub(super) base_url: Option<&'a str>,
```

`impl Default for CallOptions<'_>` 的 `Self { … }` 里补 `base_url: None,`。

- [ ] **Step 2: 写失败测试**

`billing/mod.rs` 末尾新增：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn mock_get() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let app = axum::Router::new().route(
            "/probe",
            axum::routing::get(move || {
                count.fetch_add(1, Ordering::SeqCst);
                async move {
                    (axum::http::StatusCode::OK,
                     [(axum::http::header::CONTENT_TYPE, "application/json")],
                     "{\"code\":0,\"data\":{\"ok\":true}}")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, hits, task)
    }

    fn empty_body() -> Value { json!({}) }

    fn probe_spec() -> request::BillingSpec {
        request::BillingSpec { method: "GET", path: "/probe", body: empty_body, whitelist_headers: false }
    }

    fn service(label: &str) -> (BillingService, crate::server::db::test_temp::TempDb) {
        use crate::server::core::account_store::AccountStore;
        let (db, guard) = crate::server::db::test_temp::TempDb::open(label);
        let store = AccountStore::with_db(Some(db));
        (BillingService::new(crate::server::core::auth::AuthService::for_store(store)), guard)
    }

    fn session(endpoint: &str) -> Value {
        json!({ "endpoint": endpoint, "edition": "cn",
                "auth": { "accessToken": "t" }, "account": { "uid": "u1" } })
    }

    #[tokio::test]
    async fn base_url_override_retargets_the_call() {
        let (billing, _guard) = service("billing-base-url-override");
        let (base, hits, _task) = mock_get().await;
        let session = session("http://127.0.0.1:1");
        billing.call_billing(probe_spec(), request::CallOptions {
            session: Some(&session), base_url: Some(&base),
            expect_code_ok: true, ..Default::default()
        }).await.expect("base_url 覆盖后应打到 mock");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn without_base_url_the_session_endpoint_is_used() {
        let (billing, _guard) = service("billing-base-url-none");
        let (base, hits, _task) = mock_get().await;
        let session = session(&base);
        billing.call_billing(probe_spec(), request::CallOptions {
            session: Some(&session), expect_code_ok: true, ..Default::default()
        }).await.expect("不传 base_url 时用 session.endpoint");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cd desktop-tauri/src-tauri/server && cargo test base_url 2>&1 | tail -30`
Expected: 编译失败（无 `base_url` 字段）。

- [ ] **Step 4: 改 `call_billing` 的 endpoint 解析**

把 `billing/mod.rs::call_billing` 里 `let endpoint = session … unwrap_or_else(...)` 整段替换为：

```rust
        let endpoint = match options.base_url {
            Some(base) => normalize_endpoint(base),
            None => session
                .get("endpoint")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(normalize_endpoint)
                .unwrap_or_else(|| {
                    normalize_endpoint(self.auth.default_context().base_url.as_str())
                }),
        };
```

- [ ] **Step 5: 运行确认通过（含回归）**

Run: `cd desktop-tauri/src-tauri/server && cargo test 2>&1 | tail -40`
Expected: 新用例 PASS，既有全绿。

- [ ] **Step 6: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/core/billing/request.rs desktop-tauri/src-tauri/server/src/server/core/billing/mod.rs
git commit -m "新增：计费调用支持 base_url 覆盖（为派猫猫旅行跨域接口铺路）"
```

---

### Task 2: `core/billing/travel.rs` —— 端点 + `TravelStatus`

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/core/billing/travel.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/billing/mod.rs`（加 `mod travel;`）
- Test: `travel.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `super::{BillingService, BillingError}`、`super::request::{BillingSpec, CallOptions, BillingCall}`、私有 `self.call_billing`（子模块可见）。
- Produces:
  - 常量 `TRAVEL_CN_BASE`/`TRAVEL_INTL_BASE`/`TRAVEL_STATUS_PATH`/`TRAVEL_CONFIG_PATH`/`TRAVEL_CLAIM_PATH`/`TRAVEL_DEPART_PATH`
  - `struct TravelStatus { available, state, reward_credit, daily_limit_reached, location, arrive_at, server_now, letter, use_deeplink }`
  - `impl BillingService`: `travel_status` / `travel_config` / `travel_claim` / `travel_depart`

- [ ] **Step 1: 建 `travel.rs`（含实现与测试）**

```rust
//! WorkBuddy「派猫猫旅行」（国内版运营活动）。
//!
//! 接口与计费接口**不同域**且**不带 /v2**：
//!   GET  {base}/activity/growth/buddy/travel/status   旅行状态
//!   GET  {base}/activity/growth/buddy/travel/config   可选地点
//!   POST {base}/activity/growth/buddy/travel/claim     领取到达奖励
//!   POST {base}/activity/growth/buddy/travel/depart    派发（body {\"location_id\": N}）
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
            r#"{\"code\":0,\"msg\":\"OK\",\"data\":{\"state\":\"arrived\",\"reward_credit\":8,\"daily_limit_reached\":true,\"location\":{\"id\":4,\"name\":\"古镇客栈\"},\"arrive_at\":100,\"server_now\":50}}"#,
            r#"{\"code\":0,\"data\":{\"locations\":[]}}"#).await;
        let status = billing.travel_status(&cn_session(&base)).await.unwrap();
        assert!(status.available);
        assert_eq!(status.state, "arrived");
        assert_eq!(status.reward_credit, 8);
        assert!(status.daily_limit_reached);
    }

    #[tokio::test]
    async fn empty_data_is_unavailable() {
        let (billing, _g) = service("travel-status-empty");
        let (base, _t) = mock(r#"{\"code\":0,\"data\":{}}"#, r#"{\"code\":0,\"data\":{}}"#).await;
        let status = billing.travel_status(&cn_session(&base)).await.unwrap();
        assert!(!status.available, "空 data（intl 无活动）必须判为 unavailable");
    }
}
```

- [ ] **Step 2: 注册子模块 + 运行测试**

`billing/mod.rs` 的 `mod request;` 旁加 `mod travel;`（不 pub）。
Run: `cd desktop-tauri/src-tauri/server && cargo test travel 2>&1 | tail -30`
Expected: PASS。

- [ ] **Step 3: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/core/billing/travel.rs desktop-tauri/src-tauri/server/src/server/core/billing/mod.rs
git commit -m "新增：WorkBuddy 派猫猫旅行端点封装（status/config/claim/depart）"
```

---

### Task 3: `travel_auto` 状态机

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/billing/travel.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 2 的 `travel_status` / `travel_config` / `travel_claim` / `travel_depart`。
- Produces:
  - `#[derive(Clone, Copy, Debug, Default)] pub struct TravelOpts { pub location_id: Option<i64> }`
  - `impl BillingService { pub async fn travel_auto(&self, session: &Value, opts: TravelOpts) -> Value }`
    返回 `{ available, state, claimed, rewardCredit, dailyLimitReached, location, message }`
  - 模块私有 `fn travel_result(...) -> Value`、`fn pick_location(config: &Value, fixed: Option<i64>) -> Option<i64>`

- [ ] **Step 1: 写失败测试（四态）**

在 `travel.rs` 的 `mod tests` 内追加（依赖带脚本的 mock）：

```rust
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
                 r#"{\"code\":0,\"data\":{\"state\":\"idle\",\"reward_credit\":8}}"#) } }))
            .route(TRAVEL_DEPART_PATH, axum::routing::post(move || { let d = d.clone(); async move {
                d.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
                 r#"{\"code\":0,\"data\":{\"location\":{\"id\":1}}}"#) } }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, claims, departs, task)
    }

    #[tokio::test]
    async fn arrived_is_claimed_then_no_depart_when_limited() {
        let (billing, _g) = service("travel-auto-arrived");
        let (base, claims, departs, _t) = mock_script(vec![
            r#"{\"code\":0,\"data\":{\"state\":\"arrived\",\"reward_credit\":8,\"daily_limit_reached\":true,\"location\":{\"id\":4}}}"#,
            r#"{\"code\":0,\"data\":{\"state\":\"idle\",\"daily_limit_reached\":true}}"#,
        ], r#"{\"code\":0,\"data\":{\"locations\":[{\"id\":1}]}}"#).await;
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
            vec![r#"{\"code\":0,\"data\":{\"state\":\"idle\",\"daily_limit_reached\":false}}"#],
            r#"{\"code\":0,\"data\":{\"locations\":[{\"id\":1},{\"id\":2}]}}"#).await;
        let out = billing.travel_auto(&cn_session(&base), TravelOpts::default()).await;
        assert_eq!(out.get("claimed").and_then(Value::as_bool), Some(false));
        assert_eq!(claims.load(Ordering::SeqCst), 0);
        assert_eq!(departs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn traveling_is_skipped() {
        let (billing, _g) = service("travel-auto-traveling");
        let (base, claims, departs, _t) = mock_script(
            vec![r#"{\"code\":0,\"data\":{\"state\":\"traveling\",\"arrive_at\":100,\"server_now\":50,\"daily_limit_reached\":false}}"#],
            r#"{\"code\":0,\"data\":{\"locations\":[{\"id\":1}]}}"#).await;
        let out = billing.travel_auto(&cn_session(&base), TravelOpts::default()).await;
        assert_eq!(claims.load(Ordering::SeqCst), 0);
        assert_eq!(departs.load(Ordering::SeqCst), 0);
        assert_eq!(out.get("state").and_then(Value::as_str), Some("traveling"));
    }

    #[tokio::test]
    async fn unavailable_status_does_no_writes() {
        let (billing, _g) = service("travel-auto-unavailable");
        let (base, claims, departs, _t) = mock_script(
            vec![r#"{\"code\":0,\"data\":{}}"#], r#"{\"code\":0,\"data\":{\"locations\":[]}}"#).await;
        let out = billing.travel_auto(&cn_session(&base), TravelOpts::default()).await;
        assert_eq!(out.get("available").and_then(Value::as_bool), Some(false));
        assert_eq!(claims.load(Ordering::SeqCst), 0);
        assert_eq!(departs.load(Ordering::SeqCst), 0);
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cd desktop-tauri/src-tauri/server && cargo test travel_auto 2>&1 | tail -30`
Expected: 编译失败（`travel_auto` / `TravelOpts` 不存在）。

- [ ] **Step 3: 实现**

在 `travel.rs` 的 `impl BillingService` 块内（`travel_depart` 之后）追加：

```rust
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
```

在 `impl` 块外（文件级）加：

```rust
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
```

- [ ] **Step 4: 运行确认通过**

Run: `cd desktop-tauri/src-tauri/server && cargo test travel 2>&1 | tail -40`
Expected: Task 2 + Task 3 全部 PASS。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/core/billing/travel.rs
git commit -m "新增：派猫猫旅行状态机 travel_auto（先领后派 + 上限门控）"
```

---

### Task 4: `autoCheckin.travel` 配置读写

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/auto_checkin.rs`
- Test: 同文件 `#[cfg(test)] mod tests`（若无则新增）

**Interfaces:**
- Consumes: `normalize_time`、`raw_object`、`read_state`、`write_state`。
- Produces:
  - `#[derive(Clone, Debug, Default)] pub struct TravelState { pub enabled: bool, pub claim_time: String, pub location_id: Option<i64>, pub last_depart_date: Option<String>, pub last_claim_date: Option<String> }`
  - `pub const DEFAULT_TRAVEL_CLAIM_TIME: &str = "06:00";`
  - `fn read_travel_state(raw: &Map<String, Value>) -> TravelState`
  - `fn write_travel_state(patch: Value)`（合并进 `autoCheckin.travel`）
  - `CheckinState` 新增 `travel: TravelState`；`state()` 返回值新增 `"travel"` 键。

- [ ] **Step 1: 写失败测试**

在 `auto_checkin.rs` 末尾新增（若已存在 `mod tests` 则并入）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn travel_defaults_to_disabled() {
        let travel = read_travel_state(&Map::new());
        assert!(!travel.enabled);
        assert_eq!(travel.claim_time, DEFAULT_TRAVEL_CLAIM_TIME);
        assert_eq!(travel.location_id, None);
    }

    #[test]
    fn travel_reads_and_normalizes_claim_time() {
        let mut raw = Map::new();
        raw.insert("travel".into(), json!({
            "enabled": true, "claimTime": "6:5", "locationId": 2,
            "lastDepartDate": "2026-10-12", "lastClaimDate": "2026-10-12"
        }));
        let travel = read_travel_state(&raw);
        assert!(travel.enabled);
        assert_eq!(travel.claim_time, "06:05", "HH:MM 应补零归一");
        assert_eq!(travel.location_id, Some(2));
        assert_eq!(travel.last_depart_date.as_deref(), Some("2026-10-12"));
    }

    #[test]
    fn travel_invalid_claim_time_falls_back() {
        let mut raw = Map::new();
        raw.insert("travel".into(), json!({ "enabled": true, "claimTime": "99:99" }));
        assert_eq!(read_travel_state(&raw).claim_time, DEFAULT_TRAVEL_CLAIM_TIME);
    }
}
```

> 注：`write_travel_state` 的合并语义在 Task 5 的去重用例里验证（它需要 `config` 全局快照，单测成本高；由 Task 5 的集成路径覆盖）。若你希望单独可测，把合并逻辑抽成纯函数 `fn merge_travel(raw: &Map<String, Value>, patch: &Value) -> Value` 并单测。

- [ ] **Step 2: 运行确认失败**

Run: `cd desktop-tauri/src-tauri/server && cargo test travel_ 2>&1 | tail -30`
Expected: 编译失败（`read_travel_state` / `DEFAULT_TRAVEL_CLAIM_TIME` 不存在）。

- [ ] **Step 3: 实现**

在 `auto_checkin.rs`（`CheckinState` 定义附近）新增：

```rust
/// 旅行配置的缺省领取时刻（北京时间）：主轮 00:01 派发后，6 小时后领取。
pub const DEFAULT_TRAVEL_CLAIM_TIME: &str = "06:00";

/// `autoCheckin.travel` 子对象（缺失 = 关闭，向后兼容）。
#[derive(Clone, Debug, Default)]
pub struct TravelState {
    pub enabled: bool,
    pub claim_time: String,
    pub location_id: Option<i64>,
    pub last_depart_date: Option<String>,
    pub last_claim_date: Option<String>,
}

/// 从 `autoCheckin` 原始对象里读 travel 子对象。
fn read_travel_state(raw: &Map<String, Value>) -> TravelState {
    let Some(travel) = raw.get("travel").and_then(Value::as_object) else {
        return TravelState {
            enabled: false,
            claim_time: DEFAULT_TRAVEL_CLAIM_TIME.to_string(),
            location_id: None,
            last_depart_date: None,
            last_claim_date: None,
        };
    };
    let claim_time = travel
        .get("claimTime")
        .and_then(|value| normalize_time(Some(value)).ok())
        .unwrap_or_else(|| DEFAULT_TRAVEL_CLAIM_TIME.to_string());
    TravelState {
        enabled: travel.get("enabled").and_then(Value::as_bool) == Some(true),
        claim_time,
        location_id: travel.get("locationId").and_then(Value::as_i64),
        last_depart_date: travel.get("lastDepartDate").and_then(Value::as_str).map(str::to_string),
        last_claim_date: travel.get("lastClaimDate").and_then(Value::as_str).map(str::to_string),
    }
}

/// 合并写回 `autoCheckin.travel`（不丢其它键，对应 spec 的 Review Focus 第 5 条）。
fn write_travel_state(patch: Value) {
    let mut raw = raw_object();
    let mut travel = raw
        .get("travel")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Value::Object(entries) = patch {
        for (key, value) in entries {
            travel.insert(key, value);
        }
    }
    raw.insert("travel".to_string(), Value::Object(travel));
    config::update_raw_field(CONFIG_KEY, Value::Object(raw));
}
```

把 `CheckinState` 加字段 `pub travel: TravelState,`，并在 `read_state()` 的构造里加 `travel: read_travel_state(&raw),`。

在 `state()` 返回的 `json!({ … })` 里加：

```rust
            "travel": {
                "enabled": state.travel.enabled,
                "claimTime": state.travel.claim_time,
                "locationId": state.travel.location_id,
                "lastDepartDate": state.travel.last_depart_date,
                "lastClaimDate": state.travel.last_claim_date,
            },
```

- [ ] **Step 4: 运行确认通过**

Run: `cd desktop-tauri/src-tauri/server && cargo test travel_ 2>&1 | tail -30`
Expected: 三条 PASS。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/core/auto_checkin.rs
git commit -m "新增：autoCheckin.travel 配置读写（默认关闭，向后兼容）"
```

---

### Task 5: 每日两轮调度 + `fire_travel`

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/auto_checkin.rs`（`tick`、`fire_travel`、`configure`）
- Modify: `desktop-tauri/src-tauri/server/src/server/core/billing/mod.rs`（`pub use travel::TravelOpts;`）
- Test: `auto_checkin.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 4 的 `TravelState`/`write_travel_state`/`read_state`；Task 3 的 `BillingService::travel_auto` + `TravelOpts`。
- Produces:
  - `enum TravelPhase { Depart, Claim }`（模块私有）
  - `async fn fire_travel(&self, phase: TravelPhase) -> Option<Value>`
  - `tick` 改为三轮各自去重（主轮签到 / 主轮旅行派发 / `claimTime` 旅行领取）
  - `configure` 接受 `{ travel: { enabled, claimTime, locationId } }`
  - 纯函数 `fn travel_patch_from_payload(value: &Value) -> Result<Value, AutoCheckinConfigError>`

- [ ] **Step 1: 写失败测试（`travel_patch_from_payload`）**

在 `auto_checkin.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn travel_patch_accepts_valid_body() {
        let patch = travel_patch_from_payload(&json!({
            "enabled": true, "claimTime": "6:5", "locationId": 3
        })).unwrap();
        assert_eq!(patch.get("enabled").and_then(Value::as_bool), Some(true));
        assert_eq!(patch.get("claimTime").and_then(Value::as_str), Some("06:05"));
        assert_eq!(patch.get("locationId").and_then(Value::as_i64), Some(3));
    }

    #[test]
    fn travel_patch_rejects_bad_location() {
        assert!(travel_patch_from_payload(&json!({ "locationId": "x" })).is_err());
    }

    #[test]
    fn travel_patch_null_location_clears_it() {
        let patch = travel_patch_from_payload(&json!({ "locationId": null })).unwrap();
        assert!(patch.get("locationId").unwrap().is_null());
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cd desktop-tauri/src-tauri/server && cargo test travel_patch 2>&1 | tail -20`
Expected: 编译失败（`travel_patch_from_payload` 不存在）。

- [ ] **Step 3: 实现 `travel_patch_from_payload` + `configure` 接线**

在 `auto_checkin.rs` 新增：

```rust
/// 把 POST /api/auto-checkin 里的 `travel` 子对象归一成可合并的 patch。
fn travel_patch_from_payload(value: &Value) -> Result<Value, AutoCheckinConfigError> {
    let Some(object) = value.as_object() else {
        return Err(AutoCheckinConfigError::new("travel 必须是对象"));
    };
    let mut patch = Map::new();
    if let Some(enabled) = object.get("enabled") {
        patch.insert("enabled".to_string(), Value::Bool(enabled == &Value::Bool(true)));
    }
    if let Some(claim_time) = object.get("claimTime") {
        patch.insert("claimTime".to_string(), Value::String(normalize_time(Some(claim_time))?));
    }
    if let Some(location) = object.get("locationId") {
        if location.is_null() {
            patch.insert("locationId".to_string(), Value::Null);
        } else if let Some(id) = location.as_i64() {
            patch.insert("locationId".to_string(), Value::Number(id.into()));
        } else {
            return Err(AutoCheckinConfigError::new("locationId 必须是数字或 null"));
        }
    }
    if patch.is_empty() {
        return Err(AutoCheckinConfigError::new("travel 没有需要更新的字段"));
    }
    Ok(Value::Object(patch))
}
```

在 `configure` 末尾（`if patch.is_empty() { return Err(...) }` 之前）插入 travel 处理：

```rust
        let mut travel_patch: Option<Value> = None;
        if let Some(value) = payload.get("travel") {
            travel_patch = Some(travel_patch_from_payload(value)?);
        }
        if patch.is_empty() && travel_patch.is_none() {
            return Err(AutoCheckinConfigError::new("没有需要更新的字段"));
        }
        // （把原有的 `if patch.is_empty() { return Err(...) }` 删掉，用上面这句代替）
```

并在 `write_state(Value::Object(patch));` 之后加：

```rust
        if let Some(travel) = travel_patch {
            write_travel_state(travel);
            logging::log("[Checkin]", "旅行设置已更新");
        }
```

- [ ] **Step 4: 实现 `fire_travel` + 重构 `tick`**

在 `auto_checkin.rs` 加：

```rust
/// 旅行轮次的阶段（与去重键一一对应）。
#[derive(Clone, Copy)]
enum TravelPhase {
    Depart,
    Claim,
}

impl AutoCheckin {
    /// 跑一轮旅行（对所有启用的 CN WorkBuddy 账号）。与 `fire` 共用 `running` 守卫。
    async fn fire_travel(&self, phase: TravelPhase) -> Option<Value> {
        let guard = {
            let mut inner = self.lock();
            if inner.running {
                return None;
            }
            inner.running = true;
            RunningGuard { inner: self.inner.clone() }
        };
        let today = today_key();
        match phase {
            TravelPhase::Depart => write_travel_state(json!({ "lastDepartDate": today })),
            TravelPhase::Claim => write_travel_state(json!({ "lastClaimDate": today })),
        }
        let state = read_state();
        if !state.travel.enabled {
            drop(guard);
            return None;
        }
        let opts = crate::server::core::billing::TravelOpts { location_id: state.travel.location_id };
        let accounts = self
            .store
            .list_accounts()
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut results = Vec::new();
        for account in &accounts {
            let provider = account.get("provider").and_then(Value::as_str).unwrap_or("");
            let edition = account.get("edition").and_then(Value::as_str).unwrap_or("");
            let enabled = account.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            if provider != "workbuddy" || edition != "cn" || !enabled {
                continue;
            }
            let Some(id) = account.get("id").and_then(Value::as_str) else { continue; };
            let Some(entry) = self.store.get_session_by_id(id) else { continue; };
            let result = self.billing.travel_auto(&entry.session, opts).await;
            logging::log(
                "[Travel]",
                &format!("账号 {id}: {}", result.get("message").and_then(Value::as_str).unwrap_or("")),
            );
            results.push(json!({ "id": id, "travel": result }));
        }
        drop(guard);
        Some(json!({
            "phase": match phase { TravelPhase::Depart => "depart", TravelPhase::Claim => "claim" },
            "results": results,
        }))
    }
}
```

把 `tick` 方法体替换为（**去掉原来的全局早退**，改为每轮各自判定）：

```rust
    pub async fn tick(&self) {
        let state = read_state();
        if !state.enabled {
            return;
        }
        let now = beijing::now();
        let today = date_key(now);

        // 主轮次：签到
        if due_now(&state.time, now).is_some()
            && state.last_fired_date.as_deref() != Some(today.as_str())
        {
            match self.fire("到点触发").await {
                Some(result) => {
                    if result.get("qoderIntlPending").and_then(Value::as_bool) == Some(true) {
                        write_state(json!({ "lastFiredDate": Value::Null }));
                        logging::log(
                            "[Checkin]",
                            "⏳ Qoder 国际版的活动窗口未开（官方每日 10:00 UTC+8），今天会自动重试到领到为止",
                        );
                    }
                }
                None => {}
            }
        }

        // 旅行轮次（独立去重，不受主轮 lastFiredDate 影响）
        if state.travel.enabled {
            if due_now(&state.time, now).is_some()
                && state.travel.last_depart_date.as_deref() != Some(today.as_str())
            {
                self.fire_travel(TravelPhase::Depart).await;
            }
            if due_now(&state.travel.claim_time, now).is_some()
                && state.travel.last_claim_date.as_deref() != Some(today.as_str())
            {
                self.fire_travel(TravelPhase::Claim).await;
            }
        }
    }
```

> `billing/mod.rs` 加 `pub use travel::TravelOpts;`（`travel` 仍为私有 `mod`）。

- [ ] **Step 5: 运行确认通过**

Run: `cd desktop-tauri/src-tauri/server && cargo test 2>&1 | tail -40`
Expected: `travel_patch` 三条 PASS，既有全绿。

- [ ] **Step 6: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/core/auto_checkin.rs desktop-tauri/src-tauri/server/src/server/core/billing/mod.rs
git commit -m "新增：派猫猫旅行每日两轮调度（主轮派发 + claimTime 领取）"
```

---

### Task 6: API —— `GET`/`POST /api/accounts/{id}/travel`

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/api/travel.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/api/mod.rs`（`mod travel;`）
- Modify: `desktop-tauri/src-tauri/server/src/server/api/accounts.rs`（GET/POST 后缀分派各一条）

**Interfaces:**
- Consumes: `state.billing().travel_status/travel_config/travel_auto`、`state.store().get_session_by_id`、`state.auto_checkin().state()` 里的 `travel`。
- Produces:
  - `pub async fn status(state: &ServerState, account_id: &str) -> Response`（只读）
  - `pub async fn run(state: &ServerState, account_id: &str, body: &Value) -> Response`（写，跑一次 `travel_auto`）

- [ ] **Step 1: 建 `api/travel.rs`**

```rust
//! 派猫猫旅行的两个 per-account 端点。
//!
//!   GET  /api/accounts/{id}/travel   只读状态（卡片惰性查询，零写）
//!   POST /api/accounts/{id}/travel   跑一次 travel_auto（手动派发/领取）
//!
//! 只服务 CN WorkBuddy 账号；其它 provider / 国际版返回 400（无此活动）。

use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::auto_checkin;
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
```

- [ ] **Step 2: 注册模块 + 接线分派**

`api/mod.rs` 加 `pub mod travel;`（与 `pub mod onboarding;` 同级）。

`api/accounts.rs` 的 **POST 后缀段**（`/onboarding/claim` 附近）加：

```rust
        if let Some(id) = rest.strip_suffix("/travel") {
            let id = decode_segment(id);
            if !id.is_empty() {
                return super::travel::run(&state, &id, &body).await;
            }
        }
```

`api/accounts.rs` 的 **GET 后缀段**（`/onboarding` 附近）加：

```rust
        if let Some(id) = rest.strip_suffix("/travel") {
            let id = decode_segment(id);
            if !id.is_empty() {
                return super::travel::status(&state, &id).await;
            }
        }
```

> `body` 在 GET 分支不可用 —— GET 分支里只调 `status`（不传 body），POST 分支才传 `body`。确认 `run` 的 body 参数在 POST 分支能拿到已解析的 JSON（若 handler 用 `Bytes`，先 `serde_json::from_slice` 再传）。

- [ ] **Step 3: 编译 + 手工验证**

Run: `cd desktop-tauri/src-tauri/server && cargo build 2>&1 | tail -20`
Expected: 编译通过。

启动服务后（已有实例跑在 3065）：
```bash
curl -s 'http://127.0.0.1:3065/api/accounts/<CN_ACCOUNT_ID>/travel' | head -c 400
# 预期：{"status":{"available":true,"state":"...",...},"config":{...},"auto":{...}}
```

- [ ] **Step 4: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/api/travel.rs desktop-tauri/src-tauri/server/src/server/api/mod.rs desktop-tauri/src-tauri/server/src/server/api/accounts.rs
git commit -m "新增：派猫猫旅行 per-account API（GET 状态 / POST 执行）"
```

---

### Task 7: 前端 —— 签到中心旅行卡片 + i18n

**Files:**
- Modify: `desktop-tauri/ui-islands/src/islands/checkin-state.ts`
- Modify: `desktop-tauri/ui-islands/src/islands/checkin-page.tsx`
- Modify: `desktop-tauri/ui/i18n/*/checkin.js` + `desktop-tauri/i18n/i18n-src/*/checkin.json`（六语）

**Interfaces:**
- Consumes: Task 6 的 `GET/POST /api/accounts/{id}/travel`；`saveAutoCheckin`（已存在，走 `POST /api/auto-checkin`）。
- Produces: `travelOf(id)` / `fetchTravel(id)` / `runTravel(id)`；`TravelCard` 组件。

- [ ] **Step 1: 在 `checkin-state.ts` 加类型与 action**

参照文件内 `onboardingOf` / `queryOnboarding` 的写法（`patch(...)` + `subscribers` + `fetch`）：

```ts
export type TravelStatus = {
  available: boolean
  state: string
  rewardCredit: number
  dailyLimitReached: boolean
  location: { name?: string } | null
  arriveAt: number
  serverNow: number
  letter: { text?: string } | null
  useDeeplink: string
}

export type TravelCache = {
  loading: boolean
  status: TravelStatus | null
  auto: { enabled?: boolean; claimTime?: string; locationId?: number | null } | null
}

export function travelOf(id: string): TravelCache {
  return getCheckinStore().travel[id] ?? { loading: false, status: null, auto: null }
}

export async function fetchTravel(id: string): Promise<void> {
  patchTravel(id, { loading: true })
  try {
    const response = await fetch(`/api/accounts/${encodeURIComponent(id)}/travel`)
    const payload = await response.json().catch(() => null)
    patchTravel(id, {
      loading: false,
      status: payload?.status ?? null,
      auto: payload?.auto ?? null,
    })
  } catch {
    patchTravel(id, { loading: false })
  }
}

export async function runTravel(id: string): Promise<void> {
  patchTravel(id, { loading: true })
  try {
    await fetch(`/api/accounts/${encodeURIComponent(id)}/travel`, { method: 'POST' })
  } finally {
    await fetchTravel(id)
  }
}
```

并（参照 `patchOnboarding`）加 `function patchTravel(id, next)` 写入 `store.travel[id]`；在 `CheckinStore` 类型里加 `travel: Record<string, TravelCache>`，初值 `{}`。

- [ ] **Step 2: 在 `checkin-page.tsx` 渲染旅行行**

在 `AccountRows` 里，对 `group.id === 'workbuddy'` 的账号行，渲染一个按钮：

```tsx
{group.id === 'workbuddy' && (
  <Button variant='outline' size='sm' disabled={busy(account.id, 'travel')}
    onClick={() => { void fetchTravel(account.id); void runTravel(account.id) }}>
    {t('旅行')}
  </Button>)
}
```

并新增 `TravelCard` 组件（在账号行展开区渲染），状态映射：
- `!available` → 「暂无旅行活动」；
- `state === 'idle'` → 「空闲」+「派发」按钮（`auto?.enabled` 时）；
- `state === 'traveling'` → 倒计时 `arriveAt - serverNow`（用本地时钟递增）；
- `state === 'arrived'` → 「可领 {rewardCredit} 积分」+「领取」按钮；
- `dailyLimitReached` → 追加「今日派遣已用完」。

组件内用 `useCheckinStore()` + `React.useEffect(() => { void fetchTravel(account.id) }, [account.id])` 惰性加载。

- [ ] **Step 3: 补 i18n 词条**

在 `ui/i18n/<lang>/checkin.js` 与 `i18n/i18n-src/<lang>/checkin.json` 六语各加：
`旅行` / `暂无旅行活动` / `空闲` / `派发` / `旅行中` / `可领 {n} 积分` / `领取` / `今日派遣已用完`。
（按仓库 i18n 构建脚本的流程：改 `i18n-src` 后跑构建生成 `ui/i18n`，以实际脚本为准。）

- [ ] **Step 4: 构建前端 + 手工验证**

Run: `cd desktop-tauri && npm run build:ui 2>&1 | tail -20`（或仓库实际的前端构建脚本）
Expected: 构建通过。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/ui-islands desktop-tauri/ui/i18n desktop-tauri/i18n
git commit -m "新增：签到中心派猫猫旅行卡片与手动按钮"
```

---

### Task 8: 端到端手工验证（当天派发 + 领取闭环）

**Files:** 无（验证任务）

**Interfaces:**
- Consumes: Task 1–7 全部。
- Produces: 一份可归档的验证记录（写入 PR 描述或提交信息，不新建文件）。

- [ ] **Step 1: 备份数据目录**

```bash
cd /root/programming/github/agent2api
tar czf /tmp/agent2api-data-$(date +%s).tar.gz data/
```

- [ ] **Step 2: 用真实 CN 账号验证读路径**

```bash
# 列出一个启用的 CN workbuddy 账号 id
python3 - <<'PY'
import sqlite3, json
db = sqlite3.connect('file:data/agent2api.db?mode=ro', uri=True)
for rid, d in db.execute("select id, data from accounts where provider='workbuddy' and enabled=1 order by priority"):
    j = json.loads(d)
    if j.get('edition') != 'intl':
        print(rid); break
PY
```

```bash
curl -s 'http://127.0.0.1:3065/api/accounts/<ID>/travel' | python3 -m json.tool | head -30
```

Expected: `status.available=true`，`status.state` 为 `idle`/`traveling`/`arrived` 之一；`auto` 反映当前配置。

- [ ] **Step 3: 验证写路径（手点一次）**

```bash
curl -s -X POST 'http://127.0.0.1:3065/api/accounts/<ID>/travel' | python3 -m json.tool
```

Expected: 返回 `{available,state,claimed,rewardCredit,message}`。若原本 `idle` 且未达上限 → `message` 含「已派出」；若 `arrived` → `claimed=true`。
再查一次状态确认 `state` 变化。

- [ ] **Step 4: 验证自动调度（不等到次日）**

在签到中心开启「旅行」（`travel.enabled=true`，`claimTime` 设为几分钟后），把 `autoCheckin.time` 设为一个已过的时刻并手动触发一次 tick（重启服务或调 `/api/auto-checkin/run`），观察日志 `[Travel]` 行与 `config.json` 的 `autoCheckin.travel.lastDepartDate` / `lastClaimDate` 是否按预期落盘。

- [ ] **Step 5: 回归**

Run: `cd desktop-tauri/src-tauri/server && cargo test 2>&1 | tail -20`
Expected: 全绿（含既有签到/计费用例）。

- [ ] **Step 6: 提交验证记录**

```bash
cd /root/programming/github/agent2api
git commit --allow-empty -m "验证：派猫猫旅行端到端（CN 账号读/写路径均通）"
```

---

## Self-Review（写完计划后对照 spec 的检查）

**1. Spec 覆盖：**
- §3.1 host 按 edition 派生 → Task 2（`travel_base`）✅
- §3.2 四端点（无 /v2）→ Task 2 ✅
- §3.3 响应字段 → Task 2（`TravelStatus`）✅
- §3.4 状态机 → Task 3 ✅
- §4.1 host 覆盖（方案 A）→ Task 1 ✅
- §5.2 travel 模块 → Task 2/3 ✅
- §5.3 结果行挂 travel → **已改为不挂**（见 Deviations 1），由 Task 6 GET + Task 7 卡片代替 ✅
- §5.4 每日两轮 → Task 5 ✅
- §5.5 API → Task 6 ✅
- §5.6 前端 → Task 7 ✅
- §6 配置模型 → Task 4 ✅
- §7 兼容安全 / §8 错误处理 → Global Constraints + Task 2/3 的收敛 ✅
- §9 测试计划 → 各 Task 的测试步 ✅
- §10 分阶段 → Task 1–8 对应 ✅

**2. 占位符扫描：** 无 TBD/TODO；每个代码步都有具体代码。✅

**3. 类型一致性：** `TravelOpts.location_id`（Task 3）→ Task 5/6 一致；`TravelState`（Task 4）→ Task 5 一致；`travel_result` 字段名（`rewardCredit`/`dailyLimitReached`）→ Task 6/7 一致。✅

**4. Review Focus 对应：** 五条均在各 Task 的测试步中钉住（Task 1 回归、Task 4/5 去重、Task 3 上限顺序、Task 2 intl 空数据、Task 4 合并写回）。✅

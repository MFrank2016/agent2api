# 代理池轮询（Proxy Pool Rotation）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 agent2api 的账号能把出网请求分摊到代理池里的多个出口 IP（round-robin / random / least-latency），并在连接失败时于同一请求内透明换出口重试。

**Architecture:** 在账号出口层引入统一类型 `AccountEgress`（Direct / Single / Rotate）。账号 `proxy` 新增 `pool-rotate` 来源（可引用分组 `group` 或列举 `proxyIds`）。两条聊天转发路径（`upstream` 通用 + `custom`）都经 `upstream::request::send_chat_request` 出网，改由 `egress::dispatch` 按策略选出口、失败换下一个。轮询游标与 EWMA 存进程内存。

**Tech Stack:** Rust 2021（MSRV 1.88）、axum、reqwest、serde_json、rusqlite（kv 存储）、getrandom；前端原生 HTML/JS。

**Spec:** `docs/superpowers/specs/2026-10-09-proxy-pool-rotation-design.md`

## Global Constraints

- Rust edition 2021，MSRV 1.88；**不新增第三方依赖**（随机用现有 `getrandom`）。
- release 是 `panic = abort`：**绝不 `unwrap`/`expect` 可失败路径**；错误如实返回。
- 未使用 `pool-rotate` 的账号行为与日志**逐字不变**。
- 代理用户名/密码**绝不进日志或 HTTP 响应**（沿用 `ResolvedProxy` 的 describe 口径）。
- 修改文件遵循既有单文件行数约定，超长则拆分。
- 测试命令（在仓库根执行）：`cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server <filter>`

## Review Focus

1. 账号配了 `pool-rotate` 但池里条目全被禁用/删除 → 期望**回退直连** + `proxyError` 提示，不是请求报错。
2. 轮询成员里混入端口非法的脏条目 → 期望**跳过该成员**，其余照常，不 panic。
3. `onError = none` 且首选出口连不上 → 期望**只失败一次**（不换出口），错误如实透传。
4. 上游返回 4xx/5xx（已拿到响应）→ 期望**不换出口**，原样交回账号降级链。
5. 会话/计划里的 `egress` JSON 畸形（手改库）→ 期望回退直连，不 panic。
6. `least-latency`：某出口记录失败惩罚后 → 后续请求倾向避开它。

---

## File Structure

- `core/proxies.rs`（改）：`Strategy`/`OnError`/`RotationPlan`/`AccountEgress` 类型、`normalize_account_proxy` 的 `pool-rotate` 分支、`resolve_account_egress`。
- `core/proxy_pool.rs`（改）：条目 `group` 字段、`select_members`/`members_for`。
- `core/egress_rotation.rs`（建）：轮询游标 + EWMA + `candidates()`。
- `core/egress.rs`（改）：`dispatch`。
- `core/mod.rs`（改）：注册 `egress_rotation`。
- `core/upstream/mod.rs`、`core/upstream/request.rs`、`core/upstream/rotate.rs`、`core/upstream/provider_loop.rs`（改）：`RouteTarget.proxy`/`TransportRequest.proxy` → `egress`；`with_proxy_notice`；`send_chat_request` 走 `dispatch`。
- `core/providers/custom/forward.rs`、`core/account_store/custom_accounts.rs`（改）：`proxy` → `egress`。
- `core/account_store/store.rs`、`core/account_store/state.rs`（改）：会话/`SessionById` 增加 `egress`。
- `api/proxies.rs`（改）：池列表返回 `groups` 摘要。
- `ui/*`（改）：网络代理页分组字段 + 账号出口"轮询"模式。

---

### Task 1: `AccountEgress` / `RotationPlan` 类型

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/proxies.rs`

**Interfaces:**
- Produces:
  - `pub enum Strategy { RoundRobin, Random, LeastLatency }`，`Strategy::parse(&str)->Option<Self>`、`Strategy::as_str(self)->&'static str`
  - `pub enum OnError { Next, None }`，`OnError::parse(&str)->Option<Self>`、`OnError::as_str(self)->&'static str`
  - `pub struct RotationPlan { pub members: Vec<ResolvedProxy>, pub strategy: Strategy, pub on_error: OnError, pub group_key: String }`
  - `pub enum AccountEgress { Direct, Single(ResolvedProxy), Rotate(RotationPlan) }`，`AccountEgress::from_json(&Value)->Self`、`AccountEgress::to_json(&self)->Value`、`AccountEgress::primary(&self)->Option<&ResolvedProxy>`
  - `pub(crate) fn proxy_to_json(&ResolvedProxy)->Value`

- [ ] **Step 1: Write the failing test**（加到 `proxies.rs` 末尾的 `#[cfg(test)] mod tests`）

```rust
#[test]
fn account_egress_round_trips_a_rotation_plan() {
    let plan = RotationPlan {
        members: vec![ResolvedProxy {
            source: "pool".into(), protocol: "http".into(), host: "1.2.3.4".into(),
            port: Some(8080), username: "u".into(), password: "p".into(), label: "a".into(),
        }],
        strategy: Strategy::LeastLatency,
        on_error: OnError::Next,
        group_key: "g".into(),
    };
    let json = AccountEgress::Rotate(plan).to_json();
    match AccountEgress::from_json(&json) {
        AccountEgress::Rotate(back) => {
            assert_eq!(back.strategy, Strategy::LeastLatency);
            assert_eq!(back.on_error, OnError::Next);
            assert_eq!(back.members.len(), 1);
            assert_eq!(back.members[0].host, "1.2.3.4");
        }
        other => panic!("expected rotate, got {other:?}"),
    }
}

#[test]
fn account_egress_from_json_falls_back_to_direct_on_garbage() {
    assert!(matches!(AccountEgress::from_json(&serde_json::json!({"source":"pool-rotate","members":[]})), AccountEgress::Direct));
    assert!(matches!(AccountEgress::from_json(&serde_json::json!(42)), AccountEgress::Direct));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxies::tests::account_egress -v`
Expected: FAIL（`cannot find type Strategy` / `AccountEgress`）

- [ ] **Step 3: Write minimal implementation**（加在 `proxies.rs` 中 `ResolvedProxy` 定义之后）

```rust
/// 轮询策略（账号出口为 `pool-rotate` 时生效）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy { RoundRobin, Random, LeastLatency }

impl Strategy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "round-robin" => Some(Self::RoundRobin),
            "random" => Some(Self::Random),
            "least-latency" => Some(Self::LeastLatency),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self { Self::RoundRobin => "round-robin", Self::Random => "random", Self::LeastLatency => "least-latency" }
    }
}

/// 连接失败时是否换下一个出口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError { Next, None }

impl OnError {
    pub fn parse(value: &str) -> Option<Self> {
        match value { "next" => Some(Self::Next), "none" => Some(Self::None), _ => None }
    }
    pub fn as_str(self) -> &'static str {
        match self { Self::Next => "next", Self::None => "none" }
    }
}

/// 轮询计划：成员 + 策略 + 失败语义 + 进程内状态键。
#[derive(Clone, Debug)]
pub struct RotationPlan {
    pub members: Vec<ResolvedProxy>,
    pub strategy: Strategy,
    pub on_error: OnError,
    /// 游标 / EWMA 的状态键（成员端点排序拼接，见 `core::egress_rotation`）。
    pub group_key: String,
}

/// 一次请求可用的出网出口（`Direct` ≡ 无代理）。
#[derive(Clone, Debug)]
pub enum AccountEgress { Direct, Single(ResolvedProxy), Rotate(RotationPlan) }

/// 出口 → JSON（与 `account_store::proxy_json` 同形）。
pub(crate) fn proxy_to_json(proxy: &ResolvedProxy) -> Value {
    json!({
        "source": proxy.source, "protocol": proxy.protocol, "host": proxy.host,
        "port": proxy.port, "username": proxy.username, "password": proxy.password, "label": proxy.label,
    })
}

impl AccountEgress {
    /// 从会话/计划里的 JSON 还原；畸形数据一律回退 `Direct`（可用性优先）。
    pub fn from_json(value: &Value) -> Self {
        if value.as_object().and_then(|o| o.get("source")).and_then(Value::as_str) == Some("pool-rotate") {
            let members: Vec<ResolvedProxy> = value
                .get("members").and_then(Value::as_array)
                .map(|items| items.iter().filter_map(|it| ResolvedProxy::from_json(it).ok().flatten()).collect())
                .unwrap_or_default();
            if members.is_empty() { return Self::Direct; }
            let strategy = value.get("strategy").and_then(Value::as_str).and_then(Strategy::parse).unwrap_or(Strategy::RoundRobin);
            let on_error = value.get("onError").and_then(Value::as_str).and_then(OnError::parse).unwrap_or(OnError::Next);
            let group_key = value.get("groupKey").and_then(Value::as_str).unwrap_or("").to_string();
            return Self::Rotate(RotationPlan { members, strategy, on_error, group_key });
        }
        match ResolvedProxy::from_json(value) {
            Ok(Some(proxy)) => Self::Single(proxy),
            _ => Self::Direct,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Direct => Value::Null,
            Self::Single(proxy) => proxy_to_json(proxy),
            Self::Rotate(plan) => json!({
                "source": "pool-rotate",
                "members": plan.members.iter().map(proxy_to_json).collect::<Vec<_>>(),
                "strategy": plan.strategy.as_str(),
                "onError": plan.on_error.as_str(),
                "groupKey": plan.group_key,
            }),
        }
    }

    pub fn primary(&self) -> Option<&ResolvedProxy> {
        match self {
            Self::Direct => None,
            Self::Single(proxy) => Some(proxy),
            Self::Rotate(plan) => plan.members.first(),
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxies::tests::account_egress -v`
Expected: PASS（2 passed）

- [ ] **Step 5: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/proxies.rs
git commit -m "feat(proxies): add AccountEgress/RotationPlan types for pool rotation"
```

---

### Task 2: 代理池条目 `group` 字段

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/proxy_pool.rs`

**Interfaces:**
- Produces: 池条目新增 `group: String`；`normalize_item` 写入、`describe_item` 回显。

- [ ] **Step 1: Write the failing test**（加到 `proxy_pool.rs` 的 tests 模块）

```rust
#[test]
fn manual_item_keeps_group_tag_and_defaults_empty() {
    let tagged = normalize_item(&serde_json::json!({"name":"a","host":"1.2.3.4","port":8080,"group":"kilo"}), None).unwrap();
    assert_eq!(tagged["group"], "kilo");
    let plain = normalize_item(&serde_json::json!({"name":"b","host":"1.2.3.5","port":8080}), None).unwrap();
    assert_eq!(plain["group"], "");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxy_pool::tests::manual_item_keeps_group -v`
Expected: FAIL（`tagged["group"]` 为 null）

- [ ] **Step 3: Write minimal implementation**

在 `proxy_pool.rs` 常量区加：

```rust
const MAX_GROUP_LENGTH: usize = 60;
```

在 `normalize_item` 里（`let name = ...` 之后、`normalized.insert("name", ...)` 附近）加：

```rust
    let group = clean_string(object.get("group"), MAX_GROUP_LENGTH);
```

并在 `normalized.insert("source", ...)` 之后插入：

```rust
    normalized.insert("group".to_string(), Value::String(group));
```

在 `describe_item` 的 json 里加一行：

```rust
        "group": text_of(item, "group"),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxy_pool::tests::manual_item_keeps_group -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/proxy_pool.rs
git commit -m "feat(proxy_pool): add optional group tag to pool entries"
```

---

### Task 3: `select_members` / `members_for`

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/proxy_pool.rs`

**Interfaces:**
- Consumes: `resolve_item`（既有）、`ProxyResolution`（既有）。
- Produces:
  - `fn select_members(items: &[Value], group: &str, proxy_ids: &[String]) -> Vec<ResolvedProxy>`（纯函数，可测）
  - `pub fn members_for(group: &str, proxy_ids: &[String]) -> Vec<ResolvedProxy>`（读库后调 `select_members`）

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn select_members_unions_group_and_ids_skipping_disabled() {
    let items = vec![
        serde_json::json!({"id":"a","name":"a","source":"manual","enabled":true,"protocol":"http","host":"1.1.1.1","port":1,"group":"kilo"}),
        serde_json::json!({"id":"b","name":"b","source":"manual","enabled":false,"protocol":"http","host":"2.2.2.2","port":2,"group":"kilo"}),
        serde_json::json!({"id":"c","name":"c","source":"manual","enabled":true,"protocol":"http","host":"3.3.3.3","port":3,"group":"other"}),
    ];
    let ids = vec!["c".to_string(), "a".to_string()];
    let members = select_members(&items, "kilo", &ids);
    // a（group）在前，c（proxyIds）在后；b 被禁用跳过；a 去重
    let hosts: Vec<&str> = members.iter().map(|m| m.host.as_str()).collect();
    assert_eq!(hosts, vec!["1.1.1.1", "3.3.3.3"]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxy_pool::tests::select_members -v`
Expected: FAIL（`cannot find function select_members`）

- [ ] **Step 3: Write minimal implementation**

在 `proxy_pool.rs` 里（`resolve_item` 之后）加：

```rust
/// 收集一个条目为成员（跳过已见 / 已禁用 / 解析失败）。
fn push_member(
    item: &Value,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<crate::server::core::proxies::ResolvedProxy>,
) {
    let id = text_of(item, "id");
    if id.is_empty() || !seen.insert(id) {
        return;
    }
    if matches!(item.get("enabled"), Some(Value::Bool(false))) {
        return;
    }
    if let Some(ProxyResolution::Resolved(proxy)) = resolve_item(item) {
        out.push(proxy);
    }
}

/// 按「分组标签 ∪ 显式 id」收集成员：先 group（按池内顺序），再 proxyIds（按传入顺序），按条目 id 去重。
pub(crate) fn select_members(
    items: &[Value],
    group: &str,
    proxy_ids: &[String],
) -> Vec<crate::server::core::proxies::ResolvedProxy> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    if !group.is_empty() {
        for item in items.iter().filter(|it| text_of(it, "group") == group) {
            push_member(item, &mut seen, &mut out);
        }
    }
    for id in proxy_ids {
        if let Some(item) = items.iter().find(|it| text_of(it, "id") == *id) {
            push_member(item, &mut seen, &mut out);
        }
    }
    out
}

/// 读库后按分组/显式 id 收集可用出口（供 `proxies::resolve_account_egress`）。
pub fn members_for(group: &str, proxy_ids: &[String]) -> Vec<crate::server::core::proxies::ResolvedProxy> {
    select_members(&read_items(), group, proxy_ids)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxy_pool::tests::select_members -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/proxy_pool.rs
git commit -m "feat(proxy_pool): select_members/members_for for rotation groups"
```

---

### Task 4: `normalize_account_proxy` 的 `pool-rotate` 分支

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/proxies.rs`

**Interfaces:**
- Produces: `normalize_account_proxy` 接受并存 `{source:"pool-rotate", group, proxyIds, strategy, onError}`。

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn normalize_pool_rotate_accepts_and_rejects() {
    let ok = normalize_account_proxy(&serde_json::json!({
        "source":"pool-rotate","group":"kilo","strategy":"least-latency","onError":"none"
    })).unwrap().unwrap();
    assert_eq!(ok["strategy"], "least-latency");
    assert_eq!(ok["onError"], "none");

    // 空成员
    assert!(normalize_account_proxy(&serde_json::json!({"source":"pool-rotate"})).is_err());
    // 非法策略
    assert!(normalize_account_proxy(&serde_json::json!({"source":"pool-rotate","group":"x","strategy":"fastest"})).is_err());
    // proxyIds 非数组
    assert!(normalize_account_proxy(&serde_json::json!({"source":"pool-rotate","proxyIds":"a"})).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxies::tests::normalize_pool_rotate -v`
Expected: FAIL（`source=pool-rotate` 落到「不支持的代理来源」）

- [ ] **Step 3: Write minimal implementation**

在 `proxies.rs` 常量区加：

```rust
const MAX_GROUP_LENGTH: usize = 60;
const MAX_ROTATION_MEMBERS: usize = 64;
```

在 `normalize_account_proxy` 里，`if source == "pool" { ... }` 分支之后、`if source != "custom"` 之前插入：

```rust
    if source == "pool-rotate" {
        let group = clean_string(object.get("group"), MAX_GROUP_LENGTH);
        let proxy_ids = match object.get("proxyIds") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut ids: Vec<String> = Vec::new();
                for item in items {
                    let id = clean_string(Some(item), MAX_LABEL_LENGTH);
                    if id.is_empty() {
                        return Err(ProxyConfigError::new("轮询出口的 proxyIds 含空项"));
                    }
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                if ids.len() > MAX_ROTATION_MEMBERS {
                    return Err(ProxyConfigError::new(format!("轮询出口成员过多（最多 {MAX_ROTATION_MEMBERS}）")));
                }
                ids
            }
            Some(_) => return Err(ProxyConfigError::new("轮询出口的 proxyIds 必须是数组")),
        };
        if group.is_empty() && proxy_ids.is_empty() {
            return Err(ProxyConfigError::new("轮询出口缺少成员（group 或 proxyIds）"));
        }
        let strategy = {
            let raw = clean_string(object.get("strategy"), 20).to_lowercase();
            if raw.is_empty() { Strategy::RoundRobin }
            else { Strategy::parse(&raw).ok_or_else(|| ProxyConfigError::new("轮询策略必须是 round-robin / random / least-latency"))? }
        };
        let on_error = {
            let raw = clean_string(object.get("onError"), 10).to_lowercase();
            if raw.is_empty() { OnError::Next }
            else { OnError::parse(&raw).ok_or_else(|| ProxyConfigError::new("onError 必须是 next 或 none"))? }
        };
        return Ok(Some(json!({
            "source": "pool-rotate",
            "group": group,
            "proxyIds": proxy_ids,
            "strategy": strategy.as_str(),
            "onError": on_error.as_str(),
        })));
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxies::tests::normalize_pool_rotate -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/proxies.rs
git commit -m "feat(proxies): validate pool-rotate account proxy source"
```

---

### Task 5: `resolve_account_egress`

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/proxies.rs`

**Interfaces:**
- Consumes: `proxy_pool::members_for`（Task 3）。
- Produces:
  - `pub enum EgressResolution { Resolved(AccountEgress), Failed(String) }`
  - `pub fn resolve_account_egress(config: Option<&Value>) -> EgressResolution`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn resolve_egress_passes_through_single_and_reports_empty_rotation() {
    // 单出口沿用既有解析
    let single = resolve_account_egress(Some(&serde_json::json!({"source":"custom","protocol":"http","host":"1.2.3.4","port":8080})));
    assert!(matches!(single, EgressResolution::Resolved(AccountEgress::Single(_))));

    // 无配置 → Direct
    assert!(matches!(resolve_account_egress(None), EgressResolution::Resolved(AccountEgress::Direct)));

    // 轮询但池里无成员 → Failed（测试库为空）
    let rot = resolve_account_egress(Some(&serde_json::json!({"source":"pool-rotate","group":"__nope__"})));
    assert!(matches!(rot, EgressResolution::Failed(_)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxies::tests::resolve_egress -v`
Expected: FAIL（`cannot find function resolve_account_egress`）

- [ ] **Step 3: Write minimal implementation**

```rust
/// 出口解析结果：成功给可用出口，失败给原因（转发层回退直连）。
#[derive(Clone, Debug)]
pub enum EgressResolution { Resolved(AccountEgress), Failed(String) }

/// 把账号里存的代理配置解析成统一出口（Direct / Single / Rotate）。
pub fn resolve_account_egress(config: Option<&Value>) -> EgressResolution {
    let Some(config) = config else { return EgressResolution::Resolved(AccountEgress::Direct) };
    if config.is_null() { return EgressResolution::Resolved(AccountEgress::Direct); }
    let Some(object) = config.as_object() else {
        return EgressResolution::Failed("不支持的代理来源: undefined".to_string());
    };
    if object.get("source").and_then(Value::as_str) == Some("pool-rotate") {
        let group = object.get("group").and_then(Value::as_str).unwrap_or("");
        let proxy_ids: Vec<String> = object
            .get("proxyIds").and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let members = crate::server::core::proxy_pool::members_for(group, &proxy_ids);
        if members.is_empty() {
            return EgressResolution::Failed("轮询组没有可用出口".to_string());
        }
        let strategy = object.get("strategy").and_then(Value::as_str).and_then(Strategy::parse).unwrap_or(Strategy::RoundRobin);
        let on_error = object.get("onError").and_then(Value::as_str).and_then(OnError::parse).unwrap_or(OnError::Next);
        let group_key = members
            .iter()
            .map(|m| format!("{}://{}:{}", m.protocol, m.host, m.port.map(|p| p.to_string()).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("|");
        return EgressResolution::Resolved(AccountEgress::Rotate(RotationPlan { members, strategy, on_error, group_key }));
    }
    match resolve_account_proxy(Some(config)) {
        None => EgressResolution::Resolved(AccountEgress::Direct),
        Some(ProxyResolution::Resolved(proxy)) => EgressResolution::Resolved(AccountEgress::Single(proxy)),
        Some(ProxyResolution::Failed(message)) => EgressResolution::Failed(message),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server proxies::tests::resolve_egress -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/proxies.rs
git commit -m "feat(proxies): resolve_account_egress (Direct/Single/Rotate)"
```

---

### Task 6: `egress_rotation` 状态与候选顺序

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/core/egress_rotation.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/mod.rs`（加 `pub mod egress_rotation;`）

**Interfaces:**
- Consumes: `AccountEgress`/`RotationPlan`/`Strategy`/`ResolvedProxy`（Task 1）。
- Produces:
  - `pub fn candidates(egress: &AccountEgress) -> Vec<ResolvedProxy>`
  - `pub fn record_success(plan: &RotationPlan, proxy: &ResolvedProxy, latency_ms: f64)`
  - `pub fn record_failure(plan: &RotationPlan, proxy: &ResolvedProxy, penalty_ms: f64)`

- [ ] **Step 1: Write the failing test**（文件内 `#[cfg(test)] mod tests`）

```rust
#[test]
fn round_robin_cycles_through_all_members() {
    let plan = RotationPlan {
        members: vec![m("1.1.1.1"), m("2.2.2.2"), m("3.3.3.3")],
        strategy: Strategy::RoundRobin, on_error: OnError::Next, group_key: "rr-test".into(),
    };
    let egress = AccountEgress::Rotate(plan);
    let first = candidates(&egress)[0].host.clone();
    let second = candidates(&egress)[0].host.clone();
    let third = candidates(&egress)[0].host.clone();
    assert_eq!(vec![first, second, third], vec!["1.1.1.1", "2.2.2.2", "3.3.3.3"]);
}

#[test]
fn least_latency_prefers_the_fast_member_after_a_failure_penalty() {
    let plan = RotationPlan {
        members: vec![m("1.1.1.1"), m("2.2.2.2")],
        strategy: Strategy::LeastLatency, on_error: OnError::Next, group_key: "ll-test".into(),
    };
    let egress = AccountEgress::Rotate(plan.clone());
    record_success(&plan, &m("2.2.2.2"), 50.0);
    record_failure(&plan, &m("1.1.1.1"), 30_000.0);
    assert_eq!(candidates(&egress)[0].host, "2.2.2.2");
}

fn m(host: &str) -> ResolvedProxy {
    ResolvedProxy { source: "pool".into(), protocol: "http".into(), host: host.into(),
        port: Some(80), username: String::new(), password: String::new(), label: String::new() }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server egress_rotation:: -v`
Expected: FAIL（module 不存在 / 编译错）

- [ ] **Step 3: Write minimal implementation**

`core/mod.rs` 加 `pub mod egress_rotation;`（与 `pub mod egress;` 相邻）。新建 `core/egress_rotation.rs`：

```rust
//! 轮询出口的进程内状态：round-robin 游标 + least-latency 的 EWMA（重启清零）。
use std::collections::HashMap;
use std::sync::Mutex;

use crate::server::core::proxies::{AccountEgress, ResolvedProxy, RotationPlan, Strategy};

/// EWMA 平滑系数。
const ALPHA: f64 = 0.3;

#[derive(Default)]
struct State { cursor: u64, ewma: HashMap<String, f64> }

static STATE: Mutex<Option<HashMap<String, State>>> = Mutex::new(None);

fn with_state<R>(key: &str, f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = match STATE.lock() { Ok(g) => g, Err(p) => p.into_inner() };
    let map = guard.get_or_insert_with(HashMap::new);
    f(map.entry(key.to_string()).or_default())
}

fn member_key(proxy: &ResolvedProxy) -> String {
    format!("{}://{}:{}", proxy.protocol, proxy.host, proxy.port.map(|p| p.to_string()).unwrap_or_default())
}

fn random_below(bound: u64) -> u64 {
    if bound == 0 { return 0; }
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_err() { return 0; }
    u64::from_le_bytes(buf) % bound
}

/// 本次请求的候选顺序（首个为首选；其后为 onError=next 的重试顺序）。
pub fn candidates(egress: &AccountEgress) -> Vec<ResolvedProxy> {
    match egress {
        AccountEgress::Direct => Vec::new(),
        AccountEgress::Single(proxy) => vec![proxy.clone()],
        AccountEgress::Rotate(plan) => rotate_candidates(plan),
    }
}

fn rotate_candidates(plan: &RotationPlan) -> Vec<ResolvedProxy> {
    let n = plan.members.len();
    if n == 0 { return Vec::new(); }
    match plan.strategy {
        Strategy::RoundRobin => {
            let start = with_state(&plan.group_key, |s| {
                let c = s.cursor; s.cursor = s.cursor.wrapping_add(1); c
            }) as usize % n;
            (0..n).map(|i| plan.members[(start + i) % n].clone()).collect()
        }
        Strategy::Random => {
            let mut items = plan.members.clone();
            for i in (1..items.len()).rev() {
                let j = random_below(i as u64 + 1) as usize;
                items.swap(i, j);
            }
            items
        }
        Strategy::LeastLatency => {
            let mut ranked: Vec<(f64, usize)> = plan.members.iter().enumerate()
                .map(|(i, m)| (with_state(&plan.group_key, |s| *s.ewma.get(&member_key(m)).unwrap_or(&0.0)), i))
                .collect();
            ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            ranked.into_iter().map(|(_, i)| plan.members[i].clone()).collect()
        }
    }
}

fn sample(plan: &RotationPlan, proxy: &ResolvedProxy, value: f64) {
    if plan.strategy != Strategy::LeastLatency { return; }
    let key = member_key(proxy);
    with_state(&plan.group_key, |s| {
        let entry = s.ewma.entry(key).or_insert(value);
        *entry = ALPHA * value + (1.0 - ALPHA) * *entry;
    });
}

/// 记录一次成功延迟样本（毫秒）。
pub fn record_success(plan: &RotationPlan, proxy: &ResolvedProxy, latency_ms: f64) { sample(plan, proxy, latency_ms); }

/// 记录一次连接失败（惩罚样本抬高 EWMA，使后续倾向避开该出口）。
pub fn record_failure(plan: &RotationPlan, proxy: &ResolvedProxy, penalty_ms: f64) { sample(plan, proxy, penalty_ms); }

#[cfg(test)]
mod tests {
    use super::*;
    fn m(host: &str) -> ResolvedProxy {
        ResolvedProxy { source: "pool".into(), protocol: "http".into(), host: host.into(),
            port: Some(80), username: String::new(), password: String::new(), label: String::new() }
    }
    // ...（Step 1 的两个测试）
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server egress_rotation:: -v`
Expected: PASS（2 passed）

- [ ] **Step 5: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/egress_rotation.rs desktop-tauri/src-tauri/server/src/server/core/mod.rs
git commit -m "feat(egress_rotation): round-robin cursor + least-latency EWMA"
```

---

### Task 7: `egress::dispatch`

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/egress.rs`

**Interfaces:**
- Consumes: `egress_rotation::{candidates, record_success, record_failure}`（Task 6）。
- Produces: `pub async fn dispatch<F>(egress: &AccountEgress, build: F) -> Result<reqwest::Response, reqwest::Error> where F: Fn(&reqwest::Client) -> reqwest::RequestBuilder`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn dispatch_direct_hits_an_unreachable_port_and_errors() {
    let egress = AccountEgress::Direct;
    let result = dispatch(&egress, |client| client.get("http://127.0.0.1:1/")).await;
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server egress::tests::dispatch_direct -v`
Expected: FAIL（`cannot find function dispatch`）

- [ ] **Step 3: Write minimal implementation**

在 `egress.rs` 顶部补 `use crate::server::core::proxies::{AccountEgress, OnError};`，并加：

```rust
/// 按出口类型发一次请求：Direct/Single 直发；Rotate 按候选顺序发，连接失败换下一个。
///
/// 只在「一个响应字节都没收到」的连接/传输错误时换出口；一旦拿到响应
/// （含 4xx/5xx）立即返回，交回账号降级链处理。
pub async fn dispatch<F>(egress: &AccountEgress, build: F) -> Result<reqwest::Response, reqwest::Error>
where
    F: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
{
    let candidates = crate::server::core::egress_rotation::candidates(egress);
    if candidates.is_empty() {
        return build(&client_for(None)).send().await;
    }
    let limit = match egress {
        AccountEgress::Rotate(plan) if plan.on_error == OnError::None => 1,
        _ => candidates.len(),
    };
    let penalty = crate::server::config::timeout_settings().connect_ms() as f64;
    let mut last_error: Option<reqwest::Error> = None;
    for proxy in candidates.iter().take(limit) {
        let started = logging::now_ms();
        match build(&client_for(Some(proxy))).send().await {
            Ok(response) => {
                if let AccountEgress::Rotate(plan) = egress {
                    crate::server::core::egress_rotation::record_success(plan, proxy, (logging::now_ms() - started) as f64);
                }
                return Ok(response);
            }
            Err(error) => {
                if let AccountEgress::Rotate(plan) = egress {
                    crate::server::core::egress_rotation::record_failure(plan, proxy, penalty);
                }
                last_error = Some(error);
            }
        }
    }
    // 至少尝试了一个出口（candidates 非空、limit ≥ 1）
    match last_error {
        Some(error) => Err(error),
        None => build(&client_for(None)).send().await,
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server egress::tests::dispatch_direct -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/egress.rs
git commit -m "feat(egress): dispatch with rotation + in-request failover"
```

---

### Task 8: 接线（RouteTarget / TransportRequest / 会话 / custom）

**Files:**
- Modify: `core/upstream/mod.rs`、`core/upstream/request.rs`、`core/upstream/rotate.rs`、`core/upstream/provider_loop.rs`、`core/account_store/store.rs`、`core/account_store/state.rs`、`core/account_store/custom_accounts.rs`、`core/providers/custom/forward.rs`

**Interfaces:**
- Consumes: `AccountEgress`（Task 1）、`resolve_account_egress`（Task 5）、`egress::dispatch`（Task 7）。
- Produces: `RouteTarget.egress: AccountEgress`、`TransportRequest.egress: AccountEgress`、`SessionById.egress: Value`、`CustomCredential.egress: AccountEgress`。

- [ ] **Step 1: 改 `RouteTarget` 与 `TransportRequest` 字段**

`core/upstream/mod.rs`：把 `pub proxy: Option<ResolvedProxy>,` 改成

```rust
    pub egress: crate::server::core::proxies::AccountEgress,
```

（删掉该文件里对 `ResolvedProxy` 的 now-unused import，若编译器提示。）

`core/upstream/request.rs`：`TransportRequest` 的 `pub proxy: Option<ResolvedProxy>,` 改成

```rust
    pub egress: crate::server::core::proxies::AccountEgress,
```

- [ ] **Step 2: 改 `send_chat_request` 用 `dispatch`**

`core/upstream/request.rs`（`send_chat_request` 内）：

```rust
    let url = plan.url.clone();
    let payload = plan.payload.clone();
    let headers = plan.headers.clone();
    let response = crate::server::core::egress::dispatch(&plan.egress, |client| {
        let mut builder = client.post(&url).body(payload.clone());
        for (key, value) in &headers {
            builder = builder.header(key, value);
        }
        if let Some(timeout) = NO_TOTAL_TIMEOUT {
            builder = builder.timeout(Duration::from_millis(timeout));
        }
        builder
    })
    .await;
```

把原来的 `match tokio::time::timeout(headers_budget, builder.send()).await` 换成对 `dispatch(...).await` 结果的 `tokio::time::timeout(headers_budget, ...)` 包裹（超时语义不变）：

```rust
    let response = tokio::time::timeout(headers_budget, response).await;
```

`via` 文案改用 `plan.egress.primary()`（有 label 用 label，否则 host，None 为「直连」）。

- [ ] **Step 3: 改 `with_proxy_notice`**

`core/upstream/rotate.rs`：签名改为

```rust
pub(super) fn with_proxy_notice(
    account: Value,
    egress: Value,
    proxy_error: Option<String>,
    account_id: String,
) -> RouteTarget {
    let mut notice = proxy_error.map(|error| format!("账号「{}」代理不可用，本次直连: {error}", account_display(&account)));
    let egress = crate::server::core::proxies::AccountEgress::from_json(&egress);
    RouteTarget {
        provider: provider_of(&account).to_string(),
        account_id: Some(account_id),
        priority: priority_of(&account),
        account: Some(account),
        egress,
        proxy_notice: notice,
    }
}
```

三处调用（`rotate.rs:160/234/242`）把 `entry.proxy` 改成 `entry.egress`。

- [ ] **Step 4: 会话携带 `egress`**

`core/account_store/state.rs`：`SessionById` 增加字段 `pub egress: Value,`（放在 `pub proxy: Value,` 之后）。

`core/account_store/store.rs`：

- `session_from_record` 里把

```rust
        let resolution = resolve_account_proxy(Some(&record.proxy()));
        let (proxy, proxy_error) = split_resolution(resolution);
```

改为

```rust
        let (proxy, egress, proxy_error) = split_egress(resolve_account_egress(Some(&record.proxy())));
```

并在 session 的 json! 里，`"proxy": proxy,` 之后加 `"egress": egress,`。

- `get_session_by_id` 里同样改为 `let (proxy, egress, proxy_error) = split_egress(resolve_account_egress(Some(&record.proxy())));`，并在 `SessionById { ... }` 里加 `egress,`。

- 加辅助函数（与 `split_resolution` 相邻）：

```rust
/// 出口解析结果拆成 `(proxy, egress, proxyError)`：Single 时 proxy 与 egress 同形，Rotate 时 proxy 为 null。
fn split_egress(resolution: crate::server::core::proxies::EgressResolution) -> (Value, Value, Option<String>) {
    use crate::server::core::proxies::{AccountEgress, EgressResolution};
    match resolution {
        EgressResolution::Resolved(egress) => {
            let json = egress.to_json();
            let proxy = match &egress { AccountEgress::Single(_) => json.clone(), _ => Value::Null };
            (proxy, json, None)
        }
        EgressResolution::Failed(message) => (Value::Null, Value::Null, Some(message)),
    }
}
```

顶部 import 增加 `resolve_account_egress`。

- [ ] **Step 5: provider_loop 传 `egress`**

`core/upstream/provider_loop.rs:879`：`proxy: target.proxy.clone(),` → `egress: target.egress.clone(),`

- [ ] **Step 6: custom 链路改 `AccountEgress`**

`core/account_store/custom_accounts.rs`：
- import 改 `use crate::server::core::proxies::{resolve_account_egress, AccountEgress};`
- `CustomCredential` 的 `pub proxy: Option<ResolvedProxy>,` → `pub egress: AccountEgress,`
- `credential_of_record` 里：

```rust
    let egress = match resolve_account_egress(Some(&record.proxy())) {
        crate::server::core::proxies::EgressResolution::Resolved(egress) => egress,
        crate::server::core::proxies::EgressResolution::Failed(_) => AccountEgress::Direct,
    };
```

（并把 `CustomCredential { ... proxy, }` 改为 `egress,`。）

`core/providers/custom/forward.rs`：
- import 改 `use crate::server::core::proxies::AccountEgress;`
- `forward(..., proxy: Option<ResolvedProxy>, ...)` → `egress: AccountEgress`
- `forward_translated(..., proxy: Option<ResolvedProxy>, ...)` → `egress: AccountEgress`
- 两处 `TransportRequest { url, headers, payload, proxy }` → `{ url, headers, payload, egress }`
- 调用方（`forward` 的入口，取 `credential.proxy`）→ 传 `credential.egress`

- [ ] **Step 7: 编译并跑全量测试**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server`
Expected: 编译通过；既有测试全绿（不轮询路径行为不变）。

- [ ] **Step 8: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/core/upstream desktop-tauri/src-tauri/server/src/server/core/account_store desktop-tauri/src-tauri/server/src/server/core/providers/custom
git commit -m "feat(upstream,custom): carry AccountEgress through chat paths"
```

---

### Task 9: API 分组摘要 + 前端

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/api/proxies.rs`、`desktop-tauri/ui/*`

**Interfaces:**
- Produces: `GET /api/proxies/pool` 顶层 `groups: [{name, count}]`；前端可配分组与轮询。

- [ ] **Step 1: 池列表加 `groups` 摘要**

`api/proxies.rs` 的 `pool_payload`：在 `json!({ "items": items, "clash": ... })` 里加 `groups`：

```rust
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for item in proxy_pool::list() {
        if let Some(group) = item.get("group").and_then(Value::as_str).filter(|g| !g.is_empty()) {
            *counts.entry(group.to_string()).or_insert(0) += 1;
        }
    }
    let groups: Vec<Value> = counts.into_iter().map(|(name, count)| json!({"name": name, "count": count})).collect();
```

并在返回 json 里加 `"groups": groups,`。

- [ ] **Step 2: 手工验证接口**

Run: `curl -s http://127.0.0.1:3065/api/proxies/pool -H 'x-panel-token: <token>' | jq '.data.groups'`
Expected: 数组（当前池条目无分组则为 `[]`）。

- [ ] **Step 3: 前端（网络代理页 + 账号出口选择器）**

- 池条目表单：新增「分组」文本输入，提交时带上 `group`。
- 账号出口选择器：新增「轮询」模式 —— 策略下拉（round-robin/random/least-latency）、「失败自动换出口」开关、成员选择（多选池条目或填分组名），保存 `{source:"pool-rotate", group, proxyIds, strategy, onError}`。
- 复用 `GET /api/proxies/pool` 的 `items` + `groups` 渲染选项。

- [ ] **Step 4: Commit**

```bash
git add desktop-tauri/src-tauri/server/src/server/api/proxies.rs desktop-tauri/ui
git commit -m "feat(ui,api): rotation group field + proxy picker rotation mode"
```

---

### Task 10: 端到端验证（Kilo 账号接轮询组）

**Files:** 无（验证）

- [ ] **Step 1: 给现有 10 条 Kilo 代理打分组**

对每条池条目调 `/api/proxies/pool/update` 带上 `"group":"kilo"`。

- [ ] **Step 2: 给 Kilo 账号绑轮询**

`PATCH /api/accounts/custom-acct-70d17b624e34`，body：

```json
{"proxy":{"source":"pool-rotate","group":"kilo","strategy":"round-robin","onError":"next"}}
```

- [ ] **Step 3: 连发请求并观察出口**

```bash
for i in $(seq 1 10); do
  curl -s http://127.0.0.1:3065/v1/chat/completions \
    -H "Authorization: Bearer sk-a2a-52cd10e462112aa04421f52af2246c30" \
    -H 'Content-Type: application/json' \
    -d '{"model":"kilo-auto/free","messages":[{"role":"user","content":"ok"}],"max_tokens":5}' \
    -o /dev/null -w "%{http_code}\n"
done
```

在运行日志里核对每条的「本次出口」不同（轮询生效）。

- [ ] **Step 4: 失败换出口验证**

把首选成员临时改成一个连不上的地址，确认请求仍成功（自动换下一个出口）。

- [ ] **Step 5: 记录结果并收尾**

在 `docs/release-notes/`（本地）记一句验证结论；如需发版按 `agent.md` 流程走（需人工确认）。

---

## Self-Review

- **Spec coverage**：配置模型→Task 1/2/4；解析与选择→Task 5/6；分发与失败重试→Task 7/8；接入范围→Task 8（两条聊天路径）；API/前端→Task 9；测试→各 Task + Task 10。全部覆盖。
- **Placeholder scan**：无 TBD/TODO；每个代码步骤含实际代码。
- **Type consistency**：`AccountEgress`/`RotationPlan`/`Strategy`/`OnError`/`EgressResolution` 在 Task 1/5 定义，Task 6/7/8 一致引用；`RouteTarget.egress`/`TransportRequest.egress`/`SessionById.egress`/`CustomCredential.egress` 命名一致。
- **Review Focus 覆盖**：①→Task 5（空成员 Failed）；②→Task 3（跳过解析失败/禁用）；③→Task 7（`onError=none` limit=1）；④→Task 7（拿到响应即返回）；⑤→Task 1（`from_json` 回退 Direct）；⑥→Task 6（失败惩罚 EWMA）。

## 与 spec 的两处偏差（实现口径，已确认可接受）

1. `least-latency` 的 EWMA 初值用**首个观测样本**，未接池条目 `lastTest.durationMs`（v1 简化，可后续补）。
2. 轮询状态键 `group_key` 用**成员端点**（`协议://主机:端口`）而非池条目 id（`ResolvedProxy` 不携带 id；端点唯一且稳定）。

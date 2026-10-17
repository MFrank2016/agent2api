# 代理池轮询（Proxy Pool Rotation）设计

- 状态：待评审
- 日期：2026-10-09
- 作者：AI Agent（对话式头脑风暴产出）
- 相关代码：`desktop-tauri/src-tauri/server/src/server/core/{proxies.rs, proxy_pool.rs, egress.rs}`、`core/upstream/{mod.rs, request.rs, rotate.rs}`、`core/providers/custom/forward.rs`、`core/account_store/*`、`api/proxies.rs`、`ui/*`

> 本文档按仓库约定放在 `docs/`（该目录已 gitignore，只在本机留存，不入库）。

---

## 1. 背景与目标

agent2api 的账号可以绑定**一个**出网代理（`core::proxies`），所有出网请求经 `core::egress::client_for(proxy)` 走该出口。账号的 `proxy` 字段目前是单引用：`{source:"pool", proxyId}` / `{source:"clash", listenerUid}` / `{source:"custom", host,port,…}`。

**目标**：让**一个账号**能把出网请求分摊到**代理池里的多个出口 IP**，策略含：

- `round-robin`（轮询）
- `random`（随机）
- `least-latency`（低延迟优先）

并在某出口**连接失败**时**请求内自动换下一个出口重试**（透明，对上层与账号降级链无感）。

**动机（首个使用者）**：Kilo（`api.kilo.ai`）匿名免费模型限速 **200 请求/小时/IP**。轮询出口可成倍抬高上限，并在某个出口被封/超时时自动切换。

**非目标（v1 不做）**：

- 有状态提供商（catpaw / qoder / accio / codearts / trae / kuku）的转发轮询。
- 余额查询、登录、模型刷新等辅助出网点的轮询（仍走单出口）。
- 出口健康度主动探测/后台探活（仅靠请求内失败惩罚 + 池条目 `lastTest` 作初值）。
- 出口级限速、按权重分配、跨进程/持久化轮询状态。

---

## 2. 现状（代码事实）

- **唯一出网点**：`core::egress::client_for(Option<&ResolvedProxy>) -> Arc<reqwest::Client>`，按 `协议://主机:端口|connect_ms|read_ms` 缓存 Client（LRU 24）。
- **账号出口解析**：`core::proxies::resolve_account_proxy(config) -> Option<ProxyResolution>`，`ProxyResolution = Resolved(ResolvedProxy) | Failed(String)`。
- **账号入库**：`normalize_account_proxy(input) -> Result<Option<Value>, ProxyConfigError>` 校验并存标准形态。
- **代理池**：`core::proxy_pool`（kv 键 `proxyPool`），条目 `{id, name, source, enabled, protocol, host, port, username, password, createdAt, updatedAt, lastTest}`；账号按 `{source:"pool", proxyId}` 引用。
- **选路**：`upstream::rotate::with_proxy_notice` 把会话里的 `proxy`（已解析的 `ResolvedProxy` JSON）再解析进 `RouteTarget.proxy: Option<ResolvedProxy>`。
- **转发**：
  - 通用无状态：`upstream::request::send_chat_request(plan)` → `egress::client_for(plan.proxy)` → `builder.send()`。
  - 自定义提供商：`providers::custom::forward(..., proxy: Option<ResolvedProxy>, ...)` 同款。
- **回退口径**：代理解析失败 → **本次回退直连**并把原因挂 `proxy_notice`；`ResolvedProxy::from_json` 失败同理。

---

## 3. 配置模型

### 3.1 代理池条目新增 `group`

- 字段：`group: String`，`clean_string(≤60)`，缺省 `""`（未分组）。
- 只影响**手动**条目；Clash 同步来的条目 `group` 恒为 `""`。
- `proxy_pool::normalize_item` 读写它；`describe_item` 回显它。
- `GET /api/proxies/pool` 额外返回 `groups: [{name, count}]`（去重分组摘要，供前端下拉）。

### 3.2 账号 `proxy` 新增来源 `pool-rotate`

```json
{
  "source": "pool-rotate",
  "group": "kilo",                 // 可选：按分组标签选成员
  "proxyIds": ["px_...", "px_..."], // 可选：按池条目 id 选成员
  "strategy": "round-robin",        // round-robin(默认) | random | least-latency
  "onError": "next"                 // next(默认，失败换出口) | none
}
```

**成员集合** = `group` 命中的条目 ∪ `proxyIds` 命中的条目，要求条目 `enabled`，按池内顺序去重（先 group 后 proxyIds，各自按池顺序）。

**校验（`normalize_account_proxy`）**：

- `group`：`clean_string(≤60)`。
- `proxyIds`：字符串数组，逐项 trim 非空，去重，长度 ≤ 64；非数组/含空项 → 400。
- `strategy`：小写归一，缺省 `round-robin`；非三选一 → 400。
- `onError`：小写归一，缺省 `next`；非 `next|none` → 400。
- `group` 与 `proxyIds` 至少一个非空，否则 400「轮询出口缺少成员（group 或 proxyIds）」。

---

## 4. 解析与选择

### 4.1 统一出口类型

`core::proxies` 新增：

```rust
pub enum AccountEgress {
    Direct,                 // 无代理（等价现状 None）
    Single(ResolvedProxy),  // 现状 Some(proxy)
    Rotate(RotationPlan),   // 新增
}

pub struct RotationPlan {
    pub members: Vec<ResolvedProxy>, // 已解析、已启用、已去重
    pub strategy: Strategy,          // RoundRobin | Random | LeastLatency
    pub on_error: OnError,           // Next | None
    pub group_key: String,           // 轮询/EWMA 状态键 = 成员 id 排序后拼接
}
```

新增 `resolve_account_egress(config) -> EgressResolution`（`Ok(AccountEgress) | Failed(String)`）：

- 非 `pool-rotate` → 复用现有 `resolve_account_proxy` 结果（`Direct` / `Single` / `Failed`）。
- `pool-rotate` → 经 `proxy_pool::members_for(group, proxy_ids)` 解析成员；成员为空 → `Failed("轮询组没有可用出口")`（转发层回退直连，与现状一致）。

保留 `resolve_account_proxy` 作为单出口内核；`resolve_account_egress` 在其上包一层。

### 4.2 会话携带

账号存储组装会话时改用 `resolve_account_egress`：

- `Direct` / `Single`：写入会话 `proxy` 字段，**与现状逐字一致**（其他消费者不受影响）。
- `Rotate`：写入会话**新增** `egress` 字段（序列化的 `AccountEgress`），`proxy` 置 `null`。
- 解析失败：沿用 `proxyError`。

新增 `session_egress(session) -> AccountEgress`：

- 有 `egress` → `AccountEgress::from_json`；
- 否则由 `proxy` 得 `Direct`/`Single`（复用 `ResolvedProxy::from_json`）。

转发层与 `with_proxy_notice` 改读 `session_egress`。

### 4.3 选择策略（`RotationPlan::candidates() -> Vec<ResolvedProxy>`）

返回**本次请求的候选顺序**（第一个是首选，其后为 `onError=next` 时的重试顺序）：

- `round-robin`：全局游标 `cursor`（按 `group_key`），从 `cursor % n` 起按环形顺序排列；取用后 `cursor += 1`。
- `random`：Fisher–Yates 洗牌（随机源见下）。
- `least-latency`：按成员 EWMA 升序（小者优先）；未知者以乐观值（`0`）参与，先被探测。

随机源：项目无 `rand` 依赖，沿用既有手法（`getrandom` 或 `RandomState` 哈希取随机）。

### 4.4 进程内状态

新模块 `core::egress_rotation`（或 `egress/rotation.rs`）：

```rust
static STATE: Mutex<HashMap<String /*group_key*/, RotationState>>;
struct RotationState {
    cursor: u64,
    ewma: HashMap<String /*成员 协议://主机:端口*/, f64>, // 毫秒
}
```

- 键 = `group_key`（成员 id 集合），使**同组多账号共享**轮询游标与 EWMA（摊得更均）。
- **仅内存**，重启清零；EWMA 初值取池条目 `lastTest.durationMs`（无则缺失）。
- EWMA 参数：`α = 0.3`。
- 失败惩罚：连接失败时以「连接超时值」作为一次延迟样本（拉高 EWMA），使后续请求倾向别的成员。

---

## 5. 分发与失败重试

`core::egress` 新增：

```rust
pub async fn dispatch(
    egress: &AccountEgress,
    build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error>;
```

行为：

- `Direct` → `client_for(None).send()`。
- `Single` → `client_for(member).send()`。
- `Rotate` → 取 `candidates()`：
  - 逐个 `client_for(member)` 发；
  - **仅当** `.send()` 返回连接/传输错误（DNS、TCP、TLS、代理鉴权失败、连接超时）→ 换下一个；
  - **一旦拿到响应**（含 4xx/5xx）→ 立即停手并返回（那是真实上游回答，交回既有账号降级链处理）；
  - `onError = none` → 只试首选一个；
  - 全部失败 → 返回最后一个错误。
- 成功（拿到响应头）→ 记一次延迟样本（仅 `least-latency` 用）。

**接入点（仅两条聊天路径）**：

1. `core/upstream/request.rs::send_chat_request` —— `plan.proxy` 改为 `plan.egress: AccountEgress`，用 `dispatch` 替换 `client_for(...).send()`。`via` 文案按 `AccountEgress` 生成。
2. `core/providers/custom/forward.rs` —— 入参 `proxy: Option<ResolvedProxy>` 改为 `egress: AccountEgress`，用 `dispatch`。

`RouteTarget.proxy` / `TransportRequest.proxy` / custom forward 的 `proxy` 参数统一替换为 `AccountEgress`；`with_proxy_notice` 产出 `AccountEgress`。

**范围明确**：余额/登录/模型刷新/oauth/welfare 等出网点**不动**；有状态提供商**不接**（其 `client_for(proxy)` 调用保持单出口语义，`session_proxy` 仍可用）。

---

## 6. API 变更

- `POST /api/proxies/pool`、`/pool/update`：接受并回显 `group`。
- `GET /api/proxies/pool`：`items[].group` + 顶层 `groups: [{name, count}]`。
- `POST /api/proxies`（账号出口测试）：无需改（`normalize_account_proxy` 已能校验 `pool-rotate`；测试仍测单出口）。
- 账号增删改（`/api/accounts*`）：`proxy` 字段校验经 `normalize_account_proxy`，自动支持 `pool-rotate`。

---

## 7. 前端变更

- **网络代理页**：池条目表单新增「分组」输入；列表显示分组；提供分组下拉/过滤。
- **账号出口选择器**（`ui/proxy-form.js` 等）：新增「轮询」模式：
  - 策略下拉（round-robin / random / least-latency）；
  - 「失败自动换出口」开关（`onError`）；
  - 成员选择：多选池条目，或填/选分组名；
  - 保存时提交 `{source:"pool-rotate", …}`。

---

## 8. 兼容性与安全

- **不轮询的账号**：`Direct`/`Single` 路径行为逐字不变（会话 `proxy` 字段不变，`client_for` 路径不变）。
- **密码**：沿用 `ResolvedProxy` 的 describe 口径，`egress` 序列化不含明文到日志/HTTP 响应；`egress` 存会话（本机配置目录，与现状同级）。
- **`client_for` 缓存**：不变；轮询只是用不同成员调用它，LRU 24 足够容纳池成员。
- **账号降级链**：`dispatch` 只在所有成员都连不上时返回错误 → 账号循环仍按「一次尝试失败」处理，语义不被轮询打乱。

---

## 9. 错误处理

- 成员为空 / 全部条目不存在或被禁用 → `resolve_account_egress` 返回 `Failed` → 会话 `proxyError` + 转发层回退直连（现状口径）。
- 部分成员坏（端口非法等）→ 解析时跳过该成员；若跳过到 0 个 → 同上。
- 单次请求全部成员连不上 → 返回最后一个连接错误（文案含「已尝试 N 个出口」）。

---

## 10. 测试计划

**单元测试**（`core::proxies` / `core::proxy_pool` / `core::egress_rotation`）：

- `normalize_account_proxy`：合法 `pool-rotate` 归一；非法 strategy/onError/空成员被拒。
- 成员集合：group ∪ proxyIds 去重、保序、过滤 disabled。
- 选择：round-robin 连续 n 次覆盖全成员并回到起点；random 多次覆盖全部；least-latency 选 EWMA 最小者、失败惩罚后改选。
- `candidates()` 顺序：least-latency 升序、round-robin 环形、random 洗牌。

**集成/端到端**：

- 失败重试：构造一个「首个成员必连不上」的 `AccountEgress`，断言 `dispatch` 落到第二个成员（可注入测试用 build 闭包 / 本地不可达端口）。
- Kilo 账号绑一个 10 成员轮询组，连发 N 次，读运行日志「本次出口」行，断言出现多个不同出口 IP。
- 回归：不轮询账号的请求路径与日志逐字不变（现有测试全绿）。

---

## 11. 分阶段

1. **数据层**：`group` 字段 + `pool-rotate` 校验 + `members_for`（含单测）。
2. **解析层**：`AccountEgress` / `RotationPlan` / `resolve_account_egress` / `session_egress`（含单测）。
3. **分发层**：`egress::dispatch` + `egress_rotation` 状态（含单测）。
4. **接线**：`send_chat_request` + `custom::forward` 改用 `dispatch`；`with_proxy_notice` 产出 `AccountEgress`。
5. **前端**：池分组 + 账号轮询选择器。
6. **端到端**：Kilo 账号接 10 成员轮询组验证。

---

## 12. 已定决策（原待定项）

- 轮询配置实体：**账号级轮询源（A）+ 池分组（B）** 二者结合 —— 账号可引用分组、也可列举条目。
- 失败换出口深度：**请求内透明重试**（仅在未收到响应字节时换）。
- 接入范围：**`upstream`（通用无状态）+ `custom`（所有自定义提供商，含 OpenCode Zen / Kilo）两条聊天路径**。
- 轮询/EWMA 状态：**进程内存**，重启清零。

## 13. 后续（v1 之后）

- 有状态提供商接入轮询。
- 出口健康度后台探活 + 冷却（连续失败 N 次临时摘除）。
- 持久化轮询游标 / 跨重启连续性。
- 出口权重、按出口限速。

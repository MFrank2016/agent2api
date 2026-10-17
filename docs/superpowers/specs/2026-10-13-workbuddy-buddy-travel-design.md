# WorkBuddy「派猫猫旅行」接入设计

- 状态：待评审
- 日期：2026-10-13
- 作者：AI Agent（对话式头脑风暴产出）
- 相关代码：`desktop-tauri/src-tauri/server/src/server/core/billing/{request.rs, mod.rs, travel.rs(新), checkin.rs}`、`core/auto_checkin.rs`、`config/types.rs`、`api/{accounts.rs, checkin_center.rs}`、`ui-islands/src/islands/{checkin-page.tsx, checkin-state.ts}`

> 本文档按仓库约定放在 `docs/`（该目录已 gitignore，只在本机留存，不入库）。

---

## 1. 背景与目标

WorkBuddy 国内版客户端有一个「派猫猫旅行」运营活动：派 Buddy 出门旅行（`depart`），旅行 1~4 小时后到达（`arrived`），到达后领取积分（`claim`，随机 5~10 分），每天有派发次数上限。该功能在社区项目 `kuzen-so/workbuddy-checkin`（分支 `optimized-v3`）里以脚本形式实现。

**目标**：把「派猫猫旅行」接入 agent2api，作为 WorkBuddy **国内版**账号每日签到链的一部分：

- **并入每日自动签到**：与现有每日签到同一调度体系，不新开独立任务。
- **每日两轮**：第一轮（现有签到时刻）负责「领取残留 + 派发」；第二轮（新增领取时刻）负责「领取当天到达的行程」，保证当天闭环。
- **签到中心展示**：每个 WorkBuddy 国内版账号在签到中心多一张旅行卡片（状态 / 倒计时 / 待领积分），并带手动「派发 / 领取」按钮。

**动机**：这是 WorkBuddy 国内版账号除签到外的第二条稳定收益链路（每天最多一次旅行收益），且实现只依赖同一个 accessToken，边际成本低。

**非目标（v1 不做）**：

- WorkBuddy **国际版**接入。实测国际版 host（`www.workbuddy.ai`）上该接口存在但返回空 `data`（无活动），CN host 对国际版 token 返回 401。v1 对国际版账号一律 no-op（不报错、不展示卡片）。
- 旅行信件（`letter`）的互动展示 / `use_deeplink` 跳转。
- 固定地点策略以外的花式玩法（仅支持「随机」或「固定 1 个地点」）。
- 把旅行状态塞进 `checkin_center` 聚合快照（那会破坏「打开页面零上游请求」的既有契约 —— 见 §5.5）。

---

## 2. 现状（代码事实，均为实测/源码核对）

### 2.1 计费调用层

- `call_billing(spec, options) -> Result<BillingCall, BillingError>`（`billing/mod.rs:196-381`）是唯一的计费请求内核：
  - host 恒取 `session.endpoint`（规范化后），缺失时回落 `auth.default_context().base_url`（`mod.rs:208-213`）；
  - URL = `{endpoint}{spec.path}{query}`（`mod.rs:214`）；
  - 头走 `build_headers(session, extra)`（`request.rs:169-207`），固定带 `Authorization: Bearer {accessToken}` 与 `X-User-Id`；
  - 非 GET/HEAD 一律带 body（`spec.body` 兜底 `{}`）；
  - 信封解包读 `{code, msg, requestId, data}`，`code != 0` 且 `expect_code_ok` → 抛错。
- `BillingSpec { method, path, body: fn()->Value, whitelist_headers }`（`request.rs:18-32`）**没有 host 字段**。
- `CallOptions<'a> { session, body, query, expect_code_ok, tolerate_duplicate_claim, locale }`（`request.rs:52-65`）**没有 host 字段**。
- **跳过 `/v2` 的先例**：`BILLING_ACTIVITY_CHECKIN_STATUS` 的 path 是不带 `/v2` 的字面量（`request.rs:114-119`）—— 说明「path 里带不带 `/v2`」由常量自己决定，没有 flag。

### 2.2 多步活动的先例

`billing/activity.rs::workbuddy_daily_activity`（`activity.rs:293-458`）是「探测 → 条件领取 → 保活」三段式的现成范式：所有上游失败都被收敛进结果字段 + verbose 日志，绝不打断调度；`{status, claim, activity}` 的结果形状。

### 2.3 签到分派与结果形状

- `checkin::checkin_for`（`checkin.rs:220-368`）按 `provider_of(account)` 分派；WorkBuddy 国内版落 `_ if provider_id == DEFAULT_PROVIDER_ID` 分支（`checkin.rs:340-354`），调 `billing.claim_daily_checkin`，再经 `claim_result` 收尾成 `{id, name, claim, error}`。
- `workbuddy-intl` 分支（`checkin.rs:279-314`）返回**多一个 `activity` 块**的 `{id, name, claim, activity, error}` —— 这是「结果行挂附加块」的现成先例。

### 2.4 定时签到

- `core/auto_checkin.rs`：`sleep(30s)` 轮询 + 当天去重（不是单定时器）。配置存 `config.json` 的 `autoCheckin`（`{enabled, time, providers, lastFiredDate, lastResult}`，`CONFIG_KEY="autoCheckin"`）。
- `CheckinState { enabled, time, providers, last_fired_date, last_result }`（`auto_checkin.rs:317-324`）；`read_state`（335-355）读内存快照；`write_state`（359-367）合并写回。
- `fire(reason)`（470-519）抢 `running` 标记 → 先落 `lastFiredDate` → `run_checkin` → `record_success`/失败分支。**每天只触发一次**（靠 `lastFiredDate` 去重）。
- 「自然日」固定北京时间（`core::beijing`，issue #138）。
- `autoCheckin` **不在** `scheduledTasks` 里（`config/types.rs:367-368` 明确：它是每天定点型，由 `core::auto_checkin` 自己管）。

### 2.5 签到中心聚合

`api/checkin_center.rs::get_center`（95-258）**零上游请求**：只从账号快照聚合。资格类状态（Loomy/小浣熊新手、CodeArts 福利、ZCode 套餐）**不进快照**，由前端按需惰性调 `/onboarding`、`/codearts-welfare/preview`、`/zcode-claim/preview`。响应形状见模块头注释。

### 2.6 手动 per-account 动作

`api/accounts.rs` 用 `rest.strip_suffix("/xxx")` 分派（251-309），已有 `/zcode-claim[/preview|/captcha-config]`、`/codearts-welfare[/preview]`、`/onboarding[/claim]` 先例。

### 2.7 全仓库现状

`travel` / `depart` / `派猫猫` / `www.workbuddy.cn` 在功能代码中**零引用**：`endpoints.rs:42-43` 的 `DOMAINS_INTERNAL`（含 `www.workbuddy.cn`）是 `#[allow(dead_code)]` 白名单；`login.rs:510` 的 `workbuddy.cn` 是登录窗口导航允许列表。两者都不是 API host 来源。

---

## 3. 接口契约（实测，2026-10-13）

### 3.1 Host（**按 edition 派生，不硬编码**）

| 账号版本 | Host | 实测 |
| --- | --- | --- |
| `workbuddy`（cn） | `https://www.workbuddy.cn` | 200 / `code:0` |
| `workbuddy-intl` | `https://www.workbuddy.ai` | 200 但 `data:{}`（无活动） |
| 交叉（CN token → intl host 等） | — | 401（APISIX 网关拒绝） |

> 关键：token 绑域名，host 必须与账号 edition 匹配。源项目硬编码 `www.workbuddy.cn` 是因为它只服务 CN。

### 3.2 端点（**不带 `/v2`**）

| 用途 | 方法 | Path | Body |
| --- | --- | --- | --- |
| 查状态 | GET | `/activity/growth/buddy/travel/status` | 无 |
| 查可选地点 | GET | `/activity/growth/buddy/travel/config` | 无 |
| 领取积分 | POST | `/activity/growth/buddy/travel/claim` | `{}` |
| 派发旅行 | POST | `/activity/growth/buddy/travel/depart` | `{"location_id": <int>}` |

请求头：`Authorization: Bearer <accessToken>`、`Accept: application/json`（POST 另带 `Content-Type: application/json`）。**无 user-id / 无 cookie / 无设备指纹**（官方 spec 原话：旅行接口仅需 Bearer Token）。

### 3.3 响应信封与字段

统一 `{code, msg, requestId, data}`（与 `BillingCall` 同构，可直接复用 `call_billing`）。

**status.data**（实测全字段）：

```json
{
  "state": "idle | traveling | arrived",
  "buddy_id": 10015773,
  "record_id": 17605981,
  "location": { "id": 4, "code": "ancient_town", "name": "古镇客栈", "duration_hours": 1, "cover_url": "" },
  "depart_at": 1791853200, "arrive_at": 1791856800, "server_now": 1791859391,
  "letter": { "id": 12, "text": "…", "guide_text": "…" },
  "use_deeplink": "workbuddy://discover?cardId=…",
  "daily_limit_reached": true,
  "duration_hours": 1,
  "reward_credit": 8
}
```

- 空闲态：`buddy_id=0, record_id=0, location=null, letter=null, arrive_at=0, reward_credit=0`。
- 旅行态：`state="traveling"`，`arrive_at > server_now`（倒计时 = `arrive_at - server_now`）。

**config.data**：`{ locations: [ { id, code, name, description, cover_url, duration_hours_min, duration_hours_max, reward_credit_min, reward_credit_max, sort, preview_photos[] } ] }`。

**claim.data**：`{ state:"idle", record_id, letter, reward_credit, use_deeplink }`（`reward_credit` 为本次到账数）。

**depart.data**：`{ location, arrive_at }`（源项目 api-spec 记录）。

### 3.4 状态机（复刻源项目，并修掉它的误导日志 bug）

```
GET status
  ├─ 请求失败 / code≠0 / data 空 → available=false，不做任何写操作
  ├─ state == "arrived"  → POST claim；成功后再 GET status 刷新
  ├─ daily_limit_reached 为真 → 跳过派发（"今日派遣次数已用完"）
  ├─ state ∈ {null,"", "idle"} 且未达上限 → GET config → 随机选 location_id → POST depart
  └─ 其它（traveling）→ 跳过（"Buddy 正在旅行中，无需派遣"）
最后再 GET status 作为结果
```

硬规则（源项目 spec 与实测一致）：

1. **`daily_limit_reached` 在判断 state 之前检查** —— 达上限时连 `idle` 也不派发。
2. **领取不受每日上限限制**（上限只管派发）—— 实测：`arrived` 且 `daily_limit_reached=true` 时 `claim` 仍成功。
3. **先领后派**（claim 在前，depart 在后）。
4. 已 `arrived` 的行程不会过期，下次运行照常领取。
5. **`location_id` 用具体值**（源项目 channel A 恒带 id；channel B 才发 `{}` —— 是否可选未证实，取稳的一边）。

---

## 4. 方案选型

### 4.1 跨域 host 怎么打（核心决策）

| 方案 | 说明 | 取舍 |
| --- | --- | --- |
| **A（推荐）** | 给 `CallOptions` 加 `base_url: Option<&'a str>`，`call_billing` 里 `options.base_url` 优先于 `session.endpoint` | 改动最小、纯 additive；复用整条信封/错误/头逻辑；`BillingSpec` 保持静态常量表不变 |
| B | 给 `BillingSpec` 加 host 字段 | `BillingSpec` 是 `const` 静态表，加字段会波及所有端点定义，且 host 是「调用期」信息而非「端点」信息，语义不合 |
| C | travel 独立请求 helper，绕过 `call_billing` 直接走 `auth_http::send_raw` | 要重抄头构造 + 信封解包 + 401/错误处理，重复代码且容易与主路径分叉 |

**推荐 A**。`CallOptions` 是逐调用构造的（非静态），承载「本次调用打到哪个 host」语义自然；且 `call_billing` 是 `billing/` 私有、子模块可直接调（`mod.rs:193-195`），travel 作为 `billing/` 子模块能无缝复用。

### 4.2 travel 模块放哪

不是 `ProviderAdapter` 的职责 —— 该 trait 只管转发/模型/用量（`providers/adapter.rs`），签到与活动逻辑本就在 `core/billing/*` 之外于 trait。新增 `core/billing/travel.rs`，形态对齐 `activity.rs`。

---

## 5. 设计

### 5.1 计费层：host 覆盖（唯一的基础设施改动）

- `request.rs`：`CallOptions` 增字段 `pub(super) base_url: Option<&'a str>`，`Default` 为 `None`。
- `mod.rs::call_billing`：endpoint 解析改为

  ```rust
  let endpoint = options
      .base_url
      .map(normalize_endpoint)
      .unwrap_or_else(|| {
          session.get("endpoint")... .unwrap_or_else(|| normalize_endpoint(default_base))
      });
  ```

- **现有调用点全部不传 `base_url`（`None`）→ 行为逐字不变。**

### 5.2 新模块 `core/billing/travel.rs`

- 常量（不带 `/v2`）：
  - `TRAVEL_CN_BASE = "https://www.workbuddy.cn"`
  - `TRAVEL_INTL_BASE = "https://www.workbuddy.ai"`
  - `TRAVEL_STATUS_PATH` / `TRAVEL_CONFIG_PATH` / `TRAVEL_CLAIM_PATH` / `TRAVEL_DEPART_PATH`
- host 解析：`travel_base_for_edition(session) -> Option<&'static str>` —— `edition == "intl"` → INTL，否则 CN。
- 归一化结构：

  ```rust
  pub struct TravelStatus {
      pub available: bool,          // 请求成功且有 data
      pub state: String,            // idle | traveling | arrived | ""
      pub reward_credit: i64,
      pub daily_limit_reached: bool,
      pub location: Option<Value>,
      pub arrive_at: i64,
      pub server_now: i64,
      pub letter: Option<Value>,
      pub use_deeplink: String,
  }
  ```

- 函数：
  - `get_status(session) -> Result<TravelStatus, BillingError>`
  - `get_config(session) -> Result<Value, BillingError>`
  - `claim(session) -> Result<BillingCall, BillingError>`
  - `depart(session, location_id: i64) -> Result<BillingCall, BillingError>`
  - `travel_auto(session, opts: TravelOpts) -> Value` —— 状态机（§3.4），返回 `{available, state, claimed, reward_credit, location, message}`；**所有失败收敛进字段**（对齐 `activity.rs` 的纪律）。
- 复用 `call_billing`，`CallOptions { session, base_url: Some(base), expect_code_ok: false, ..Default }`。

### 5.3 结果形状与结果行

- WorkBuddy 国内版分支（`checkin.rs:340-354`）：`claim_result` 之后附加一个 `travel` 块：

  ```json
  { "id", "name", "claim", "travel": { "available", "state", "claimed", "reward_credit", "location", "message" }, "error" }
  ```

  形状与 `workbuddy-intl` 的 `activity` 块同款。
- **旅行失败绝不影响签到成功判定**：`claim`（签到）与 `travel` 各自独立；旅行失败只体现在 `travel` 块，不写进 `claim`、不落 `checkinAt`。

### 5.4 调度：每日两轮

在 `autoCheckin` 下新增 `travel` 子对象（**不**进 `scheduledTasks`，与 `autoCheckin` 同处）：

```json
"autoCheckin": {
  "enabled": true, "time": "00:01", "providers": [...],
  "lastFiredDate": "...", "lastResult": {...},
  "travel": {
    "enabled": false,
    "claimTime": "06:00",
    "locationId": null,
    "lastDepartDate": null,
    "lastClaimDate": null
  }
}
```

- **两轮调用完全相同的 `travel_auto`**（幂等，见 §3.4 状态机），唯一区别是触发时刻与去重键：
  - **第一轮**（现有 `time`，默认 00:01）：签到执行后，对每个「CN WorkBuddy 且 enabled」账号跑 `travel_auto`，落 `lastDepartDate`。此时账号多为 `idle`，效果是**派发**；若昨夜残留 `arrived` 也会顺带领取。
  - **第二轮**（`claimTime`，默认 06:00）：对 CN WorkBuddy 账号再跑一次 `travel_auto`，落 `lastClaimDate`。此时当天派发的行程已到达，效果是**领取**；领完 `daily_limit_reached=true` → 不再派发。
- **「先领后派」规则使两轮可安全共用同一函数**：第二轮即便账号仍 `idle`（第一轮没派出去），也只是再派一次，不会重复计分。
- 两轮**各自独立去重**（`lastDepartDate` / `lastClaimDate`），沿用现有「轮询 + 当天去重 + 启动补跑」语义。
- `travel.enabled=false`（默认）→ 调度里完全不碰旅行，老配置零影响。
- **不新增独立 tokio 任务**：在现有 `tick()` 里判定两个时间点（`time` 与 `claimTime`），各自用 `lastFired`/`lastDepart`/`lastClaim` 去重。`TICK_MS` 不变。

### 5.5 API

- **只读（供卡片惰性查询，保持 `checkin_center` 零上游）**：
  `GET /api/accounts/{id}/travel` → `{ status: TravelStatus, config: {locations}, auto: {…travel 配置…} }`（`accounts.rs` strip_suffix 分派，仿 `/onboarding`）。
- **手动动作**：`POST /api/accounts/{id}/travel` → 跑一次 `travel_auto`，返回结果块（仿 `/zcode-claim`）。请求体可选 `{ "locationId": N }`。
- **`checkin_center` 不改数据形状**：仅在 `daily.providers[].accounts[]` 行保留现有字段；前端对 WorkBuddy 国内版行**按需**调 `GET .../travel` 拿状态。理由：快照必须「打开页面零上游」（`checkin_center.rs:11-15`）。

### 5.6 前端（签到中心）

- `checkin-state.ts`：新增 `fetchTravel(id)` / `runTravel(id)` action。
- `checkin-page.tsx`：WorkBuddy 国内版账号行内（或展开区）加**旅行卡片**：
  - 状态徽章（空闲 / 旅行中 / 已到达）；
  - 旅行中显示倒计时（`arrive_at - server_now`，用本地时钟递增）；
  - 已到达显示「可领 N 积分」+「领取」按钮；
  - 空闲显示「派发」按钮（`travel.enabled` 时）；
  - 达上限显示「今日已用完」。
- i18n：`ui/i18n/*/checkin.js` 与 `i18n-src/*/checkin.json` 六语补词条。

---

## 6. 配置模型

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `autoCheckin.travel.enabled` | bool | `false` | 总开关；关=调度与卡片按钮都不启用（卡片仍可显示只读状态） |
| `autoCheckin.travel.claimTime` | `"HH:MM"` | `"06:00"` | 第二轮领取时刻（北京时间） |
| `autoCheckin.travel.locationId` | `null \| 1..N` | `null` | 固定地点；null=每次随机（N=config 返回的地点个数，实测为 4） |
| `autoCheckin.travel.lastDepartDate` | string | — | 第一轮当日去重键（内部） |
| `autoCheckin.travel.lastClaimDate` | string | — | 第二轮当日去重键（内部） |

- `claimTime` 复用 `normalize_time`（`auto_checkin.rs:235`）校验，非法回落默认。
- `locationId` 校验为 `null` 或 1..N（N 由 config 地点数决定；越界回落随机）。
- 缺 `travel` 键 = 关闭（向后兼容，绝不因升级突然多打请求）。

---

## 7. 兼容性与安全

- **host 覆盖纯 additive**：现有计费调用不传 `base_url`，URL/头/信封路径逐字不变。
- **老配置零影响**：`autoCheckin.travel` 缺失即关闭。
- **token 绑域名**：travel 调用复用账号 `accessToken`（`build_headers` 已带 Bearer），host 按 edition 选；CN token 打 intl host 会 401 → 视作失败收敛，不重试。
- **不新增越权**：仍是同一账号 token 打官方接口；与现有签到同一合规口径（README「使用声明」已覆盖）。
- **凭证不出本机**：travel 不新增落盘；不写 token 到日志（沿用 `sanitize`/现有日志口径）。
- **`call_billing` 私有性**：travel 作为 `billing/` 子模块可直接调；`api/` 层仍只走语义化方法。

---

## 8. 错误处理

- 状态查询失败 / `code≠0` / `data` 为空（含 intl）→ `available=false`，**不做任何写操作**，记 verbose 日志。
- `claim`/`depart` 失败 → 收敛进 `travel.message`，不抛到 `checkin_for` 顶层，不影响签到行。
- 单账号旅行失败不影响同轮其它账号（沿用 `run_checkin` 的串行 + 单条容错）。
- 达上限 / traveling → 正常跳过（中性结果，不算失败）。

---

## 9. 测试计划

**单元测试**（`core/billing/travel.rs`，用注入的 mock 响应）：

- 状态机四态：`arrived`→claim→刷新；`idle`→depart（随机/固定地点）；`daily_limit_reached`→跳过派发；`traveling`→跳过。
- 边界：`claim` 失败时**不**沿用源项目的误导文案（应如实标 `arrived`）。
- 空 `data`（intl）→ `available=false`，零写操作。
- `location_id` 越界 / config 无地点 → 跳过派发（不发请求）。

**配置/调度测试**：

- `normalize_time(claimTime)` 校验；缺 `travel` 键 → 关闭。
- 两轮去重：同一天第二次 tick 不重复触发；跨天重置。

**集成测试**（fixture，仿 `codearts/catalog_fixtures`）：

- 录制的 status/config/claim/depart 响应 → 断言 `travel_auto` 结果块形状。
- 回归：现有计费调用（不传 `base_url`）URL/头逐字不变；现有测试全绿。

**手动 e2e**（已在本机验证过读写均通）：

- 用真实 CN 账号跑 `GET .../travel` 与 `POST .../travel`，核对状态与积分。

---

## 10. 分阶段

1. **计费层**：`CallOptions.base_url` + `call_billing` host 覆盖（含回归测试）。
2. **travel 模块**：4 端点 + `TravelStatus` + `travel_auto` 状态机（含单测）。
3. **接线**：`checkin.rs` WorkBuddy 分支挂 `travel` 块。
4. **调度**：`auto_checkin` 两轮 + 配置读写 + 去重（含单测）。
5. **API**：`GET/POST /api/accounts/{id}/travel`。
6. **前端**：签到中心旅行卡片 + i18n。
7. **端到端**：真实 CN 账号验证当天派发 + 领取闭环。

---

## 11. 已定决策

- 接入方式：**并入每日自动签到 + 签到中心展示**（用户选定）。
- 两阶段调度：**每日两轮**（第一轮派发、第二轮领取）（用户选定）。
- 跨域 host：**方案 A（`CallOptions.base_url`）**。
- travel 模块位置：**`core/billing/travel.rs`**（非 ProviderAdapter）。
- 范围：**仅 CN**；intl no-op。
- 卡片数据：**惰性查询**（不塞进 `checkin_center` 快照）。

---

## 12. 后续（v1 之后）

- 国际版若有对应活动，补 host/活动探测。
- 旅行信件的互动展示 / `use_deeplink` 深链跳转。
- 地点偏好（按 `reward_credit_max` 择优、避开已解锁地点等）。
- 把旅行状态并入账号页余额/收益展示。

# Meoo Provider Spec

- **日期**：2026-11-02
- **目标仓库**：`ProvidersPlugin/Agent2Providers`（实现）+ `agent2api`（回调路由、图标、README）
- **状态**：待实现

## 1. 目标与范围

在 Agent2API 的提供商插件里新增 `meoo`（秒悟 Meoo）provider，能力对齐 `dsh-meoo`，并修掉调研中发现的缺陷。

**范围内：**

- OAuth PKCE 浏览器登录（`cli.compat` scope）
- 手动填写项目服务 AK（`meoo_project_service_v1.*`）
- 从旧 `~/.meoo-proxy/config.json` 导入登录态（尽力而为）
- OAuth access/refresh token 自动续期（单飞 + 比较再写）
- 项目选择 / 项目服务 AK 解析
- 模型目录（远程 `/models` + 静态兜底 + 缓存）
- OpenAI 兼容对话（流式、工具调用、视觉模型透传）
- 积分余额查询（`/account/points` + `/account/entitlement`）
- 四档错误分类
- **所有 Meoo 请求携带官方 CLI 头**：`X-Meoo-Client: cli`、`X-Meoo-CLI-Version`、`X-Meoo-Request-Id`
- 账号公开视图脱敏、身份去重、长期有效标记
- 网关 `/oauth/callback` 回调路由、图标、六份 README

**范围外：**

- 图片生成（`/meoo-ai/api/v1/services/aigc/image-generation/generation`）
- 签到 / 领取类（确认不存在每日签到）
- 独立项目管理 UI（通过重新登录或手动 AK 切换项目；另提供只读 `project.list` 管理动作）
- 版本号递增、tag、Release（按 `release-flow` 另行确认）

## 2. 上游事实

### 2.1 身份与 OAuth

- 站点：`https://meoo.com`
- OAuth 元数据：`https://meoo.com/.well-known/oauth-authorization-server`
- 授权：`GET /oauth/authorize`
- 换票：`POST /oauth/token`（`application/x-www-form-urlencoded`）
- 撤销：`POST /oauth/revoke`
- scope：`cli.compat`
- PKCE：`S256`；`state` 32 字节随机
- 官方 CLI client ID（与 `@aliyun-meoo/cli@0.5.6` 一致）：
  - 正式：`meoo_app_tdWlRbpvAxN1EjmZ8XICxdTh`
  - 测试：`meoo_app_SFNvmHUFNePxyyfGjCd9hyaE`
- 正式 client ID 只用于 `meoo.com` 与 `gray-meoo.aliyun-inc.com`；其他主机用测试 ID
- 回调：`http://127.0.0.1:<gateway_port>/oauth/callback`
- access token 默认 1 小时；refresh token 会轮换

### 2.2 控制面

基础地址 `https://meoo.com`，Bearer 为 OAuth access token：

```text
GET  /open/v1/cli-compat/account/identity
GET  /open/v1/cli-compat/projects?page_size=50
GET  /open/v1/cli-compat/projects/{urlId}/cloud/environment
POST /open/v1/cli-compat/projects/{urlId}/cloud
GET  /open/v1/cli-compat/account/points
GET  /open/v1/cli-compat/account/entitlement
GET  /open/v1/cli-compat/account/member-credits       # 团队版兜底
GET  /open/v1/cli-compat/account/member-entitlement   # 团队版兜底
```

- `identity`：`data.name`、`data.user_id` / `data.id`、`data.project_url_id`、`data.edition`、`data.tenant.role`
- `environment`：`data.meooProjectApiKey` 或 `data.instance.meooProjectApiKey`
- `points`：`data.account.availableBalance` / `totalGranted` / `totalConsumed` / `totalExpired`
- `entitlement`：`data.planCode` / `planName` / `benefits[]`

### 2.3 模型网关

```text
GET  https://api.meoo.host/meoo-ai/compatible-mode/v1/models
POST https://api.meoo.host/meoo-ai/compatible-mode/v1/chat/completions
Authorization: Bearer meoo_project_service_v1....
```

模型（官方 AI 服务文档，远程 `/models` 为准）：

| id                       | 名称              | 上下文 | 输出 | 图片 | 思考 |
| ------------------------ | ----------------- | -----: | ---: | ---- | ---- |
| `qwen3.8-flash`          | Qwen3.8-Flash     |   200k |  64k | ✓    | ✓    |
| `qwen3.8-max-0902`       | Qwen3.8-Max-0902  |   200k |  64k | ✓    | ✓    |
| `qwen3.8-max`            | Qwen3.8-Max       |   200k |  64k | ✓    | ✓    |
| `qwen3.7-max`            | Qwen3.7-Max       |   200k |  64k | ✗    | ✓    |
| `qwen3.7-plus`           | Qwen3.7-Plus      |   200k |  64k | ✓    | ✗    |
| `deepseek-v4-pro-0813`   | DeepSeek V4 Pro   |     1M | 384k | ✗    | ✓    |
| `deepseek-v4-flash-0731` | DeepSeek V4 Flash |     1M | 384k | ✗    | ✓    |
| `glm-5.2-fast-preview`   | GLM-5.2-Fast      |   200k |  64k | ✗    | ✓    |
| `glm-5.2`                | GLM-5.2           |   200k |  64k | ✗    | ✓    |
| `qwen3-vl-plus`          | Qwen VL           |   200k |  64k | ✓    | ✗    |

上下文/输出是兜底估计；远程 `/models` 给了 `context_length` / `max_input_tokens` / `max_output_tokens` 时以远程为准。

### 2.4 CLI 头（必须）

官方 CLI `apiClient` 对 `meoo.com` 的所有业务请求都带：

```text
Content-Type: application/json
X-Meoo-Client: cli
X-Meoo-CLI-Version: 0.5.6
X-Meoo-Request-Id: cli_req_<uuid>
Authorization: Bearer <token>
Accept: text/event-stream        # SSE
OneDay-App-Id: <projectUrlId>    # 项目级 API Key
```

本项目要求**模型网关请求也带**这三个 X-Meoo 头：

- `GET /models`：`Accept: application/json`
- `POST /chat/completions`：`Accept: text/event-stream, application/json`
- 控制面：`Accept: application/json`

`X-Meoo-CLI-Version` 默认 `0.5.6`，可用环境变量 `MEOO_CLI_VERSION` 覆盖；`MEOO_CLI_HEADERS=0` 可关闭（排障/合规兜底，默认开启）。`X-Meoo-Request-Id` 每个请求新生成，禁止复用。

## 3. 账号模型

### 3.1 secret

```json
{
  "accessToken": "...",
  "refreshToken": "...",
  "expiresAt": 1790000000000,
  "scope": "cli.compat",
  "openid": "...",
  "userId": "...",
  "username": "...",
  "projectUrlId": "...",
  "serviceAk": "meoo_project_service_v1....",
  "serviceAkProjectId": "...",
  "apiBaseUrl": "https://meoo.com",
  "gatewayBaseUrl": "https://api.meoo.host/meoo-ai/compatible-mode/v1",
  "allowCloudEnable": false
}
```

手动 AK 账号只有 `serviceAk` / `projectUrlId` / `serviceAkProjectId`，没有 OAuth 字段。

### 3.2 public

```json
{
  "name": "Meoo 秒悟 · <projectUrlId>",
  "uid": "<userId>",
  "userId": "<userId>",
  "projectUrlId": "<projectUrlId>",
  "tokenTail": "…abcd",
  "expiresAt": 1790000000000,
  "longLived": false
}
```

**绝不包含** `accessToken` / `refreshToken` / `serviceAk`。

### 3.3 identity

- OAuth：`meoo:<userId>:<projectUrlId>`
- 手动 AK：`meoo:project:<projectUrlId>`；没有项目时 `meoo:ak:<tail>`

一个项目 = 一个网关账号；同一 Meoo 账号的不同项目可以并存。

### 3.4 能力位

| 位                          | 值    | 理由                 |
| --------------------------- | ----- | -------------------- |
| `chat`                      | true  | OpenAI 兼容          |
| `stateful`                  | false | 一次 HTTP 一次回答   |
| `refresh`                   | true  | OAuth refresh_token  |
| `model_refresh`             | true  | `/models`            |
| `usage`                     | true  | `/account/points`    |
| `checkin`                   | false | 确认没有每日签到     |
| `claims`                    | false | 确认没有领取动作     |
| `web_login`                 | true  | OAuth PKCE           |
| `sse_model_rewrite`         | false | 网关回显客户端模型名 |
| `sse_strip_newline_chunks`  | false | 无保活换行帧         |
| `anonymous_default_session` | false | 必须账号             |
| `system_proxy_when_unset`   | false | 国内可直连           |
| `refresh_uses_account`      | true  | 目录依赖项目 AK      |
| `supports_default_model`    | false | 不认网关默认模型注入 |
| `env_credentials`           | false | 无环境变量旁路       |

### 3.5 账号行

```rust
AccountFeatures {
    usage: true,
    credit_detail: true,
    identifier: vec!["projectUrlId".into(), "userId".into()],
    expiry: vec!["expiresAt".into()],
    ..Default::default()
}
```

手动 AK 账号 `longLived: true`、`expiresAt: 0`。

## 4. 登录方式

### 4.1 浏览器 OAuth（methodIndex 0）

- 生成 PKCE + state，登记 pending（verifier / redirect_uri / apiBaseUrl / projectUrlId / allowCloudEnable，TTL 6 分钟）
- 返回 `LoginStart { url, flow=state, poll_interval_ms=0, external_browser=true }`
- 回调：`/oauth/callback?code=...&state=...`
- 换票 → identity → 项目 → 环境 AK → `/models` 验证 → `NormalizedAccount`
- 项目选择：`inputs.projectUrlId` > `identity.project_url_id` > 项目列表第一项；没有项目则失败
- 没有 AK 时：
  - `allowCloudEnable=true`：`POST /projects/{id}/cloud` 后重取
  - 否则：`LoginPoll::Failed`，提示去 Meoo 项目「云服务」开通
- 远程部署：浏览器回不到网关 loopback 时，前端“粘贴回调地址”走 `/api/session/login/callback`

### 4.2 项目服务 AK（methodIndex 1）

- 动态表单字段：`serviceAk`（secret）、`projectUrlId`（可选）、`name`（可选）
- `normalize_account` 读 `payload.inputs`（兼容平铺）
- 校验前缀 `meoo_project_service_v1.`；`meoo_ak_` 直接拒绝并说明是用户 API Key
- 用 `/models` 验证 AK；失败返回 401
- 不自动开通云服务

### 4.3 旧 meoo-proxy 导入（Desktop）

- 读取 `~/.meoo-proxy/config.json`（`$HOME` / `%USERPROFILE%`）
- 字段：`refreshToken`、`accessToken`、`apiBaseUrl`、`projectUrlId`、`serviceAk`、`serviceAkProjectId`
- `apiBaseUrl` 只允许 `https://meoo.com` / `https://gray-meoo.aliyun-inc.com`；否则拒绝
- 有有效 `serviceAk` 直接用；否则用 refresh token 换 access token 并解析 AK
- 默认不自动开通云服务

## 5. 凭证生命周期

- `ensure_token` 返回**项目服务 AK**（对话用），不是 OAuth access token
- `refresh_token`（401 后）强制刷新 OAuth + 重新解析 AK，返回新 AK
- `token_expiring`：OAuth access token 5 分钟内到期，或 AK 缺失/项目不匹配
- 单飞：`crate::flight::Table<String>`，key = `meoo:<account>:<sha256(refreshToken) 前 32 位>`
- leader 拿到锁后**重读账号**；别人已刷新则直接返回
- 写回走 `crate::store::save` 的比较再写；失败则重读并返回赢家
- refresh token 轮换时原子保存；`invalid_grant` → 401「登录已失效，请重新登录」
- **禁止** 401 无限重试；网关只重试一次，插件刷新只做一轮

## 6. 错误分类

| 上游                                       | 分类                | 处置                                                   |
| ------------------------------------------ | ------------------- | ------------------------------------------------------ |
| 401 `SERVICE_AK_MISSING` / `invalid_token` | `TokenExpired`      | 刷新 + 重新解析 AK，同账号重试一次                     |
| 401 `SERVICE_AK_FORMAT_INVALID`            | `Fatal(401)`        | 提示用 `meoo_project_service_v1.*`，不要用 `meoo_ak_*` |
| 403 `insufficient_scope` / client 被禁     | `Fatal(403)`        | 配置/合规问题，不换号                                  |
| 402 / 429 / 文案含积分不足/quota/credit    | `QuotaLimited(429)` | 冷却 + 换账号                                          |
| 400 / 422 内容策略文案                     | `ContentBlocked`    | 不罚账号，中性提示词重试一次                           |
| 404 模型不存在                             | `Fatal(404)`        | 透传                                                   |
| 5xx                                        | `Fatal(502/503)`    | 透传/重试由网关决定                                    |
| 其他                                       | `Fatal(status)`     | 透传                                                   |

## 7. 安全与合规

- 使用官方 CLI client ID / `X-Meoo-Client: cli` 属于**冒用官方客户端身份**；仅个人自用，公开产品化前必须确认 Meoo 条款或申请第三方 client。
- 所有 Meoo 请求必须走 `headers.rs`，禁止各处手拼；`MEOO_CLI_HEADERS=0` 提供关闸。
- `apiBaseUrl` / `gatewayBaseUrl` 必须 allowlist；旧配置不可信。
- 不自动开通云服务；必须 `allowCloudEnable=true`。
- 公开视图只留 `tokenTail`；日志/错误不出现 token/AK。
- OAuth `state` 一次性、TTL 6 分钟、逐字比对。
- 账号删除不撤销上游 token；如需撤销，调用 `/oauth/revoke`（管理动作，非默认）。

## 8. 网关侧改动

- 新增 public 路由 `GET /oauth/callback`（Meoo 专用，检查登录任务的 provider 是 `meoo`）
- 图标：`meoo` → `assets/providers/meoo.png`
- 六份 README：31→32 个 provider id，30→31 家真实 provider；加 Meoo 条目
- **不**新增 `ProviderKind` / `PROVIDERS` / `adapter_for` 实现；插件家自动出现在 `summary_json`

## 9. 验证

- 插件单测：PKCE、CLI 头、模型解析、错误分类、usage 形状、账号脱敏、AK 类型拒绝
- `cargo test -p agent2api-providers`
- `node scripts/verify.mjs --exe target/release/agent2api-providers`
- 网关：`cargo check -p agent2api-server`
- 实机：安装插件 → 手动 AK 账号 → `scripts/probe-provider.mjs` 九项
- 实机：浏览器 OAuth → 积分查询 → 模型刷新 → 换项目/第二个项目 → 401 刷新

# 旧镜像 vs 新镜像功能对比报告

- 日期：2026-10-31
- 旧镜像：`agent2api:local`（版本 3.1.0，容器 `agent2api`，端口 3065）
- 新镜像：`agent2api:beta6-verify`（版本 3.3.0-beta.6，容器 `agent2api-beta6-verify`，端口 13068）
- 新镜像来源：`beta` @ `ff865836`
- 对比方式：源码/路由/UI 文件对比 + 两个运行容器 API 实测 + 账号迁移抽样验证
- 说明：新旧镜像的账号数据不同，本文把“功能差异”和“账号抽样可用性”分开记录。

---

## 1. 结论摘要

| 维度 | 结论 |
|---|---|
| 架构 | 旧镜像内置 provider 实现；新镜像改为插件宿主 + 外部 provider pack |
| Provider 数量 | 旧 27 家；新镜像默认官方 pack 32 家（30 插件 + 2 web）；本地 33 家 pack 为 35 家 |
| 签到 | 核心 API/UI 不变；每日签到 provider 从 14 家扩到 21 家；新增 `checkinFeatures` 能力快照 |
| 代理 | 代理池轮换保留；旧镜像的代理订阅/内核在新镜像被 retired，`pool/provision` 也移除 |
| 路由 | 7 种策略保留，包括上游已移除、本仓 fork 保留的 `balance-weighted` |
| 数据库 | schema 17 → 22，需要迁移；账号导出/导入均为 v2，支持 `customProviders` |
| UI | 实际签到页/定时页/状态代码完全一致；新镜像新增的部分签到组件未接线 |
| 部署 | 旧镜像 root；新镜像非 root `65532:65532`；镜像约 159 MB → 209 MB |
| 许可证 | 旧 MIT；新 AGPL-3.0-only |
| 测试 | 旧网关基线 400 passed；新网关 398 passed / 0 failed / 1 ignored，另有插件 52 tests |

**最重要的风险：**
1. 新镜像默认官方 pack **不包含 Nous**；旧镜像有 Nous。只有本地 33 家 pack 才恢复 Nous。
2. 代理订阅/内核在新镜像被移除；依赖订阅节点或内核的用户需要重新评估。
3. `zcode` / `zcode-intl` 等新增每日签到 provider 的账号可用性取决于账号余额/令牌，功能存在不等于账号可用。
4. 新镜像为 beta，且引入插件二进制安装/更新这一新的高权限面；生产部署需固定 pack 版本并验证来源。
5. AGPL-3.0-only 与旧 MIT 不同，分发/修改需重新做法务确认。

---

## 2. 版本、架构与镜像

| 项目 | 旧镜像 | 新镜像 |
|---|---|---|
| 版本 | 3.1.0 | 3.3.0-beta.6 |
| 网关架构 | 单体，provider 编译进网关 | 网关 + 插件宿主，provider 在外部 pack |
| 插件路由 | 无 | `/api/plugins*`、`/plugins/{pack}/ui/*`、`/plugins/runtime/*` |
| 路由数量（`http.rs` 中 `.route`） | 160 | 191 |
| UI islands 文件数 | 75 | 194 |
| 镜像大小 | 166,483,539 B（约 159 MiB） | 218,794,701 B（约 209 MiB） |
| 容器用户 | root（`Config.User` 为空） | `65532:65532` |
| 许可证 | MIT | AGPL-3.0-only |
| 数据库 schema | 17 | 22 |
| 插件 pack | 无 | `agent2api-providers` + `agent2api-web-providers` |

### 架构影响

- 旧镜像：新增/修复 provider 需要重新构建网关镜像。
- 新镜像：provider 可以独立打包、安装、启停、更新；网关只保留宿主、协议、路由、账号、调度和 UI。
- 新镜像因此新增了插件安装/更新/签名/进程管理/崩溃恢复等能力，也新增了插件供应链风险。
- 新镜像首次启动会从官方 release 自动安装 provider pack；本次验证容器随后用本地 33 家 pack 覆盖官方 30 家 pack。

---

## 3. Provider 支持差异

### 3.1 数量口径

| 口径 | 数量 | 说明 |
|---|---:|---|
| 旧镜像 | 27 | 内置 provider，见附录 A |
| 新镜像默认官方 pack | 32 | 官方 `agent2api-providers` 30 + `agent2api-web-providers` 2 |
| 新镜像 + 本地 33 家 pack | 35 | 本地 pack 33 + web pack 2 |

**默认官方 pack 相对旧镜像：**
- 新增：`demo`、`modelscope`、`modelscope-intl`、`atomcode`、`doubaowork`、`deepseek-web`
- 移除：`nous`（旧镜像内置；官方 pack 未包含）
- 迁移：`chatglm` 从内置变为 web pack
- 净变化：27 → 32（+6 新增，-1 Nous）

**本地 33 家 pack 相对旧镜像：**
- 新增：上述 6 家 + `tabbit`、`tabbit-intl`
- 恢复：`nous`
- 移除：无
- 净变化：27 → 35（+8）

### 3.2 关键新增 provider

| Provider | 旧镜像 | 新镜像默认 | 新镜像本地 pack | 备注 |
|---|---|---|---|---|
| `doubaowork` | ❌ | ✅ | ✅ | 上游新增 |
| `modelscope` / `modelscope-intl` | ❌ | ✅ | ✅ | 上游新增 |
| `atomcode` | ❌ | ✅ | ✅ | 上游新增 |
| `deepseek-web` | ❌ | ✅（web pack） | ✅（web pack） | 网页 provider |
| `tabbit` / `tabbit-intl` | ❌ | ❌ | ✅ | 本地新增 |
| `nous` | ✅ | ❌ | ✅ | 官方 pack 缺失，本地 pack 恢复 |
| `chatglm` | ✅ 内置 | ✅ web pack | ✅ web pack | 迁移，不是删除 |
| `demo` | ❌ | ✅ | ✅ | 协议/转发验证用测试 provider |

### 3.3 Provider 功能差异

- 新镜像的 provider 能力由插件申报，包含模型调用、账号登录/导入、凭证续期、模型目录、余额查询、签到等。
- 旧镜像这些能力编译在网关里；新镜像通过插件协议按 provider 暴露。
- 新镜像的 `agent2api-web-providers` 把 `chatglm`、`deepseek-web` 作为网页 provider 单独打包。
- 旧镜像有 `nous`；新镜像默认官方 pack 没有，必须安装本地 33 家 pack 才能保留。

---

## 4. 签到功能对比

### 4.1 API 与核心链路

两边都保留：

| 功能 | 接口 | 旧 | 新 |
|---|---|---|---|
| 手动签到 | `POST /api/accounts/checkin` | ✅ | ✅ |
| 签到状态 | `GET /api/checkin/status` | ✅ | ✅ |
| 自动签到 | `GET/POST /api/auto-checkin`、`POST /api/auto-checkin/run` | ✅ | ✅ |
| 定时签到 | `/api/checkin-schedules` | ✅ | ✅ |
| 签到中心 | `GET /api/checkin-center` | ✅ | ✅ |
| 签到记录 | `GET/DELETE /api/checkin-records` | ✅ | ✅ |

新镜像 `GET /api/checkin-center` 额外返回 `checkinFeatures`，包含每个 provider 的：
`daily`、`onboarding`、`plans`、`growth`、`modes`、`exclude_guest`、`ui_hooks` 等能力位。

### 4.2 每日签到 provider

| 口径 | 数量 | Provider |
|---|---:|---|
| 旧镜像 | 14 | `workbuddy`、`workbuddy-intl`、`raccoon`、`autoclaw`、`autoclaw-intl`、`qoder`、`qoder-intl`、`trae`、`loomy`、`kuku`、`officeace`、`codearts`、`lobsterai`、`chatglm` |
| 新镜像 | 21 | 上述 14 家 + `modelscope`、`modelscope-intl`、`atomcode`、`zcode`、`zcode-intl`、`doubaowork`、`demo` |

说明：
- `modelscope` / `modelscope-intl` / `atomcode` 是新 provider，同时带签到。
- `zcode` / `zcode-intl` 是已有 provider 新加入每日签到链。
- `doubaowork` 是新 provider，插件申报签到能力。
- `demo` 是测试 provider，用于协议/签到验证。
- `chatglm` 在旧镜像和新镜像都支持签到；本次迁移的账号令牌已失效，因此不能据此判断功能缺失。

### 4.3 UI 实际差异

对比结果：

- `checkin-page.tsx`：新旧内容完全一致。
- `checkin-schedule-tab.tsx`：新旧内容完全一致。
- `checkin-state.ts`：新旧内容完全一致。

新镜像新增了以下文件，但**没有被实际页面引用**：
`checkin-center-tab.tsx`、`checkin-makeup-tab.tsx`、`checkin-schedule-editor.tsx`、`checkin-schedule-extras.tsx`、`checkin-onboarding.ts` 等。

结论：
- 没有新增可见的“补签”tab。
- 没有新增可见的“签到中心”tab；实际 CenterTab 仍是 `checkin-page.tsx` 内的实现。
- 现有签到页会因为后端 `providerOptions` 扩大，自动多出新的 provider 复选框/分组。
- WorkBuddy 连登管家的补签逻辑新旧后端都有，但没有新增独立 UI。

### 4.4 新镜像签到实测

使用新容器 demo 账号执行一次手动签到：

```text
HTTP 200
+10 credits
签到成功
```

记录落库：

```text
providerId: demo
reason: 手动签到
source: single
ok: true
```

没有对旧镜像真实账号执行签到，避免改变生产账号状态。

---

## 5. 账号、Key 与认证

| 项目 | 旧镜像 | 新镜像 |
|---|---|---|
| 账号导出/导入 | v2，含 `customProviders` | v2，含 `customProviders` |
| 自定义 provider | 支持 | 支持 |
| 账号迁移验证 | 原库 94 个账号 | 每个 provider 选 1 个，28 个全部导入成功 |
| 自定义 provider 定义 | 原库 11 个 | 11 个全部导入 |
| 导入失败 | - | 0 |
| 迁移后账号 | - | 29 个（28 迁移 + demo） |
| 网关 Key | 支持 | 支持；新镜像 `system-info` 显示 `v1FailClosed=false`、`apiKeyCount=1` |
| 面板管理员 | 支持 | 支持 |
| 非 root 数据目录 | 不要求 | 需要 `65532:65532` 写权限 |

### 迁移方式建议

- 不要直接复制旧库：账号密钥/加密上下文不同，schema 也从 17 到 22。
- 使用旧镜像 `GET /api/accounts/export` → 新镜像 `POST /api/accounts/import`。
- 导入文件包含 `accounts` 和 `customProviders`；自定义 provider 定义必须先于账号导入。
- 本次验证：28/28 账号导入成功，11 个自定义定义导入成功，0 失败。

### 账号抽样可用性

对新镜像导入的 29 个账号（含 demo）做真实 chat 探测，结果：

- 成功 14 个。
- 失败/未测 15 个。
- 失败原因包括：凭证 401、余额/限流、模型未暴露、sidecar 无账号、模型仅限 agentic harness、请求超时。
- 这部分是**账号状态**差异，不是签到/provider 功能缺失。

---

## 6. 代理、TLS、路由

### 6.1 代理池

两边都保留：

- `/api/proxies/pool`
- `/api/proxies/pool/update`
- `/api/proxies/pool/enable`
- `/api/proxies/pool/remove`
- `/api/proxies/pool/test`
- `/api/proxies/pool/sync-clash`

代理池轮换、`no_reuse`、Clash 同步等本地能力保留。

### 6.2 代理订阅/内核（重要移除）

旧镜像：

- 完整 `/api/subscriptions*` 路由。
- `core/subscriptions`、`core/sub_kernel` 实现。
- 支持订阅添加、保存、刷新、删除、二维码识别、内核下载/重启/移除。

新镜像：

- `/api/subscriptions` 和 `/api/subscriptions/{*rest}` 注册为 `retired_proxy_feature`。
- `sub_kernel` 运行时已移除。
- 旧镜像的 `/api/proxies/pool/provision` 路由在新镜像不存在。
- 代理池本身保留，但“订阅节点 + 内核”这条链路不再提供。

**影响：** 依赖 Clash/订阅节点或内核下载的用户不能直接沿用旧功能；需要改用代理池或外部代理。

### 6.3 路由策略

- 旧镜像：7 种策略。
- 新镜像：同样 7 种。
- 新源码明确保留 `balance-weighted` 作为 fork extension（上游已移除，本仓保留）。
- `priority`、`least-connections`、`round-robin`、`global-round-robin`、`least-recently-used`、`balance-weighted`、`priority-weighted` 均保留。

### 6.4 TLS 指纹

- 两边都有 `egress::apply_emulation`。
- 支持 `off / safari / firefox / edge / chrome` 档位。
- 新镜像同步时保留 wreq + TLS/HTTP2 指纹路径；插件侧也使用 wreq。
- 旧镜像的 TLS 指纹能力没有被新架构丢弃。

---

## 7. 模型、API 与流式链路

| 项目 | 旧镜像 | 新镜像 |
|---|---|---|
| 模型清单（本次账号下观测） | 243 | 552（刷新后） |
| 模型目录来源 | 内置 provider | 插件控制面 + models.dev + 本地映射 |
| `/v1/models` | 支持 | 支持 |
| `/v1/chat/completions` | 支持 | 支持 |
| 流式 | 支持 | 支持；demo 流式验证通过 |
| 首字节保活 | 支持 | 支持 |
| think 剥离 | 支持 | 支持 |
| Key 配额 | 支持 | 支持 |
| 逐家开关 | 支持 | 支持 |

### 模型差异注意

- 新镜像模型数更多，但并非所有 provider 都暴露模型。
- 本次导入账号中，`accio-cn`、`trae` 等在新镜像没有模型项；用旧模型 ID 回退也返回 404。
- `qoder` 可以用旧模型 ID 成功转发，说明部分旧模型 ID 仍可路由。
- 模型数量不是严格可比指标：新镜像包含插件模型目录和自定义 provider 模型，旧镜像只包含内置 provider。

### 流式与协议

- 新镜像 demo 非流式与流式均返回 HTTP 200。
- 流式 SSE 正常返回 `role`、`content`、`finish_reason=stop`、`[DONE]`。
- 旧镜像没有 demo provider，未做同口径 demo 流式验证；核心流式代码在新旧镜像都保留。

---

## 8. UI、i18n 与移动端

| 项目 | 旧镜像 | 新镜像 |
|---|---|---|
| UI islands 文件数 | 75 | 194 |
| 五语 i18n | 5 语言 | 5 语言，新增插件/签到文案 |
| 签到页 | Center / Schedule / Records | 代码完全一致 |
| 插件 UI | 无 | 支持 `/plugins/{pack}/ui/*` |
| 插件主题/运行时资源 | 无 | 支持 |
| 移动端 | 有 | 保留 |

新镜像 UI 文件数量大增，主要来自：
- 插件管理/插件 UI 页面；
- 签到相关拆分文件（其中一部分未接线）；
- 账号/模型/设置等上游 UI 更新。

**不要仅按文件数量判断功能增加**：签到页实际代码与旧镜像一致，新增的 makeup/center 文件未接线。

---

## 9. 插件系统（新镜像独有）

新镜像新增：

- `GET /api/plugins`
- `POST /api/plugins/install`
- `POST /api/plugins/uninstall`
- `POST /api/plugins/enable`
- `POST /api/plugins/provider`
- `GET/POST /api/plugins/update`
- `GET /api/plugins/release`
- `POST /api/plugins/install-from-release`
- `/plugins/{pack}/ui/*`
- `/plugins/runtime/*`

能力：

- 安装/卸载/启停 provider pack。
- 逐 provider 启停。
- 插件 UI 托管。
- 插件更新检查。
- 插件进程崩溃恢复。
- 插件协议握手、控制面、账号、模型、转发、签到等 RPC。

风险：

- 插件安装/更新会 spawn 外部二进制，属于高权限面。
- 官方 pack 有签名；本地 pack 需要确认来源和签名策略。
- 新镜像首次启动依赖官方 release 自动安装 pack，离线环境需要预置 pack。
- 插件版本与网关协议需要匹配；本次使用 protocol `v3.3.0-beta.6`。

---

## 10. 部署、安全与许可证

| 项目 | 旧镜像 | 新镜像 |
|---|---|---|
| 容器用户 | root | `65532:65532` |
| 数据目录权限 | root 可写 | 需要 `chown 65532:65532` |
| 镜像大小 | 约 159 MiB | 约 209 MiB |
| 健康检查 | 有 | 有 |
| 数据卷 | `/data` | `/data` |
| 默认端口 | 3065 | 3065 |
| 许可证 | MIT | AGPL-3.0-only |
| 版本通道 | 3.1.0 | 3.3.0-beta.6 |

### 部署影响

- 旧部署的 bind mount 如果属主是 root，新镜像非 root 用户可能无法写 `/data`，需要调整权限。
- 新镜像镜像体积增加约 50 MiB；provider pack 另计（本地 pack 约 15 MiB）。
- 新镜像为 beta；生产升级建议固定镜像 digest 与 pack 版本。
- AGPL 与 MIT 的义务不同，分发/修改前需要法务确认。

---

## 11. 测试与验证记录

### 网关

- 旧镜像历史验收基线：400 passed / 0 failed / 1 ignored。
- 新镜像：398 passed / 0 failed / 1 ignored。
- 新镜像 release 构建：31m00s，二进制约 20 MiB。

### 插件

- `cargo test -p agent2api-providers`：52 passed。
- `verify.mjs`：28/28。
- 网关 `verify-plugin-host.mjs`：28/28。
- `parity-check.mjs`：122 项检查，0 失败（覆盖 3 个有冻结基线的 provider）。

### 账号迁移

- 旧库账号：94 个，28 个 provider 组。
- 每 provider 选 1 个：28 个。
- 导入：28 added / 0 failed；11 个自定义 provider 定义导入。
- 新容器账号：29 个（含 demo）。

### 账号 chat 抽样

- 成功 14 个：`autoclaw`、`cline-free`、`custom-3145937efbc2`、`custom-3e9cbb1f6d15`、`custom-6ba0863f379d`、`custom-aa6aa658b064`、`custom-567465f8eb78`、`demo`、`lobsterai`、`nous`、`qoder`、`raccoon`、`workbuddy`、`workbuddy-intl`。
- 失败/未测 15 个：凭证 401、余额/限流、模型未暴露、sidecar 无账号、模型限制、超时等。
- 该结果只代表本次抽样的账号与模型，不代表 provider 的全部账号/全部模型。

### 签到实测

- 新镜像 demo：HTTP 200，`+10 credits`，签到记录写入成功。
- 未对真实账号执行签到。

---

## 12. 风险与建议

### 必须确认

1. **Nous 默认丢失**：如果继续使用官方 30 家 pack，旧镜像的 Nous 会消失；需要发布/安装本地 33 家 pack。
2. **代理订阅/内核移除**：确认业务是否依赖 `/api/subscriptions*`、`sub_kernel`、`pool/provision`；如依赖，需要保留旧镜像或补回功能。
3. **AGPL-3.0-only**：与旧 MIT 不同，分发/修改前确认合规。
4. **非 root 数据目录**：升级前 `chown -R 65532:65532 /data`，否则新容器可能无法写库。
5. **beta 版本**：新镜像为 `3.3.0-beta.6`，不建议直接替换生产稳定版；先灰度。

### 建议的升级步骤

1. 旧镜像 `GET /api/accounts/export` 备份账号（含 `customProviders`）。
2. 新容器使用独立端口/数据卷启动，先安装本地 33 家 pack。
3. 导入账号，确认每个 provider 数量与自定义 provider 定义。
4. 检查代理订阅依赖；如依赖，暂缓升级或补回功能。
5. 检查签到 provider 列表；确认新增的 `modelscope`、`atomcode`、`zcode`、`doubaowork` 是否符合预期。
6. 做账号级可用性抽样（不要全量签到/全量转发）。
7. 固定镜像 digest、pack 版本和许可证声明。

---

## 附录 A：旧镜像 27 家 provider

```text
workbuddy, workbuddy-intl, raccoon, catpaw, autoclaw, autoclaw-intl,
qoder, qoder-intl, cline-free, cline-pass, accio, accio-cn,
zcode, zcode-intl, codearts, trae, loomy, kuku,
monkeycode, monkeycode-intl, commandcode, antigravity, officeace,
nous, lobsterai, orcaterm, chatglm
```

## 附录 B：新镜像默认官方 pack（32 家）

```text
cline-free, cline-pass, demo, commandcode, modelscope, modelscope-intl,
orcaterm, kuku, monkeycode, monkeycode-intl, lobsterai, loomy,
qoder, qoder-intl, raccoon, atomcode, autoclaw, autoclaw-intl,
officeace, zcode, zcode-intl, workbuddy, workbuddy-intl, trae,
catpaw, accio, accio-cn, codearts, antigravity, doubaowork,
chatglm, deepseek-web
```

## 附录 C：新镜像本地 33 家 pack（35 家）

```text
cline-free, cline-pass, demo, commandcode, modelscope, modelscope-intl,
orcaterm, kuku, monkeycode, monkeycode-intl, lobsterai, loomy,
qoder, qoder-intl, raccoon, atomcode, autoclaw, autoclaw-intl,
officeace, zcode, zcode-intl, workbuddy, workbuddy-intl, trae,
catpaw, accio, accio-cn, codearts, antigravity, doubaowork,
tabbit, tabbit-intl, nous, chatglm, deepseek-web
```

## 附录 D：签到 provider

旧镜像（14）：

```text
workbuddy, workbuddy-intl, raccoon, autoclaw, autoclaw-intl,
qoder, qoder-intl, trae, loomy, kuku, officeace, codearts,
lobsterai, chatglm
```

新镜像（21）：

```text
workbuddy, workbuddy-intl, raccoon, autoclaw, autoclaw-intl,
qoder, qoder-intl, trae, loomy, kuku, officeace, codearts,
chatglm, modelscope, modelscope-intl, atomcode, lobsterai,
zcode, zcode-intl, demo, doubaowork
```

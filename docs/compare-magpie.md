# 与 magpie 的功能对比与可借鉴项

> 状态：调研文档，**不含任何实现改动**。
>
> 对比对象：[`yetone/magpie`](https://github.com/yetone/magpie)（Go，本地 LLM 网关 +
> Agent 配置管理器）。调研时仓库 HEAD 为 `62a3e79`，社区注册表为
> `magpie-community/plugins` @ `af360a5`。外部项目会变，引用结论前请复核。

## 一句话结论

**这不是同一类产品。** magpie 是「Agent 配置管理器 + 通用 LLM 路由网关」，
核心是让用户在 60+ 个编程 Agent 里随便换模型；本项目是「桌面客户端登录态收割器」，
核心是把 WorkBuddy / 小浣熊 / Qoder 这类国内客户端的已有账号变成 OpenAI 兼容 API。

两者的重叠只有「本地跑一个网关」这一层。因此本文的对比重点不是「谁更强」，
而是**哪些能力是结构性缺口、哪些是场景不同导致的合理差异**。

---

## 一、定位差异

| | Agent2API | magpie |
|---|---|---|
| 解决的问题 | 登录了 N 个国内 AI 客户端，每个只能在自己的客户端里用 | 订阅被锁在单个 Agent 里，换个 Agent 就没了 |
| 主要动作 | **收割**已有桌面端会话 → 转发 | **改写** Agent 配置 → 路由到任意上游 |
| 网关的角色 | **主角**（面板 / 账号 / 签到 / 余额都围绕它） | **配角**（存在的目的是让每个 Agent 摸到每个 provider） |
| 上游来源 | 逆向各家客户端协议，复用登录态 | 官方订阅 OAuth + API Key + 本地模型 |

---

## 二、上游覆盖

| | Agent2API | magpie |
|---|---|---|
| 内置 | **27 个**（20 家 × 地区/额度池） | 6 个内置订阅 + 57 个 API Key 预设 |
| 形态 | 全部是国内 AI 客户端登录态 | 官方订阅 / API Key / 本地模型 / 远程 magpie |

**magpie 的 6 个内置订阅**：`claude`、`codex`、`copilot`、`gemini`、
`antigravity`、`chatgpt-api`。

**关键事实**：magpie 原本内置 13 个国内订阅（WorkBuddy / Qoder / Trae CN /
ZCode / Grok / Cursor / Devin / Factory / Kiro / MiMo / Command Code / Zed），
**已全部移除并改由社区插件提供**，理由写在它的 `AGENTS.md`：

> Reaching them can break their vendors' terms, so they moved out of magpie into
> community OpenCode plugins, which keeps magpie itself from being banned.

所以「magpie 支持 WorkBuddy」只在装了对应插件的前提下成立。

### 两边覆盖的中国厂商高度重叠

| 厂商 | magpie（插件） | Agent2API（内置） |
|---|---|---|
| WorkBuddy | ✅ | ✅ 国内 + 国际 |
| Qoder | ✅ | ✅ 国内 + 国际 |
| Trae | ✅（标注实验性） | ✅ |
| 小浣熊 | ✅（带签到） | ✅ |
| AutoClaw | ✅（带签到） | ✅ 国内 + 国际 |
| CodeArts | ✅（非官方） | ✅ |
| ZCode | ✅ | ✅ 国内 + 国际 |
| Cline | ✅ | ✅ Free / Pass |
| Kimi / MiMo / Open Design | ✅ | ❌ |

**这是两种策略**：magpie 把国内厂商外移以规避封禁风险，覆盖靠社区补齐；
本项目内置 27 个 provider id，其中 17 家支持余额查询、14 个 provider id 参与签到，
并且全部共享同一套出站指纹与脱敏层。

---

## 三、协议与端点

| 端点 | Agent2API | magpie |
|---|---|---|
| `/v1/chat/completions` | ✅ | ✅ |
| `/v1/responses` | ✅ | ✅（WebSocket 升级 → 426） |
| `/v1/messages` | ✅ | ✅ |
| `/v1/messages/count_tokens` | ✅ | ✅ |
| `/v1/models` | ✅ | ✅ |
| Gemini `:generateContent` | ❌ | ✅ |
| `/v1/embeddings` / `/v1/rerank` | ❌ | ✅ |
| `/v1/audio/transcriptions` | ❌ | ✅ |
| Ollama 形状目录 | ❌ | ✅ |
| `/v1/codex/models` | ❌ | ✅ |
| 协议互译 | ✅ 三台状态机 | ✅ 双向互译 |

### 已知断链：Gemini CLI 接管

本项目 Agent 页声称可接管 Gemini CLI，但网关没有 Gemini 端点。证据链：

- `core/agents/mod.rs`：`gemini-cli` 声明 `protocol: "gemini"`
- `core/agents/apply.rs`：写入 `GOOGLE_GEMINI_BASE_URL` 指向本网关
- `core/agents/apply.rs`：只写 `model.name`，**不追加 `/v1beta`**
- `server/http.rs`：只注册 `/v1/chat/completions`、`/v1/responses`、
  `/v1/messages`、`/v1/messages/count_tokens`

Gemini CLI 会拼 `/v1beta/models/...`，网关无此路由 —— **这是一次会直接失败的接管**。

---

## 四、路由与故障转移

**修正一个容易下的错误判断**：本项目路由并不弱。

| | Agent2API | magpie |
|---|---|---|
| 策略数 | **7**（`priority` / `least-connections` / `round-robin` / `global-round-robin` / `least-recently-used` / `balance-weighted` / `priority-weighted`） | 6（`smart` / `order` / `rotate` / `usage` / `pace` / `weight`） |
| 路由组 | ❌ | ✅ `group/<id>` |
| 嵌套组 | ❌ | ✅ |
| 意图路由 | ❌ | ✅ 小分类模型按轮次选模型 |
| 会话亲和 | ✅（4 类头 + `prompt_cache_key`） | ✅（`stays=auto/session/turn/off/strict`） |
| 钉住账号 | ✅ | ✅ `X-Magpie-Account` |
| 429 换号 | ✅ 冷却 + 换下一个 | ✅ 换号，Agent 看不到错误 |
| 路由可观测 | ❌ | ✅ `GET /v1/magpie/route` |

**真实缺口只有三项**：路由组、嵌套组、意图路由。策略数量本项目反而更多。

---

## 五、账号、配额、签到

| | Agent2API | magpie |
|---|---|---|
| 多账号 | ✅ | ✅ |
| 余额查询 | ✅ **17 家**（实现 `supports_usage` 的 provider 数） | ✅ 覆盖订阅 + Key |
| 签到 | ✅ **14 个 provider id**（含国内/国际变体；另有增长任务与领取类） | ✅ **4 家**（WorkBuddy CN / Trae CN / MiniMax Code / Qoder） |
| 人机验证 | ✅ 支持 | ❌ 明确「never solves captchas」 |
| 5 小时窗口预热 | ❌ 无此概念 | ✅ Claude / ChatGPT 专属 |
| 每账号限额 | ✅ 余额 + Token 规则（跳过/禁用） | ✅ 每窗口用量上限 |
| 网关 Key 限额 | ⚠️ 单维（Token）+ 单条 | ✅ **最多 5 条**，日/周/月/任意 N 天 N 小时，Token + **费用**双维度 |

**本项目明显更强**：签到覆盖 14 个 provider id（`auto_checkin::CHECKIN_PROVIDERS`）vs magpie 的 4 家，且能过验证码；余额覆盖 17 家（`supports_usage` 为 true 的 provider）。
**magpie 更强**：Key 限额的粒度（多规则、任意窗口、费用维度）。

---

## 六、反检测与出网（本项目独有）

| | Agent2API | magpie |
|---|---|---|
| 每账号独立出口 | ✅ | ✅ |
| 出口轮转 | ✅ round-robin / least-latency | ❌ |
| **TLS/HTTP2 指纹模拟** | ✅ wreq + BoringSSL，4 档 | ❌ **无**（`go.mod` 无 utls / tls-client） |
| 出站指纹脱敏 | ✅ | ❌ |
| 内容拦截降级 | ✅ | ❌ |
| UA 伪装 | ✅ 按家定制 | ⚠️ 仅 WorkBuddy UA + 可回放 Agent UA |

magpie 唯一的传输层特例是**对 ChatGPT 后端强制 HTTP/1.1**（为了像 Codex）。
**完全没有 JA3/JA4 对抗能力**——这是本项目相对它的结构性优势。

---

## 七、部署与运维

| | Agent2API | magpie |
|---|---|---|
| 桌面端 | ✅ 三平台托盘 | ✅ 菜单栏 / 窗口 / TUI / Web UI / CLI |
| 无 GUI 服务端 | ✅ 注册系统服务 | ✅ `magpie serve` |
| Docker | ✅ amd64 / arm64 | ✅ |
| Termux | ❌ | ✅ |
| 自更新 | ✅ 容器内替换 | ✅ 每次构建自更新；macOS 签名 + 公证 |
| 局域网共享 | ⚠️ 面板分端口 + Key | ✅ 命名 Key + 每 Key 限额 |
| 远程实例互用 | ❌ | ✅ Remote magpie |
| HTTPS 终结 | ❌（靠 Caddy 反代） | ✅ `--tls-cert` / `--tls-key` |
| 备份 | ✅ 整库 SQL 导出/导入 | ✅ 文件 / WebDAV / S3 / 加密 GitHub 仓库 |
| 配置档案 | ❌ | ✅ `magpie save` / `use` |

**magpie 在运维便利性上全面领先**。

---

## 八、Agent 接管

| | Agent2API | magpie |
|---|---|---|
| 适配器数 | **12** | **60+** |
| 写入方式 | 原子写 + 备份 + 多套配置 | 原子写 + **保留注释与格式** |
| 注释保留 | ❌ **已知取舍**（代码注释原文：「为几行注释引两棵新依赖树不划算」） | ✅ |

本项目 12 个：`claude-code`、`codex`、`gemini-cli`、`zcode`、`workbuddy`、
`qoder`、`opencode`、`cursor-cli`、`zed`、`cline`、`kimi-code`、`crush`。

---

## 九、扩展性

| | Agent2API | magpie |
|---|---|---|
| 插件系统 | ❌ | ✅ 四套运行时（见下节） |
| Provider trait 钩子 | ✅ **20+** | — |
| 自定义上游 | ✅ OpenAI 兼容 + Responses/Anthropic 转换 | ✅ 任意 OpenAI/Anthropic/Responses/Gemini URL |
| 接受外部 PR | ✅ | ❌ **明确关闭** |

---

## 十、magpie 的插件体系（调研附录）

magpie 的「插件」不是一套机制，是**四套**：

| 类型 | 运行时 | 隔离 |
|---|---|---|
| 内置插件 | Go，编译期 | 无（与主程序同权限） |
| Provider 插件 | **Bun 子进程** | **无沙箱，完整用户权限** |
| Gateway middleware | **moejs**（Go 内嵌 JS） | 能力受限，非 OS 沙箱 |
| Page / 面板 | 浏览器 iframe | `sandbox="allow-scripts"` + CSP 全禁 |

**Provider 插件本质就是 OpenCode plugin 或 pi package**——「不需要任何 magpie
专属的东西」。这是它生态能起来的原因：直接吃 OpenCode 已有的 auth 插件生态。

### 10.1 内置插件（14 个，全部可开关）

`budget`（Key 限额）、`gateway`（网关页）、`groups`（路由组）、`library`、
`middleware`（JS 宿主）、`otel`（OTel 导出）、`redact`（隐私脱敏）、
`rotation`（6 策略）、`routing`（路由页）、`search`（借用联网模型）、
`sessions`（会话浏览）、`tools`（插件工具→MCP）、`usage`（用量）、
`warmups`（窗口预热 + 签到）。

**设计取向值得注意**：magpie 把**自己的核心功能也做成了插件**（routing、
rotation、usage、budget、redact 全是插件），可以整块关掉。`Essential` 控制预设，
`InUse > 0` 时拒绝关闭。

### 10.2 社区注册表（33 条）

注册表：`https://raw.githubusercontent.com/magpie-community/plugins/main/registry.json`

**⚠️ 内置快照只有 17 条，实时 33 条——内置副本落后 16 条。**

- **Provider 插件 20 个**：kimi-work、opencode-zen-free、zcode、qoder（国际+国内）、
  devin、zed、factory、grok、commandcode-plan、mimo-app、cursor、kiro、
  minimax-code（+global）、workbuddy（+ai）、cline、trae（cn+global，实验性）、
  opendesign、raccoon、autoclaw（+cn）、codearts（非官方）
- **Middleware 9 个**：param-override、model-map、system-prompt、word-guard、
  think-tags、antigravity-cloak、codex-history、reserve（实为路由策略）
- **Agent 3 个**：aider、type4me、kimi-desktop
- **其他 2 个**：tools-calc、page-usage

异常：仓库有 34 个包目录但注册表只列 33——`middleware-block-patterns` 不在注册表，
Discover 里看不到。

### 10.3 安全模型（最该看清的部分）

magpie 自己在文档里承认：

- 「Installing a plugin is trusting it」
- 「Bun plugins have host privileges; install trusted code」

**确实有的防护**：`bun add --ignore-scripts`（不跑安装脚本）、npm integrity 校验、
社区包 sha512 校验、社区包经 PR 审核。

**确实没有的**：无 seccomp / 容器 / 权限清单；加载时无代码签名校验；
任意 npm / git / 本地路径插件不经审核；镜像拉取的元数据不校验。

---

## 十一、Provider 插件 vs 本项目 `ProviderAdapter`

这一节单独展开，因为 provider 插件是 magpie 扩展性的**主战场**（20 / 33 个
市场条目是它），也是与本项目 `ProviderAdapter` trait 唯一真正同构的部分。

### 11.1 两边的 provider 抽象对照

| 维度 | magpie provider 插件 | 本项目 `ProviderAdapter` |
|---|---|---|
| 注册方式 | 运行期：Bun 子进程，npm 包 | **编译期**：`adapter_for(kind)` 静态分派 |
| 实现载体 | JS，`auth.loader` 返回一个 `fetch` | Rust trait，`build_chat_request` 返回 `ChatRequestPlan` |
| 能力边界 | 网关代管路由/翻译/配额 | 适配器**自己**知道 URL、头、体、错误分类 |
| 谁能写 | 任何人（npm 发布即可） | 只有能改仓库并提交 PR 的人 |
| 类型安全 | 无（JS + zod 校验） | 编译期强制 |
| 崩溃影响 | Bun 子进程，可重启 | 与主进程同命（`panic=abort`） |

**一句话**：magpie 把 provider 做成「给一个 fetch」，本项目把 provider 做成
「实现 30+ 个语义钩子」。前者开放，后者精确。

### 11.2 钩子级逐项对比

下表左列是 magpie 的钩子，右列是本项目的对应物；「—」表示没有等价物。

| magpie 钩子 | 职责 | 本项目对应物 | 判定 |
|---|---|---|---|
| `auth.provider` / `auth.methods[]` | 声明 provider 与登录方式（oauth / api） | `ProviderKind` + `supports_web_login` / `build_login_url` / `exchange_login_code` | ✅ 等价 |
| `auth.loader` | 返回携带请求的 `fetch` | `build_chat_request` + `ChatRequestPlan {url, headers, body, response}` | ✅ 等价（更强：连响应协议也声明） |
| `auth.refresh` | 到期前续期 | `ensure_access_token` + `refresh_access_token` + `credentials_expiring` | ✅ 等价（更细：分「临期刷新」与「401 强制刷新」） |
| `auth.usage` | 计划 / 额度用量 | `supports_usage` + `query_usage` | ✅ 等价 |
| `auth.checkin` | 每日签到 | **不在 trait 上**，在 `billing::checkin::checkin_for` 按 provider 分派 | ⚠️ 有但位置不同 |
| `auth.options[]` | 每 provider 开关项（≤16） | —（内置家无此机制；自定义家有 `ProviderQuirks`） | ❌ 缺 |
| `auth.icon` | 图标 | `providers::PROVIDERS` 静态元数据 | ✅ 等价（静态） |
| `auth.maxConcurrency` | 并发上限 | **账号级** `maxConcurrent`（`routing::max_concurrent_of`） | ⚠️ 粒度不同 |
| `provider.models` | 列模型，可带 `serviceTiers` / 决策 / 限额 | `list_models` + `advertise_models` + `model_meta` | ✅ 等价（更强：带能力元数据与本地覆盖） |
| `chat.headers` | 加请求头 | `build_chat_request` 的 `headers` 字段 | ✅ 等价 |
| `tool` | 提供工具（经 MCP 给所有 Agent） | — | ❌ 缺 |

**反向：本项目 trait 有、magpie 插件没有的钩子**

| 本项目钩子 | 作用 | magpie 是否有等价物 |
|---|---|---|
| `classify_error` | 把 status + 错误体分成 Fatal / Retryable / QuotaLimited 等 | ❌ 插件无法影响分类（网关统一判定） |
| `classify_conversation_error` | 有状态路径的错误分类 | ❌ |
| `retry_advice` | 按错误体给出重试预算建议 | ❌ |
| `quota_cooldown_models` | 一次限额要给哪些模型键记账 | ❌ |
| `reasoning_patch` / `outbound_reasoning` | 把通用思考档位翻译成本家字段 | ❌（magpie 无思考档位绑定） |
| `is_stateful` / `forward_conversation` | 有状态会话式转发 | ❌（插件只能提供 `fetch`） |
| `sse_model_rewrite` / `sse_strip_newline_chunks` | SSE 帧级修正 | ❌ |
| `is_error_response` | HTTP 200 但体是错误 | ❌ |
| `system_proxy_when_unset` / `env_credentials_present` | 出口与凭证来源策略 | ❌ |
| `supports_default_model` / `allows_anonymous_default_session` | 无账号时的降级语义 | ❌ |

### 11.3 这个对比说明什么

**本项目在「适配器语义」上比 magpie 深很多**。原因是场景不同：

- magpie 的上游是**标准 API**（OpenAI / Anthropic / Gemini 官方端点），协议一致，
  所以插件只需「拿对凭证 + 拼对 URL」，剩下的交给网关统一处理。
- 本项目的上游是**逆向出来的客户端协议**（会话准入、SSE 信封、NDJSON、
  设备风控身份、限额信封藏在 200 里），每家都不一样，所以必须在适配器层
  提供细粒度钩子。

换句话说：**magpie 的插件接口简单，是因为它的上游简单**。把它的接口直接
搬过来，本项目 27 家逆向协议一家都接不上。

### 11.4 值得借鉴的四个点

#### ① `auth.options[]` —— 每 provider 的开关项

magpie 允许插件声明最多 16 个开关（`{key,label,description,default,confirm}`），
在设置页渲染成开关，插件运行时通过 `input.magpie.option(s)` 读。

**本项目现状**：内置家的行为开关都是**全局**的（`promptMode`、
`sanitizeBlacklistFingerprints` 等），没有「这家要不要开 X」的维度。自定义家
有 `ProviderQuirks`（`urlSuffix` / `headers` / `toolTypeCustom` /
`emulateOpencode`），但那是**自定义家专属**，内置家享受不到。

**可借鉴**：给内置家加一组 per-provider 开关（如「WorkBuddy 是否走指纹脱敏」、
「Qoder 是否启用思考标签剥离」）。纯配置层改动，不涉及插件运行时。

#### ② `auth.maxConcurrency` 的**粒度**

magpie 是 **provider 级**并发上限；本项目是 **账号级**
（`store_util::max_concurrent_public_for`）。

**本项目的粒度其实更合理**（同一家不同账号额度不同），但缺一个
**provider 级兜底**：某个 provider 全部账号加起来并发过高时，目前无法整体限流。

一个已有的例外值得注意：CodeArts 有 provider 级默认并发
（`codearts::session::DEFAULT_SESSION_LIMIT`，由
`max_concurrent_public_for` 按 provider 套上）——但它作用于**单个账号**的
读数，不是「全 provider 合计」的上限。所以这不构成对「缺 provider 级总量」
的反驳，反而说明这类「按家定默认值」的需求是真实存在的。

**可借鉴**：在账号级之上加一层 provider 级总量上限。

#### ③ `auth.checkin` 提升为 trait 钩子

本项目的签到分派写在 `billing::checkin::checkin_for` 的 `match provider` 里，
而 `ProviderAdapter` 上**没有**签到方法。后果：新增一家要同时改两处
（`adapter.rs` 与 `checkin.rs`），而且 `CHECKIN_PROVIDERS` 是独立维护的
`[&str; 14]` 数组 —— 新增一家忘了加进去，签到会静默不执行。

**可借鉴**：把 `supports_checkin` / `checkin` 提到 trait 上，让
`CHECKIN_PROVIDERS` 从 `implemented_kinds()` 派生。这与本项目既有的
`supports_usage` / `query_usage` 完全同构。

**注意本项目已有的兜底**：`auto_checkin::PROVIDERS_VERSION` +
`PROVIDERS_ADDED_IN` 解决了「新增家默认勾选」与「用户取消勾选后不被补回来」
的冲突（只补一次，靠配置里的版本号记账）。所以这里要改的是**声明位置**，
不是那套版本机制 —— 后者是对的，应当保留。

#### ④ `provider.models` 的「失败保留旧清单」语义

magpie 明确写了「A failed read keeps the last list」。

**本项目现状**：`refresh_models` 的文档写着「刷新失败**不返回错误**：失败时
保留现有清单」—— **语义一致**。这一项两边已经对齐，不需要改动，列出来是为了
避免后来者以为这里是缺口。

### 11.5 不建议借鉴的

| 点 | 理由 |
|---|---|
| **运行期注册 provider** | 本项目 27 家都要访问 `AccountStore`、`limiter`、`egress`、`model_rules`，这些是强类型 Rust 结构；JS 插件拿不到，只能再开一套 RPC 面（magpie 就是这么做的：Bun 子进程 + stdin/stdout JSON 行）。成本极高，收益只是「不用重新编译」。 |
| **`auth.loader` 返回 `fetch`** | 本项目的适配器要的不只是一个 fetch，还要声明响应协议（`UpstreamResponse` 四档）、SSE 帧级修正、错误分类。只给 fetch 等于把 90% 的逻辑推回网关，而网关不认识各家方言。 |
| **把 provider 拆成 npm 包** | 本项目 27 家共享大量内部模块（`auth_http`、`jwt`、`refresh_flight`、`crypto`）。拆出去意味着这些要么公开 API 化，要么每家重写一份。 |

### 11.6 小结

| 项 | 结论 |
|---|---|
| provider 抽象 | **不迁移**。本项目的 trait 更深，是场景决定的，不是缺陷 |
| `auth.options[]` | **值得做**。给内置家加 per-provider 开关 |
| provider 级并发上限 | **值得做**。当前只有账号级 |
| `checkin` 提升为 trait 钩子 | **值得做**。消除「改两处」与静默漏配 |
| `provider.models` 失败保留 | 已对齐，无需改动 |
| `tool` 钩子 | **可选**。本项目的 MCP 暴露面是另一个话题 |

---

## 十二、可借鉴项（按价值 / 成本排序）

### 12.1 值得做

| # | 项目 | 理由 | 成本 |
|---|---|---|---|
| 1 | **Gemini 端点** | 已确认断链，用户点了就失败 | 中 |
| 2 | **Agent 写入保留注释** | 已知取舍，用户可感知（Codex `config.toml` 常带注释） | 低-中 |
| 3 | **Key 多维限额** | 共享网关刚需，`limiter` 已有 `rule_kind` 可复用 | 中低 |
| 4 | **think-tags 通用化** | 把 Qoder 的 `strip_thinking_tags` 提到通用层 | 低 |
| 5 | **词语拦截（word-guard 式）** | 只查用户文本，reject / mask 两档 | 低 |
| 6 | **「核心功能做成可开关单元」** | 借鉴设计取向，不需要插件运行时 | 低 |
| 7 | **per-provider 开关（`auth.options[]` 思路）** | 内置家的行为开关目前只有全局维度，缺「这家要不要开 X」 | 低 |
| 8 | **provider 级并发上限** | 当前只有账号级；缺某家全部账号合计过载时的兜底 | 低-中 |
| 9 | **`checkin` 提升为 trait 钩子** | 现在签到分派在 `checkin_for` 的 `match provider` 里，而 `ProviderAdapter` 上没有签到方法——新增一家要同时改 `adapter.rs`、`checkin.rs` 与 `CHECKIN_PROVIDERS` 三处（后者已有 `PROVIDERS_VERSION` 版本兜底，但漏加会让该家既不签到也不出现在界面清单） | 中低 |

### 12.2 需先评估

| 项目 | 说明 |
|---|---|
| **路由组 / 嵌套组** | 本项目已有模型映射 + 聚合目录，需先判断「组」能否用现有机制表达，很可能覆盖 80% 场景 |
| **通用参数覆盖** | magpie 的 operations 模型（set/delete/regex/条件）值得参考，但它只能看 body（不看头） |
| **`tool` 钩子（插件工具 → MCP）** | magpie 把插件工具暴露成 MCP server 给所有 Agent；本项目是否需要对外暴露工具是独立的产品决策 |

### 12.3 明确不建议

| 项目 | 理由 |
|---|---|
| **插件运行时本身** | 本项目 `ProviderAdapter` trait 已有 20+ 钩子，能力不比插件 API 少；全仓 `panic=abort` + 零 unwrap 与「运行期加载任意 JS / 进程」根本冲突；magpie 的插件安全是「靠信任」而非技术隔离 |
| **运行期注册 provider / 把 provider 拆成 npm 包** | 27 家适配器共享 `auth_http` / `jwt` / `refresh_flight` / `crypto` 等内部模块，且都要访问 `AccountStore`、`limiter`、`egress` 等强类型结构；拆包等于把这些全部公开 API 化，或每家重写一份 |
| **意图路由** | 需要额外一次模型调用做分类，而本项目额度来自用户自己的账号——用一次额度决定用哪份额度，收益未必为正 |
| **5 小时窗口预热** | 依赖 Claude / ChatGPT 的特定计费结构，本项目上游是日额度 / 积分制 |
| **Claude 走真实 CLI + MCP 桥接** | 那是为绕开 Anthropic 的第三方 system-prompt 分类器，本项目上游无此约束 |
| **Telemetry 默认开启** | 与「本地优先、凭证不出机器」的定位冲突 |
| **核心关闭外部 PR** | 本项目靠社区贡献维护 27 个 provider |

---

## 十三、本项目已有、magpie 没有的能力

调研中确认以下能力为**本项目独有**，不应在「借鉴」叙事里被稀释：

1. **TLS/HTTP2 指纹模拟**（wreq + BoringSSL，4 档可切换）
2. **签到覆盖 14 个 provider id + 人机验证**（magpie 只有 4 家，且明确不碰验证码）
3. **出站指纹脱敏 + 内容拦截降级**
4. **出口轮转**（round-robin / least-latency）
5. **余额查询 17 家 + 失效退避**
6. **路由策略 7 种**（比 magpie 的 6 种多）

---

## 十四、参考

- magpie 仓库：<https://github.com/yetone/magpie>
- 社区注册表：<https://github.com/magpie-community/plugins>
- 集成文档：<https://github.com/yetone/magpie/blob/main/docs/integrating.md>
- 本项目 TLS 指纹说明：[`docs/tls-fingerprint.md`](./tls-fingerprint.md)

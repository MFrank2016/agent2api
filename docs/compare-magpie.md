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

## 十一、可借鉴项（按价值 / 成本排序）

### 11.1 值得做

| # | 项目 | 理由 | 成本 |
|---|---|---|---|
| 1 | **Gemini 端点** | 已确认断链，用户点了就失败 | 中 |
| 2 | **Agent 写入保留注释** | 已知取舍，用户可感知（Codex `config.toml` 常带注释） | 低-中 |
| 3 | **Key 多维限额** | 共享网关刚需，`limiter` 已有 `rule_kind` 可复用 | 中低 |
| 4 | **think-tags 通用化** | 把 Qoder 的 `strip_thinking_tags` 提到通用层 | 低 |
| 5 | **词语拦截（word-guard 式）** | 只查用户文本，reject / mask 两档 | 低 |
| 6 | **「核心功能做成可开关单元」** | 借鉴设计取向，不需要插件运行时 | 低 |

### 11.2 需先评估

| 项目 | 说明 |
|---|---|
| **路由组 / 嵌套组** | 本项目已有模型映射 + 聚合目录，需先判断「组」能否用现有机制表达，很可能覆盖 80% 场景 |
| **通用参数覆盖** | magpie 的 operations 模型（set/delete/regex/条件）值得参考，但它只能看 body（不看头） |

### 11.3 明确不建议

| 项目 | 理由 |
|---|---|
| **插件运行时本身** | 本项目 `ProviderAdapter` trait 已有 20+ 钩子，能力不比插件 API 少；全仓 `panic=abort` + 零 unwrap 与「运行期加载任意 JS / 进程」根本冲突；magpie 的插件安全是「靠信任」而非技术隔离 |
| **意图路由** | 需要额外一次模型调用做分类，而本项目额度来自用户自己的账号——用一次额度决定用哪份额度，收益未必为正 |
| **5 小时窗口预热** | 依赖 Claude / ChatGPT 的特定计费结构，本项目上游是日额度 / 积分制 |
| **Claude 走真实 CLI + MCP 桥接** | 那是为绕开 Anthropic 的第三方 system-prompt 分类器，本项目上游无此约束 |
| **Telemetry 默认开启** | 与「本地优先、凭证不出机器」的定位冲突 |
| **核心关闭外部 PR** | 本项目靠社区贡献维护 27 个 provider |

---

## 十二、本项目已有、magpie 没有的能力

调研中确认以下能力为**本项目独有**，不应在「借鉴」叙事里被稀释：

1. **TLS/HTTP2 指纹模拟**（wreq + BoringSSL，4 档可切换）
2. **签到覆盖 14 个 provider id + 人机验证**（magpie 只有 4 家，且明确不碰验证码）
3. **出站指纹脱敏 + 内容拦截降级**
4. **出口轮转**（round-robin / least-latency）
5. **余额查询 17 家 + 失效退避**
6. **路由策略 7 种**（比 magpie 的 6 种多）

---

## 十三、参考

- magpie 仓库：<https://github.com/yetone/magpie>
- 社区注册表：<https://github.com/magpie-community/plugins>
- 集成文档：<https://github.com/yetone/magpie/blob/main/docs/integrating.md>
- 本项目 TLS 指纹说明：[`docs/tls-fingerprint.md`](./tls-fingerprint.md)

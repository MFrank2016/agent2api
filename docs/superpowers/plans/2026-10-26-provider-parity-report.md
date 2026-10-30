# Provider parity / probe report — 2026-10-26

Task 5.6 of the plugin architecture migration. Scope: validate the plugin pack against the
gateway after built-in provider implementations were removed.

## 结论摘要

| 检查 | 结果 |
| --- | --- |
| 插件协议握手 / 全链路（`verify-plugin-host.mjs`） | **28 / 28 通过** |
| 差分 parity（`parity-check.mjs`） | **122 项检查，0 失败**（仅覆盖 3 个 id） |
| 能力探测（`probe-provider.mjs`）— demo | **通过**（mock 上游；必需项全过） |
| 能力探测 — workbuddy | **阻塞**：上游 401（无真实账号） |
| 能力探测 — nous | **阻塞**：上游 401（无真实账号） |
| 其余 30 个真实 provider | **未运行**：缺少真实上游账号 / 本机客户端登录态 |

总体判断：插件加载、握手、转发构造、错误分类、模型目录、usage、流式与工具调用链路
在 demo 上端到端可用；parity 对已有冻结期望的 3 家全部通过。真实上游的能力探测被
**账号**阻断，不是代码失败。

## 环境与方法

- 网关：worktree `migrate/plugin-arch`，`cargo build -p agent2api-server`（debug，复用主仓库 target）。
- 数据目录：`/tmp/parity-home`（全新，无历史数据）。
- 监听：`127.0.0.1:13065`（3065 被本机 docker-proxy 占用）。
- 认证：`AGENT2API_PROXY_API_KEY=sk-dev-parity`。
- 插件包：`/tmp/Agent2Providers/dist/agent2api-providers-0.2.1-x86_64-unknown-linux-gnu.zip`
  （插件仓库 HEAD `1a50512`），通过 `POST /api/plugins/install` 安装。
- demo 的 `default_enabled` 为 `false`，安装后通过 `POST /api/plugins/provider`
  显式启用，否则 `demo` 不参与握手。
- demo 的转发目标由 `DEMO_UPSTREAM=http://127.0.0.1:28080` 指向一个临时 mock 上游
  （`/tmp/mock-upstream.mjs`，未入库），用于验证网关 → 插件 → 上游 → 网关的完整链路。

安装后 `/api/plugins` 显示：`agent2api-providers 0.2.1`，**33 个 provider 全部
`loaded=true` / `enabled=true`**。

## 插件 provider 清单（33）

分类口径（与计划 Appendix 一致）：

- **upstream-only**：只出现在上游插件包 `v0.2.1`，迁移前的本地 `main` 没有。
- **local-only**：本地新增/移植（`nous` 来自 `main`，`tabbit` / `tabbit-intl` 来自 `feat/tabbit-provider`）。
- **common**：两边都有。

| # | provider id | 插件模块 | 集合 | 本次验证 |
| --- | --- | --- | --- | --- |
| 1 | `demo` | `demo` | upstream-only | verify 28/28；probe 通过（mock） |
| 2 | `commandcode` | `commandcode` | common | parity 通过 |
| 3 | `modelscope` | `modelscope` | upstream-only | parity 通过 |
| 4 | `modelscope-intl` | `modelscope` | upstream-only | parity 通过 |
| 5 | `orcaterm` | `orcaterm` | common | 未运行（无账号） |
| 6 | `loomy` | `loomy` | common | 未运行（无账号） |
| 7 | `lobsterai` | `lobsterai` | common | 未运行（无账号） |
| 8 | `raccoon` | `raccoon` | common | 未运行（无本机登录态） |
| 9 | `atomcode` | `atomcode` | upstream-only | 未运行（无账号） |
| 10 | `chatglm` | `chatglm` | common | 未运行（无账号） |
| 11 | `autoclaw` | `autoclaw` | common | 未运行（桌面端登录态仅 Windows） |
| 12 | `autoclaw-intl` | `autoclaw` | common | 未运行（同上） |
| 13 | `officeace` | `officeace` | common | 未运行（无账号） |
| 14 | `zcode` | `zcode` | common | 未运行（无账号） |
| 15 | `zcode-intl` | `zcode` | common | 未运行（无账号） |
| 16 | `workbuddy` | `workbuddy` | common | probe：context_window 通过；stream 401 阻塞 |
| 17 | `workbuddy-intl` | `workbuddy` | common | 未运行（无账号） |
| 18 | `cline-free` | `cline` | common | 未运行（无账号） |
| 19 | `cline-pass` | `cline` | common | 未运行（无账号） |
| 20 | `kuku` | `kuku` | common | 未运行（无本机 Cookie） |
| 21 | `monkeycode` | `monkeycode` | common | 未运行（无账号） |
| 22 | `monkeycode-intl` | `monkeycode` | common | 未运行（无账号） |
| 23 | `qoder` | `qoder` | common | 未运行（PAT / 设备登录需要真实账号） |
| 24 | `qoder-intl` | `qoder` | common | 未运行（同上） |
| 25 | `trae` | `trae` | common | 未运行（无账号） |
| 26 | `catpaw` | `catpaw` | common | 未运行（无本机登录态） |
| 27 | `accio` | `accio` | common | 未运行（无账号） |
| 28 | `accio-cn` | `accio` | common | 未运行（无账号） |
| 29 | `codearts` | `codearts` | common | 未运行（无 AK/SK/STS） |
| 30 | `antigravity` | `antigravity` | common | 未运行（无账号） |
| 31 | `tabbit` | `tabbit` | local-only | 未运行（Cookie/JWT 需真实账号，normalize 会联网校验） |
| 32 | `tabbit-intl` | `tabbit` | local-only | 未运行（同上） |
| 33 | `nous` | `nous` | local-only | probe：stream 401 阻塞 |

集合统计：**upstream-only 4**（`atomcode`、`demo`、`modelscope`、`modelscope-intl`）、
**local-only 3**（`nous`、`tabbit`、`tabbit-intl`）、**common 26**。

## 运行结果明细

### 1. 插件协议验证

```bash
node scripts/verify-plugin-host.mjs --exe /tmp/Agent2Providers/target/release/agent2api-providers
```

结果：**通过 28/28**。覆盖握手、能力申报、登录方式、无状态转发、控制面
（models / usage / checkin）、错误分类、reasoning patch、账号 normalize、登录、
流式 + 背压、取消、错误路径、崩溃恢复。

### 2. 差分 parity

```bash
# 插件二进制不在仓库根目录的 agent2api-providers/ 下，用临时目录 + 符号链接运行，
# 不改动网关/插件源码：
mkdir -p /tmp/parity/scripts
cp scripts/parity-check.mjs /tmp/parity/scripts/
ln -sfn /tmp/Agent2Providers /tmp/parity/agent2api-providers
node /tmp/parity/scripts/parity-check.mjs
```

结果：**122 项检查，0 项失败**。

适配说明：内置实现已从网关删除，脚本里的「内置期望值」是**冻结常量**，因此它现在的
实际含义是「插件输出 vs 迁移前冻结契约」，不再做真正的双实现差分。当前常量只覆盖：

- `commandcode`
- `modelscope`
- `modelscope-intl`

即 **3/33** 个 id 有 parity 基线；其余 30 个 id 没有差分基线（见「风险与缺口」）。

### 3. 能力探测 — demo（mock 上游）

```bash
node scripts/probe-provider.mjs --base http://127.0.0.1:13065 \
  --key sk-dev-parity --provider demo --model demo-fast --json
```

按目录声明运行：**pass 6 / fail 0 / skip 2 / info 1**，必需项全过。

- `stream`、`tool_call`、`tool_roundtrip`、`parallel_tools`、`json_schema`、`usage` 通过；
- `image`、`reasoning` 因 demo 目录声明不支持而跳过；
- `context_window` 为 info（demo 目录没有 `max_input_tokens`）。

强制全测（`--force`）：**pass 8 / fail 0 / info 1**，包括图片与 reasoning。
这证明网关 → 插件 → 上游 → 网关的请求构造、SSE 折帧、工具调用、工具结果回灌、
usage 透传都工作。

### 4. 能力探测 — 真实 provider（受账号阻断）

用无真实凭证的占位账号验证到「请求确实到达上游」为止：

| provider / model | 结果 |
| --- | --- |
| `workbuddy` / `glm-5.3` | `context_window` 通过（1,000,000 / 64,000）；`stream` 失败：**上游 401 Authorization Required（APISIX）** |
| `nous` / `nous/welcome` | `stream` 失败：**上游 401 API key invalid / blocked / out of funds**；`context_window` info |

两家的失败都发生在**上游鉴权**，不是网关或插件构造错误；模型目录、provider 归属
（`owned_by`）、请求发出均已被验证。

## 阻塞项（精确原因）

1. **真实上游账号缺失** — 29 个真实 provider（除 demo 外）需要各自的登录态：
   - WorkBuddy / Qoder / Trae / Cline / Accio / CodeArts / Antigravity 等需要真实
     access token、PAT、OAuth 或设备授权；
   - Tabbit 的 `normalize` 会在添加账号时联网校验 base-info，假 Cookie 无法落库；
   - Qoder 的 PAT 也需要真实校验；
   - CatPaw / KukuAI / AutoClaw / 小浣熊依赖本机客户端登录态（当前 Linux 环境没有）。
2. **parity 基线覆盖不足** — `parity-check.mjs` 只冻结了 3 个 id 的期望值，无法覆盖
   其余 30 个 id。内置实现删除后，也不存在可对照的第二实现。
3. **桌面端 / Windows 专属登录态** — AutoClaw 桌面端登录态仅 Windows；CatPaw、KukuAI、
   小浣熊需要本机客户端文件。本次 Linux headless 环境无法提供。
4. **测试门禁未全绿** — `cargo test -p agent2api-server` 仍为 397 passed / 2 failed
   （均为插件迁移导致的 `MissingPluginAdapter` 行为变化，非本次 parity/probe 引入）。

## 风险与缺口

- **覆盖缺口**：33 个 id 中只有 3 个有 parity 基线、1 个（demo）跑通完整 probe。
  真实上游的 chat / stream / usage / balance / checkin / login 尚未逐家验证。
- **demo 不是真实上游**：本次 demo 的通过依赖 mock 上游，只能证明协议与转发链路，
  不能证明任何真实厂商的适配正确性。
- **建议**：为每家 provider 在插件仓库补「冻结请求计划」单测（类似 `parity-check`
  的常量），把 parity 从 3 家扩到 33 家；真实账号探测放到有账号的发布验收环境执行。

## 复现命令

```bash
# 1. 构建网关
cd desktop-tauri/src-tauri && cargo build -p agent2api-server

# 2. 起网关（临时数据目录）
AGENT2API_PROXY_HOME=/tmp/parity-home \
AGENT2API_HOST=127.0.0.1 AGENT2API_PROXY_PORT=13065 \
AGENT2API_PROXY_API_KEY=sk-dev-parity \
DEMO_UPSTREAM=http://127.0.0.1:28080 \
target/debug/agent2api-server

# 3. 安装插件
curl -X POST http://127.0.0.1:13065/api/plugins/install \
  -H 'authorization: Bearer sk-dev-parity' \
  -H 'content-type: application/zip' \
  --data-binary @/tmp/Agent2Providers/dist/agent2api-providers-0.2.1-x86_64-unknown-linux-gnu.zip

# 4. 启用 demo 并添加 demo 账号
curl -X POST http://127.0.0.1:13065/api/plugins/provider \
  -H 'authorization: Bearer sk-dev-parity' -H 'content-type: application/json' \
  -d '{"id":"agent2api-providers","provider":"demo","enabled":true}'
curl -X POST http://127.0.0.1:13065/api/accounts \
  -H 'authorization: Bearer sk-dev-parity' -H 'content-type: application/json' \
  -d '{"provider":"demo","token":"demo-parity-token","name":"parity-demo"}'

# 5. probe
node scripts/probe-provider.mjs --base http://127.0.0.1:13065 \
  --key sk-dev-parity --provider demo --model demo-fast --json
```

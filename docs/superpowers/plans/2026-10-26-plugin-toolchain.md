# 插件工具链基线

- **日期**：2026-10-26
- **关联计划**：`.omo/plans/2026-10-26-plugin-architecture-migration.md` Task 0.3

## 1. 仓库与版本

| 项 | 值 |
|---|---|
| 仓库 | `ProvidersPlugin/Agent2Providers` |
| 固定 tag | `v0.2.1` |
| HEAD | `d4e532c27f56548a1285c0130f48914f3fad70ae` |
| 许可证 | **AGPL-3.0**（`LICENSE` + `license-file`） |
| workspace | `crates/providers`、`crates/provider-protocol`、`crates/provider-sdk` |
| Rust MSRV | 1.88 |
| Node | >= 18.17（CI 使用 22） |
| 网关契约依赖 | `agent2api-provider-protocol` v0.2.1 |

## 2. pack.json 基线

```json
{
  "id": "agent2api-providers",
  "name": "Agent2API 官方提供商",
  "version": "0.2.1",
  "protocol": 1,
  "providers": 30
}
```

30 个 provider id（含 `demo`；真实 provider 29 个，含国内/国际版本）：

```text
demo, commandcode, modelscope, modelscope-intl, orcaterm, loomy, lobsterai,
raccoon, atomcode, chatglm, autoclaw, autoclaw-intl, officeace, zcode,
zcode-intl, workbuddy, workbuddy-intl, cline-free, cline-pass, kuku,
monkeycode, monkeycode-intl, qoder, qoder-intl, trae, catpaw, accio,
accio-cn, codearts, antigravity
```

实现目录（22 个模块 + `demo`）：

```text
accio, antigravity, atomcode, autoclaw, catpaw, chatglm, cline, codearts,
commandcode, demo, kuku, lobsterai, loomy, modelscope, monkeycode, officeace,
orcaterm, qoder, raccoon, trae, workbuddy, zcode
```

## 3. 平台与产物

`pack.json.entry` 声明 5 个 target：

| Target triple | 覆盖 |
|---|---|
| `x86_64-pc-windows-msvc` | Windows x64 |
| `x86_64-apple-darwin` | macOS Intel |
| `aarch64-apple-darwin` | macOS Apple Silicon |
| `x86_64-unknown-linux-gnu` | Linux / Docker amd64 |
| `aarch64-unknown-linux-gnu` | Docker arm64 |

产物：`dist/agent2api-providers-<version>-<target>.zip`，内含 `pack.json` + `bin/agent2api-providers[.exe]`。

## 4. 构建 / 验证 / 签名

```bash
# 构建当前平台
cargo build --release -p agent2api-providers

# 打包（会顺带构建；--skip-build 只打包已有二进制）
node scripts/pack.mjs
node scripts/pack.mjs --skip-build --target x86_64-unknown-linux-gnu

# 协议端到端验证
node scripts/verify.mjs --exe target/release/agent2api-providers

# 签名
node scripts/sign.mjs keygen
node scripts/sign.mjs sign --zip dist/*.zip
```

`verify.mjs` 覆盖：`init` 握手、`chat`、`models.list`、`usage.query`、`checkin.claim`、`account.normalize`、`login.start/poll`、`conversation`（head → chunk → end）、`credit` 背压、`abort`、回调、崩溃恢复。

## 5. 与本 fork 的关键差异：HTTP 栈

| 项 | 本 fork | 插件仓库 v0.2.1 |
|---|---|---|
| 出站 HTTP | `wreq` + BoringSSL | **`reqwest` + rustls** |
| WebSocket | `wreq` / 现有栈 | `tokio-tungstenite` + rustls |
| 影响 | TLS/HTTP2 指纹 | 插件 provider 默认没有指纹 |

结论：要保留本 fork 的 TLS 指纹，**不能只改网关 `egress.rs`**；必须把 `Agent2Providers` 的 22 个 provider 从 `reqwest` 迁到 `wreq`，或在插件 SDK 提供统一的 `client_for(proxy)` 并逐家替换。计划 Task 3.1 已补上这项工作，估算 **+5–10 人天**。

## 6. 构建状态

- 插件仓库已克隆到 `/tmp/Agent2Providers`，HEAD `d4e532c2`。
- `cargo build --release -p agent2api-providers`：**成功**，耗时 68m12s，二进制 9.7MB；2 个 `dead_code` warning（chatglm 的 `COOKIE_*` 常量），不影响运行。
- `node scripts/verify.mjs --exe target/release/agent2api-providers`：**28/28 通过**；30 个 provider 握手成功，覆盖 chat / models / usage / checkin / account / login / conversation / credit / abort / 崩溃恢复。
- 构建耗时受机器高负载影响（load 9+、swap 1.9G）；CI 上应明显更快。

# TLS / HTTP2 指纹模拟（wreq）

> 状态：已落地。默认 `chrome`，可用 `AGENT2API_TLS_EMULATION` 关闭或换档。

## 为什么需要这一层

上游（WorkBuddy / Codex / Claude 等）除了看请求头与内容，还会看 **TLS ClientHello**
（JA3 / JA4）与 **HTTP/2 帧参数**。`rustls` 的默认栈是一个非常好认的「非浏览器」
指纹：同一套 cipher suite 顺序、同一组扩展、没有 GREASE。改造前本项目就是这套栈，
因此在「只按指纹拦」的批次里会整批命中。

CLIProxyAPI 之所以在这一项上领先，就是因为它用 uTLS 复刻了 Chrome 与 Claude Code
的 ClientHello。Rust 侧没有 uTLS，等价能力只能来自 BoringSSL 系客户端。

## 备选与结论

| 方案 | TLS 后端 | 指纹能力 | 结论 |
|---|---|---|---|
| `reqwest` + `rustls`（改造前） | rustls | 无 | 保留不了；这一层就是缺口 |
| **`wreq` 6.0.0-rc.31** | BoringSSL | 100+ 浏览器档位，Client 级 + 请求级 `emulation` | **采用** |
| `chromimic` 0.12.1 | BoringSSL | Chrome / OkHttp | 备选；API 与 reqwest 差异更大 |
| `rquest` | BoringSSL | Chrome 等 | crates.io 已不可用（0.0.0 占位） |

选 `wreq` 的直接理由：它是 `reqwest 0.12` 的分支，`Client` / `Response` /
`RequestBuilder` / `Proxy` / `header` / `redirect` 的形状一致，全仓只需把
`reqwest::` 换成 `wreq::`；`system-proxy`、`socks`、`stream`、`json`、`gzip`、
`brotli`、`charset`、`form` 这些原有用到的能力它都有。

## 构建前置

BoringSSL 由 `btls-sys` 的 build script 用 CMake 编译，bindgen 需要 libclang。

```bash
# Debian / Ubuntu
sudo apt-get install -y cmake nasm libclang-dev
# macOS
brew install cmake nasm llvm && export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
# Windows（runner 自带 cmake / nasm / LLVM）
set LIBCLANG_PATH=C:\Program Files\LLVM\bin
```

CI 的三条构建链（`build.yml` 的 macOS / Windows / Linux、`warm-cache.yml`、
`Dockerfile` 的 builder 阶段）都已加上这些依赖。冷编译 BoringSSL 约 6 分钟
（本机 8 核实测），之后走缓存。

## 配置

| 环境变量 | 取值 | 默认 | 说明 |
|---|---|---|---|
| `AGENT2API_TLS_EMULATION` | `off` / `safari` / `firefox` / `edge` / `chrome` | `chrome` | 出口 Client 的指纹档位 |

实现位置：`core/egress.rs` 的 `apply_emulation()`，作用在**所有**出口
（直连 / 指定代理 / 系统代理）的 `ClientBuilder` 上。`off` 用于排障对比：
怀疑「指纹模拟本身触发风控」时，关掉它就能区分是内容问题还是握手问题。

档位对应：`chrome` → Chrome149，`safari` → Safari26，`firefox` → Firefox151，
`edge` → Edge148（`wreq-util` 的 `Emulation` 枚举）。

## 风险与取舍

1. **`6.0.0-rc.31` 是预发布**：Cargo.toml 锁死版本；升级前必须重跑全量测试与
   一次真实上游回归。
2. **两套 TLS 栈**：HTTP 走 BoringSSL，MonkeyCode 的 WebSocket 仍走
   `tokio-tungstenite` + rustls（`wss://` 链路）。两者各自只服务自己的连接，
   不共享会话；这不是缺陷，但排障时要分清是哪条链路。
3. **构建变重**：源码构建与 CI 都需要 cmake / nasm / libclang；Docker 镜像的
   builder 阶段已装，最终镜像不含这些工具。
4. **全局指纹 vs 分家指纹**：当前是所有出口统一一档。CLIProxyAPI 的做法是按
   provider 分档（Chrome / Claude Code）。若后续发现某家对「浏览器指纹 + 桌面
   UA」的组合更敏感，再把 `apply_emulation` 改成按 provider 取档即可——入口
   已经收敛在 `egress.rs` 一处。
5. **许可证**：`wreq` 为 Apache-2.0，与本项目的 MIT + 附加声明兼容。

## 验证记录

- `cargo check -p agent2api-server`：通过。
- `cargo test -p agent2api-server --lib`：**763 passed / 0 failed / 11 ignored**。
- BoringSSL 冷编译：本机 6 分 06 秒（`cargo check` 全量）。
- 未验证：Windows / macOS 实机构建（本机为 Linux；CI 已补依赖，需在 CI 上确认）。

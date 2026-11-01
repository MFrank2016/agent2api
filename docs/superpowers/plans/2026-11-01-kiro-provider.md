# Kiro Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `ProvidersPlugin/Agent2Providers` 里新增 `kiro` provider，让 Agent2API 网关能用一个 `ksk_` API Key 账号完成流式对话、拉取模型目录、查询余额；并在网关侧解除 `kiro` 的停用状态。

**Architecture:** 网关侧只做「解禁 + 让路」：`adapter_for()` 恒返回占位，实现全部在插件仓。插件实现 `Provider` trait 的 `conversation` 分支：自己拼 Kiro 的 `generateAssistantResponse` 请求、自己解码 AWS Event Stream 二进制帧、自己产出 OpenAI Chat SSE 帧（`UpstreamKind::Chat`），与 `kuku` 同一范式。`is_stateful = true`，所以必须同时实现 `classify`。

**Tech Stack:** Rust 2021（workspace `rust-version = 1.88`，实测工具链 1.99）、`reqwest`（rustls）、`serde_json`、`tokio`、`futures`。无新增依赖。

**Spec:** `docs/superpowers/specs/2026-11-01-kiro-provider.md`

## Global Constraints

- 插件 crate 顶部有 `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` —— 生产代码里**不得**出现 `unwrap` / `expect` / `panic!`；需要兜底时用 `match` 或 `if let`。
- 网关 `adapter_for()` 的注释写着「**不要**在这里加任何真实现」—— 本计划不在网关里写任何 Kiro 上游调用代码。
- 所有出网走 `crate::http::client(proxy, gateway_proxy)`，不得自己 `reqwest::Client::new()`（代理与超时口径统一）。
- `user-agent` 必须是 `aws-sdk-js/3.738.0 KiroIDE`（协议常量）。
- 账号公开形态（`public`）**绝不能**含 access token / API Key 本体，只留 `tokenTail` 与「有没有令牌」。
- 提交信息用中文，格式 `feat(kiro): …` / `fix(kiro): …` / `test(kiro): …`；署名按 `release-flow` skill。
- 两个仓库都要 `cargo check` 0 error 才算完成：网关 `cargo check -p agent2api-server`，插件 `cargo check -p agent2api-providers`。
- 每个 Task 结束时工作区必须能编译（不允许「先写一堆再一起修」）。

## Review Focus

实现者最容易在这五处出错，每个都在对应 Task 里配了测试：

1. **Event Stream 分片对齐** —— 一次 `bytes_stream().next()` 可能拿到半帧或两帧半。忘记保留残帧的症状是「长回答偶尔少最后一句」。
2. **history 交替与 198 条上限** —— 不补 padding 会被上游 400；不折 oldest 会在长对话里突然 `Input is too long.`，而它**不该**被当成账号限额去冷却账号。
3. **`Input is too long.` 的错误归属** —— 它必须是 `Fatal` 且不罚账号。判成 `QuotaLimited` 会让一次长上下文请求把所有账号轮流冷却一遍。
4. **API Key 账号不续期** —— `refresh` 能力位对纯 API Key 账号必须是「跳过」而不是「失败」，否则后台凭证维护每轮刷一次日志。
5. **`toolUseId` 回灌校验** —— 对不上 history 的工具结果必须降级成文本，否则整条请求 400 `Invalid tool use format.`。

---

## 文件结构

### 插件仓 `ProvidersPlugin/Agent2Providers`（主要工作量）

```
crates/providers/src/providers/kiro/
  mod.rs           Provider trait 实现、meta 申报、能力位、账号规范化
  endpoints.rs     端点与 region 计算（三条 URL 规则）
  auth.rs          API Key 解析、鉴权头、refresh token 续期
  eventstream.rs   AWS Event Stream 增量解码器（纯函数，带单测）
  wire.rs          OpenAI chat body → Kiro generateAssistantResponse 请求体
  stream.rs        Kiro 事件帧 → OpenAI Chat SSE 帧（含工具调用）
  models.rs        ListAvailableModels + ListAvailableProfiles
  usage.rs         getUsageLimits 归一化
```

### 网关仓 `agent2api`（小改动）

```
desktop-tauri/src-tauri/server/src/server/core/providers/availability.rs   解禁 kiro
desktop-tauri/ui-shared/provider-availability.ts                           解禁 kiro
desktop-tauri/i18n/i18n-src/{en,ja,ko,pt-BR,zh-Hant}/add-account.json      删除三条孤儿键
README.md + docs/README.{zh-CN,zh-Hant,ja,ko,pt-BR}.md                      支持矩阵加一行
```

---

### Task 1: 网关侧解禁 kiro

**Files:**

- Modify: `desktop-tauri/src-tauri/server/src/server/core/providers/availability.rs:11-25`
- Modify: `desktop-tauri/ui-shared/provider-availability.ts:2-21`
- Test: `desktop-tauri/src-tauri/server/src/server/core/providers/availability.rs`（文件末尾追加 `#[cfg(test)]`）

**Interfaces:**

- Consumes: 无
- Produces: `is_suspended("kiro") == false`；`PLUGIN_RUNTIME_IDS` 长度 13 → 12

- [ ] **Step 1: 写失败测试**

在 `availability.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiro_is_no_longer_suspended() {
        assert!(!is_suspended("kiro"), "kiro 已实现插件，必须可添加");
    }

    #[test]
    fn other_suspended_ids_are_unchanged() {
        for id in ["cursor", "devin", "factory", "zed", "grok", "opendesign", "nous-portal"] {
            assert!(is_suspended(id), "{id} 仍应保持停用");
        }
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/agent2api/desktop-tauri/src-tauri
cargo test -p agent2api-server availability::tests::kiro_is_no_longer_suspended
```

Expected: FAIL，断言信息「kiro 已实现插件，必须可添加」。

- [ ] **Step 3: 从停用清单移除 kiro**

`availability.rs` 改成（数组长度 13 → 12，删掉 `"kiro",` 一行）：

```rust
const PLUGIN_RUNTIME_IDS: [&str; 12] = [
    "cursor",
    "devin",
    "factory",
    "zed",
    "grok",
    "minimax-code",
    "minimax-code-global",
    "qwenwork",
    "qwenwork-enterprise",
    "kimi-work",
    "opendesign",
    "nous-portal",
];
```

同时把模块头注释补一句：

```rust
//! 停用路线的稳定身份；保留存量账号，不提供执行体或添加入口。
//!
//! `kiro` 于 2026-11 移出本表：插件仓已提供实现（`providers/kiro/`），
//! 网关侧只需放行，不引入任何内置转发代码。
```

`provider-availability.ts` 同步删掉 `'kiro',` 一行（`SUSPENDED_PROVIDER_IDS` 只影响前端过滤，长度无类型约束）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/agent2api/desktop-tauri/src-tauri
cargo test -p agent2api-server availability::tests
```

Expected: 2 passed。

- [ ] **Step 5: 前端类型检查**

```bash
cd /root/programming/github/agent2api/desktop-tauri/ui-islands && npx tsc --noEmit
```

Expected: 0 error。

- [ ] **Step 6: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/core/providers/availability.rs \
        desktop-tauri/ui-shared/provider-availability.ts
git commit -m "feat(kiro): 解除 kiro 停用状态，改由插件提供实现"
```

---

### Task 2: 插件仓身份登记（能编译、能列出、可添加空壳账号）

**Files:**

- Create: `crates/providers/src/providers/kiro/mod.rs`
- Modify: `crates/providers/src/providers/mod.rs`（`mod` 声明 + `registry()` 末尾一行）
- Modify: `crates/providers/Cargo.toml`（`all` feature + `kiro = []`）
- Modify: `pack.json`（`providers[]` 末尾一条）
- Modify: `README.md` / `README.zh-CN.md`（21 家 → 22 家）

**Interfaces:**

- Consumes: `agent2api_provider_sdk::{Provider, ProviderMeta}`
- Produces: `pub const ID: &str = "kiro";`、`pub struct KiroProvider;`

- [ ] **Step 1: 写最小实现（编译即测试）**

`crates/providers/src/providers/kiro/mod.rs`：

```rust
//! Kiro（AWS CodeWhisperer / Amazon Q Developer）接入。
//!
//! ── 上游形态 ────────────────────────────────────────────────
//! 非 OpenAI 兼容：请求体是自有 JSON，响应是 `vnd.amazon.eventstream`
//! 二进制帧。完整协议见 `docs/superpowers/specs/2026-11-01-kiro-provider.md`。
//!
//! ── 为什么自己翻译帧 ────────────────────────────────────────
//! 网关的 `UpstreamKind` 只有 Chat / Anthropic / Ndjson / Gemini /
//! ChatglmSnapshot 五种，没有 Event Stream。插件自己解码、自己产 OpenAI 帧、
//! 报 `UpstreamKind::Chat` —— 与 `kuku` 同一范式。
//!
//! ── 签到 / 领取：确认为 ✗ ───────────────────────────────────
//! 三份公开实现（Kiro-Go / kiro2cc-proxy / dsh-kiro）都没有签到接口；
//! `getUsageLimits` 是**查询**不是签到。`freeTrialInfo` 与 bonus 由上游
//! 被动发放，没有可调用的领取动作。故 `checkin` / `claims` 两位恒 false。
//! 排除过程记录于此，不是「还没做」。

mod auth;
mod endpoints;
mod eventstream;
mod models;
mod stream;
mod usage;
mod wire;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{Provider, ProviderMeta};

/// provider id（`pack.json` 里必须写同一个）。
pub const ID: &str = "kiro";

/// Kiro 的默认模型清单（没拉过远程目录时的兜底）。
///
/// 取值依据：`ListAvailableModels` 在免费档账号上的实测返回，
/// 只保留当前主力 Claude 系；远程刷新会覆盖这份。
fn default_models() -> Vec<serde_json::Value> {
    use serde_json::json;
    vec![
        json!({ "id": "claude-sonnet-4.5", "name": "Claude Sonnet 4.5", "context_length": 200000 }),
        json!({ "id": "claude-haiku-4.5", "name": "Claude Haiku 4.5", "context_length": 200000 }),
    ]
}

pub struct KiroProvider;

impl Provider for KiroProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "Kiro")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 上游是「一次请求一个 conversationState」的会话式协议
                stateful: true,
                // API Key 不续期；OAuth 账号支持，见 auth::refresh
                refresh: true,
                model_refresh: true,
                usage: true,
                // 见模块头：上游没有签到 / 领取动作
                checkin: false,
                claims: false,
                web_login: false,
                sse_model_rewrite: true,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Token {
                label: "Kiro API Key".into(),
                placeholder: "ksk_…（可带 |region 后缀，如 ksk_abc|us-east-1）".into(),
                extra_fields: vec![],
            }])
            .with_models(default_models())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: false,
                identifier: vec!["email".to_string()],
                expiry: vec!["expiresAt".to_string()],
                ..Default::default()
            })
    }
}
```

- [ ] **Step 2: 建空壳模块，保证可编译**

逐个创建（本 Task 只放最小可编译内容，实现留给后续 Task）：

`crates/providers/src/providers/kiro/endpoints.rs`：

```rust
//! Kiro 端点与 region 计算。三条 URL 规则见 spec 第 1 节。

/// Kiro CLI（API Key）的数据面。
pub fn api_key_base(region: &str) -> String {
    format!("https://runtime.{region}.kiro.dev/")
}

/// 默认 region（API Key 不带 `|region` 时用它）。
pub const DEFAULT_REGION: &str = "us-east-1";
```

其余文件先各放一行 `//! TODO(Task N)` —— 但 `mod.rs` 已经 `mod` 了它们，所以必须至少有内容。
`auth.rs` / `eventstream.rs` / `models.rs` / `stream.rs` / `usage.rs` / `wire.rs` 各写：

```rust
//! 见 `docs/superpowers/specs/2026-11-01-kiro-provider.md`。
```

- [ ] **Step 3: 注册**

`crates/providers/src/providers/mod.rs` 加（按字母序插在 `kuku` 之前）：

```rust
#[cfg(feature = "kiro")]
pub mod kiro;
```

`registry()` 末尾（在 `list` 返回前）加：

```rust
    #[cfg(feature = "kiro")]
    {
        list.push(Box::new(kiro::KiroProvider));
    }
```

`crates/providers/Cargo.toml`：`all = [ … ]` 里加 `"kiro"`，并新增

```toml
kiro = []
```

`pack.json` 的 `providers[]` 末尾加：

```json
{
  "id": "kiro",
  "label": "Kiro",
  "order": 22,
  "default_enabled": true
}
```

同时把 `pack.json` 顶层 `version` 递增一位（`0.2.1` → `0.2.2`），并同步 `Cargo.toml` 的 `[workspace.package] version`。

- [ ] **Step 4: 编译**

```bash
cd /root/programming/github/Agent2Providers
cargo check -p agent2api-providers --features all
```

Expected: 0 error。若报 `unused` 警告可忽略（后续 Task 会用到）。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/Cargo.toml crates/providers/src/providers/mod.rs \
        crates/providers/src/providers/kiro pack.json README.md README.zh-CN.md Cargo.toml
git commit -m "feat(kiro): 登记 kiro provider 身份与能力位"
```

---

### Task 3: AWS Event Stream 解码器

**Files:**

- Modify: `crates/providers/src/providers/kiro/eventstream.rs`（整体实现）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**

- Produces:
  - `pub struct FrameDecoder { … }`
  - `FrameDecoder::new() -> Self`
  - `FrameDecoder::push(&mut self, bytes: &[u8]) -> Result<Vec<KiroFrame>, String>`
  - `FrameDecoder::finish(&self) -> Result<(), String>`
  - `pub struct KiroFrame { pub event: String, pub message_type: String, pub payload: String }`

- [ ] **Step 1: 写失败测试**

`eventstream.rs` 末尾：

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 按 spec 第 2 节拼一帧（CRC 填 0，解码器不校验）。
    fn encode_frame(event: &str, message_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":event-type", event), (":message-type", message_type)] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total = 12 + headers.len() + payload.len() + 4;
        let mut frame = Vec::with_capacity(total);
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0, 0]);
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload.as_bytes());
        frame.extend_from_slice(&[0, 0, 0, 0]);
        frame
    }

    #[test]
    fn decodes_one_whole_frame() {
        let bytes = encode_frame("assistantResponseEvent", "event", r#"{"content":"hi"}"#);
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&bytes).expect("应能解出一帧");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "assistantResponseEvent");
        assert_eq!(frames[0].payload, r#"{"content":"hi"}"#);
        assert!(decoder.finish().is_ok());
    }

    #[test]
    fn buffers_a_frame_split_across_chunks() {
        let bytes = encode_frame("assistantResponseEvent", "event", r#"{"content":"hello"}"#);
        let (head, tail) = bytes.split_at(7);
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(head).expect("半帧不应报错").is_empty());
        let frames = decoder.push(tail).expect("补齐后应解出");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, r#"{"content":"hello"}"#);
    }

    #[test]
    fn decodes_two_frames_in_one_chunk() {
        let mut bytes = encode_frame("a", "event", r#"{"n":1}"#);
        bytes.extend_from_slice(&encode_frame("b", "event", r#"{"n":2}"#));
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&bytes).expect("应解出两帧");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].event, "b");
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let bytes = encode_frame("a", "event", r#"{"n":1}"#);
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&bytes[..10]).expect("半帧不报错").is_empty());
        assert!(decoder.finish().is_err(), "流在帧中途结束必须报错");
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut bytes = vec![0u8; 12];
        bytes[..4].copy_from_slice(&(MAX_FRAME_BYTES as u32 + 1).to_be_bytes());
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&bytes).is_err());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::eventstream
```

Expected: 编译失败（`FrameDecoder` 未定义）。

- [ ] **Step 3: 实现解码器**

`eventstream.rs`：

```rust
//! AWS Event Stream（`vnd.amazon.eventstream`）增量解码。
//!
//! 帧结构：12 字节 prelude（总长 / 头长 / prelude CRC）+ headers +
//! payload + 4 字节 message CRC。Kiro 的 header 值只有 UTF-8 string
//! （类型标记 7）。CRC 不校验：TLS 已保护传输，坏帧会在 JSON 解析处失败。

/// prelude 字节数。
const PRELUDE_BYTES: usize = 12;
/// 尾部 message CRC 字节数。
const MESSAGE_CRC_BYTES: usize = 4;
/// header 值类型：UTF-8 string。
const HEADER_TYPE_STRING: u8 = 7;
/// 单帧上限（AWS 规定 16 MiB）；超过视为流错位。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// 解出的一帧。
#[derive(Clone, Debug, PartialEq)]
pub struct KiroFrame {
    /// `:event-type` 的值（如 `assistantResponseEvent`）。
    pub event: String,
    /// `:message-type` 的值（`event` / `exception`）。
    pub message_type: String,
    /// 未解析的 payload（JSON 文本）。
    pub payload: String,
}

/// 增量解码器：喂字节，出完整帧；最后半帧留在内部。
#[derive(Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段字节，返回其中**完整**的帧。
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<KiroFrame>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame()? {
            frames.push(frame);
        }
        Ok(frames)
    }

    /// 收尾：残留字节说明流被截断，必须报错而不是吐出半帧。
    pub fn finish(&self) -> Result<(), String> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Kiro 事件流在帧中途结束（残留 {} 字节）",
                self.buffer.len()
            ))
        }
    }

    fn next_frame(&mut self) -> Result<Option<KiroFrame>, String> {
        if self.buffer.len() < PRELUDE_BYTES {
            return Ok(None);
        }
        let total = read_u32(&self.buffer, 0) as usize;
        let headers_len = read_u32(&self.buffer, 4) as usize;
        if total > MAX_FRAME_BYTES {
            return Err(format!("Kiro 事件流单帧超过 {MAX_FRAME_BYTES} 字节，流已错位"));
        }
        let minimum = PRELUDE_BYTES + MESSAGE_CRC_BYTES;
        if total < minimum || headers_len > total - minimum {
            return Err("Kiro 事件流帧长度非法".to_string());
        }
        if self.buffer.len() < total {
            return Ok(None);
        }
        let frame: Vec<u8> = self.buffer.drain(..total).collect();
        let headers_end = PRELUDE_BYTES + headers_len;
        let payload_end = total - MESSAGE_CRC_BYTES;
        let headers = decode_headers(&frame, PRELUDE_BYTES, headers_end)?;
        Ok(Some(KiroFrame {
            event: headers.get(":event-type").cloned().unwrap_or_default(),
            message_type: headers.get(":message-type").cloned().unwrap_or_default(),
            payload: String::from_utf8_lossy(&frame[headers_end..payload_end]).to_string(),
        }))
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_be_bytes(raw)
}

fn decode_headers(
    frame: &[u8],
    mut offset: usize,
    end: usize,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut headers = std::collections::BTreeMap::new();
    while offset < end {
        if offset + 1 > end {
            return Err("Kiro 事件流 header 名长度越界".to_string());
        }
        let name_len = frame[offset] as usize;
        offset += 1;
        if offset + name_len + 3 > end {
            return Err("Kiro 事件流 header 名越界".to_string());
        }
        let name = String::from_utf8_lossy(&frame[offset..offset + name_len]).to_string();
        offset += name_len;
        let value_type = frame[offset];
        offset += 1;
        if value_type != HEADER_TYPE_STRING {
            return Err(format!(
                "Kiro 事件流 header `{name}` 的值类型 {value_type} 不受支持"
            ));
        }
        let value_len = u16::from_be_bytes([frame[offset], frame[offset + 1]]) as usize;
        offset += 2;
        if offset + value_len > end {
            return Err("Kiro 事件流 header 值越界".to_string());
        }
        headers.insert(
            name,
            String::from_utf8_lossy(&frame[offset..offset + value_len]).to_string(),
        );
        offset += value_len;
    }
    Ok(headers)
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::eventstream
```

Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro/eventstream.rs
git commit -m "feat(kiro): 实现 AWS Event Stream 增量解码器"
```

---

### Task 4: API Key 账号（解析、鉴权头、不续期）

**Files:**

- Modify: `crates/providers/src/providers/kiro/auth.rs`（整体实现）
- Modify: `crates/providers/src/providers/kiro/endpoints.rs`（补 `region_of`）
- Modify: `crates/providers/src/providers/kiro/mod.rs`（`normalize_account` / `account_view` / `account_identity` / `ensure_token` / `token_expiring`）
- Test: `auth.rs` 的 `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `pub struct KiroCredential { pub api_key: String, pub region: String, pub kind: CredentialKind }`
  - `pub enum CredentialKind { ApiKey, OAuth }`
  - `pub fn parse_api_key(raw: &str) -> Result<KiroCredential, String>`
  - `pub fn auth_headers(cred: &KiroCredential) -> Vec<(String, String)>`
  - `pub fn identity_of(cred: &KiroCredential) -> String`

- [ ] **Step 1: 写失败测试**

`auth.rs` 末尾：

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_api_key_with_default_region() {
        let cred = parse_api_key("ksk_abc123").expect("应能解析");
        assert_eq!(cred.api_key, "ksk_abc123");
        assert_eq!(cred.region, "us-east-1");
    }

    #[test]
    fn parses_api_key_with_region_suffix() {
        let cred = parse_api_key("ksk_abc123|eu-west-1").expect("应能解析");
        assert_eq!(cred.api_key, "ksk_abc123");
        assert_eq!(cred.region, "eu-west-1");
    }

    #[test]
    fn rejects_empty_key() {
        assert!(parse_api_key("   ").is_err());
    }

    #[test]
    fn rejects_non_ksk_prefix() {
        assert!(parse_api_key("sk-other").is_err());
    }

    #[test]
    fn api_key_headers_carry_token_type_and_ua() {
        let cred = parse_api_key("ksk_abc").expect("应能解析");
        let headers = auth_headers(&cred);
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(find("tokentype"), Some("API_KEY"));
        assert_eq!(find("user-agent"), Some(KIRO_USER_AGENT));
        assert_eq!(find("authorization"), Some("Bearer ksk_abc"));
    }

    #[test]
    fn identity_is_stable_and_does_not_leak_the_key() {
        let cred = parse_api_key("ksk_abc").expect("应能解析");
        let identity = identity_of(&cred);
        assert!(identity.starts_with("kiro-api-"));
        assert!(!identity.contains("ksk_abc"));
        assert_eq!(identity, identity_of(&parse_api_key("ksk_abc").expect("应能解析")));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::auth
```

Expected: 编译失败（`parse_api_key` 未定义）。

- [ ] **Step 3: 实现 auth.rs**

```rust
//! Kiro 凭证：API Key 解析、鉴权头、续期。
//!
//! ── 两种凭证形态 ────────────────────────────────────────────
//! `ApiKey`（`ksk_…`）：不续期、不需要 profileArn、走 `runtime.{region}.kiro.dev`。
//! `OAuth`：access/refresh token，走 `q.{region}` 或 `codewhisperer.{region}`，
//! 续期见 `refresh`（Task 10 实现）。
//!
//! ── 为什么 API Key 也要记 region ─────────────────────────────
//! `runtime.{region}.kiro.dev` 的 region 是数据面，不是鉴权面；用户粘贴的
//! `ksk_…|eu-west-1` 后半段就是它。缺省 `us-east-1`。

use super::endpoints::DEFAULT_REGION;

/// 协议常量：不点名官方 IDE 的 UA 会被拒（spec 第 1 节）。
pub const KIRO_USER_AGENT: &str = "aws-sdk-js/3.738.0 KiroIDE";

/// 凭证种类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CredentialKind {
    ApiKey,
    OAuth,
}

/// 一份 Kiro 凭证。
#[derive(Clone, Debug)]
pub struct KiroCredential {
    /// 真正放进 `Authorization: Bearer` 的令牌。
    pub access_token: String,
    /// API Key 本体（`CredentialKind::ApiKey` 时与 `access_token` 相同）。
    pub api_key: Option<String>,
    /// OAuth refresh token（`CredentialKind::OAuth` 时才有）。
    pub refresh_token: Option<String>,
    /// 数据面 region。
    pub region: String,
    /// 凭证种类。
    pub kind: CredentialKind,
}

/// 解析用户粘贴的 API Key（`ksk_…` 或 `ksk_…|region`）。
pub fn parse_api_key(raw: &str) -> Result<KiroCredential, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Kiro API Key 不能为空".to_string());
    }
    let (key, region) = match trimmed.split_once('|') {
        Some((key, region)) => (key.trim(), region.trim()),
        None => (trimmed, ""),
    };
    if key.is_empty() {
        return Err("Kiro API Key 不能为空".to_string());
    }
    if !key.starts_with("ksk_") {
        return Err("Kiro API Key 应以 `ksk_` 开头".to_string());
    }
    Ok(KiroCredential {
        access_token: key.to_string(),
        api_key: Some(key.to_string()),
        refresh_token: None,
        region: if region.is_empty() {
            DEFAULT_REGION.to_string()
        } else {
            region.to_string()
        },
        kind: CredentialKind::ApiKey,
    })
}

/// 鉴权与身份头（不含 `content-type` / `accept`，那两个由调用方按用途给）。
pub fn auth_headers(cred: &KiroCredential) -> Vec<(String, String)> {
    let mut headers = vec![
        ("authorization".to_string(), format!("Bearer {}", cred.access_token)),
        ("user-agent".to_string(), KIRO_USER_AGENT.to_string()),
        ("x-amz-user-agent".to_string(), KIRO_USER_AGENT.to_string()),
        ("x-amzn-kiro-agent-mode".to_string(), "vibe".to_string()),
    ];
    match cred.kind {
        CredentialKind::ApiKey => {
            headers.push(("tokentype".to_string(), "API_KEY".to_string()));
            headers.push((
                "x-amz-target".to_string(),
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse".to_string(),
            ));
            headers.push(("origin".to_string(), "KIRO_CLI".to_string()));
        }
        CredentialKind::OAuth => {}
    }
    headers
}

/// 账号稳定身份：由凭证派生，**不含密钥本体**。
pub fn identity_of(cred: &KiroCredential) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in cred.access_token.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("kiro-api-{hash:016x}")
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::auth
```

Expected: 6 passed。

- [ ] **Step 5: 接进 `mod.rs` 的账号方法**

在 `impl Provider for KiroProvider` 里补：

```rust
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        input: proto::NormalizeInput,
    ) -> agent2api_provider_sdk::SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let payload = match &input.source {
                proto::NormalizeSource::Manual { payload } => payload.clone(),
                proto::NormalizeSource::FileText { content, .. } => serde_json::from_str::<serde_json::Value>(content)
                    .map_err(|e| format!("文件不是合法 JSON: {e}"))?,
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("Kiro 不支持二进制文件导入".to_string())
                }
                proto::NormalizeSource::Desktop => {
                    return Err("Kiro 不支持读取本机客户端登录态".to_string())
                }
            };
            let raw = payload
                .get("apiKey")
                .or_else(|| payload.get("token"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let cred = auth::parse_api_key(raw)?;
            let identity = auth::identity_of(&cred);
            Ok(proto::NormalizedAccount {
                secret: serde_json::json!({
                    "accessToken": cred.access_token,
                    "apiKey": cred.api_key,
                    "region": cred.region,
                    "kind": "api_key",
                }),
                public: serde_json::json!({
                    "name": input.name.clone().unwrap_or_else(|| "Kiro".to_string()),
                    "email": identity,
                    "tokenTail": tail_of(&cred.access_token),
                    "hasToken": true,
                }),
                identity,
                desktop: false,
            })
        })
    }

    fn account_identity(&self, secret: &serde_json::Value) -> Result<String, String> {
        let cred = credential_from_secret(secret)?;
        Ok(auth::identity_of(&cred))
    }

    fn ensure_token<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        account: &'a str,
    ) -> agent2api_provider_sdk::SendFuture<'a, String> {
        Box::pin(async move {
            // API Key 不续期：账号里存的就是可直接用的令牌。
            // 需要读账号的调用点在 conversation 里做（那里有 ctx）。
            Ok(account.to_string())
        })
    }

    fn token_expiring<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        _account: &'a str,
    ) -> agent2api_provider_sdk::SendFuture<'a, bool> {
        // 判不出来就 false：宁可漏刷，也不要凭猜测打上游。
        Box::pin(async move { Ok(false) })
    }
```

并在 `mod.rs` 末尾加两个私有辅助：

```rust
/// 从落盘 secret 还原凭证。
fn credential_from_secret(secret: &serde_json::Value) -> Result<auth::KiroCredential, String> {
    let token = secret
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if token.is_empty() {
        return Err("账号里没有 Kiro 令牌，请重新添加".to_string());
    }
    let region = secret
        .get("region")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(endpoints::DEFAULT_REGION)
        .to_string();
    Ok(auth::KiroCredential {
        access_token: token.to_string(),
        api_key: secret
            .get("apiKey")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        refresh_token: secret
            .get("refreshToken")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        region,
        kind: auth::CredentialKind::ApiKey,
    })
}

/// 令牌尾部四位（界面显示用，绝不回显本体）。
fn tail_of(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}
```

- [ ] **Step 6: 编译 + 跑测试**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::
```

Expected: 全部通过，0 error。

- [ ] **Step 7: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro
git commit -m "feat(kiro): API Key 账号解析与鉴权头"
```

---

### Task 5: 请求序列化（OpenAI body → Kiro 请求体）

**Files:**

- Modify: `crates/providers/src/providers/kiro/wire.rs`（整体实现）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**

- Produces: `pub fn build(body: &serde_json::Value, model: &str, profile_arn: Option<&str>) -> Result<serde_json::Value, String>`
- 产出的 body 形状严格按 spec 第 3 节与 `WireRequest`：
  `{ profileArn?, conversationState: { chatTriggerType, conversationId, currentMessage, history? } }`

- [ ] **Step 1: 写失败测试**

`wire.rs` 末尾：

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_prompt_is_prepended_to_first_user_turn() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "你是助手" },
                { "role": "user", "content": "你好" }
            ]
        });
        let wire = build(&body, "claude-sonnet-4.5", None).expect("应能构造");
        let current = &wire["conversationState"]["currentMessage"]["userInputMessage"];
        assert!(current["content"].as_str().unwrap_or("").contains("你是助手"));
        assert!(current["content"].as_str().unwrap_or("").contains("你好"));
        assert_eq!(current["modelId"], "claude-sonnet-4.5");
    }

    #[test]
    fn last_user_turn_is_current_message_and_not_in_history() {
        let body = json!({
            "messages": [
                { "role": "user", "content": "第一轮" },
                { "role": "assistant", "content": "回答" },
                { "role": "user", "content": "第二轮" }
            ]
        });
        let wire = build(&body, "m", None).expect("应能构造");
        let history = wire["conversationState"]["history"].as_array().expect("应有 history");
        assert_eq!(history.len(), 2);
        assert!(history[0].get("userInputMessage").is_some());
        assert!(history[1].get("assistantResponseMessage").is_some());
        let current = &wire["conversationState"]["currentMessage"]["userInputMessage"]["content"];
        assert_eq!(current, "第二轮");
    }

    #[test]
    fn missing_turns_are_padded_so_history_alternates() {
        let body = json!({
            "messages": [
                { "role": "user", "content": "a" },
                { "role": "user", "content": "b" },
                { "role": "assistant", "content": "c" },
                { "role": "assistant", "content": "d" },
                { "role": "user", "content": "e" }
            ]
        });
        let wire = build(&body, "m", None).expect("应能构造");
        let history = wire["conversationState"]["history"].as_array().expect("应有 history");
        for (index, entry) in history.iter().enumerate() {
            let is_user = entry.get("userInputMessage").is_some();
            assert_eq!(is_user, index % 2 == 0, "第 {index} 条角色不符");
        }
    }

    #[test]
    fn history_is_capped_at_198_entries() {
        let mut messages = Vec::new();
        for index in 0..400 {
            messages.push(json!({
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "content": format!("m{index}")
            }));
        }
        let body = json!({ "messages": messages });
        let wire = build(&body, "m", None).expect("应能构造");
        let history = wire["conversationState"]["history"].as_array().expect("应有 history");
        assert!(history.len() <= MAX_HISTORY_ENTRIES, "history 超上限：{}", history.len());
    }

    #[test]
    fn tools_are_mapped_to_tool_specifications() {
        let body = json!({
            "messages": [{ "role": "user", "content": "读文件" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "读一个文件",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }]
        });
        let wire = build(&body, "m", None).expect("应能构造");
        let tools = &wire["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"];
        let tools = tools.as_array().expect("应有 tools");
        assert_eq!(tools[0]["toolSpecification"]["name"], "read_file");
        assert_eq!(tools[0]["toolSpecification"]["inputSchema"]["json"]["type"], "object");
    }

    #[test]
    fn profile_arn_is_only_present_when_given() {
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let without = build(&body, "m", None).expect("应能构造");
        assert!(without.get("profileArn").is_none());
        let with = build(&body, "m", Some("arn:aws:codewhisperer:us-east-1:1:profile/x")).expect("应能构造");
        assert_eq!(with["profileArn"], "arn:aws:codewhisperer:us-east-1:1:profile/x");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::wire
```

Expected: 编译失败（`build` 未定义）。

- [ ] **Step 3: 实现 wire.rs**

```rust
//! OpenAI chat body → Kiro `generateAssistantResponse` 请求体。
//!
//! 四条硬约束（spec 第 3 节，全部实测）：
//!   1. 没有 system 槽位 —— 系统提示词拼进第一条 user turn；
//!   2. 最后一轮 user 是 `currentMessage`，`history` 严格交替，缺档补 padding；
//!   3. history 上限 198 条，超出把最老的折成一条文本 turn；
//!   4. 工具结果挂 user turn，`toolUseId` 必须真实存在（对不上降级成文本）。
//!
//! padding 常量照抄官方客户端：user 用 `Continue`、assistant 用 `understood`。
//! **不要**用形似系统消息的标记 —— 模型会模仿并输出到正文。

use serde_json::{json, Map, Value};

/// history 上限（实测 198 过、200 报 `Input is too long.`）。
pub const MAX_HISTORY_ENTRIES: usize = 198;
/// user turn 的空档填充。
pub const CONTINUE_PADDING: &str = "Continue";
/// assistant turn 的空档填充。
pub const ACKNOWLEDGE_PADDING: &str = "understood";

/// 构造请求体。
pub fn build(body: &Value, model: &str, profile_arn: Option<&str>) -> Result<Value, String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "请求里没有 messages 数组".to_string())?;
    if messages.is_empty() {
        return Err("messages 为空".to_string());
    }

    // ① 拆出 system（可能多条，按顺序拼接）
    let mut system_parts: Vec<String> = Vec::new();
    let mut turns: Vec<(bool, String)> = Vec::new(); // (是否 user, 文本)
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = flatten_content(message.get("content"));
        match role {
            "system" | "developer" => system_parts.push(text),
            "assistant" => turns.push((false, text)),
            _ => turns.push((true, text)),
        }
    }
    if turns.is_empty() {
        return Err("messages 里没有 user / assistant 轮次".to_string());
    }

    // ② system 拼进第一条 user turn
    if !system_parts.is_empty() {
        let prefix = system_parts.join("\n\n");
        if let Some(first_user) = turns.iter_mut().find(|(is_user, _)| *is_user) {
            first_user.1 = format!("{prefix}\n\n{}", first_user.1);
        }
    }

    // ③ 拆 currentMessage 与 history
    let (_, current_text) = turns.pop().ok_or_else(|| "messages 为空".to_string())?;
    let mut history: Vec<(bool, String)> = turns;

    // ④ 补 padding 使 history 严格交替（起点必须是 user）
    if history.first().is_some_and(|(is_user, _)| !*is_user) {
        history.insert(0, (true, CONTINUE_PADDING.to_string()));
    }
    let mut normalized: Vec<(bool, String)> = Vec::with_capacity(history.len());
    for (is_user, text) in history {
        if let Some((last_is_user, _)) = normalized.last() {
            if *last_is_user == is_user {
                let filler = if is_user { CONTINUE_PADDING } else { ACKNOWLEDGE_PADDING };
                normalized.push((!is_user, filler.to_string()));
            }
        }
        normalized.push((is_user, text));
    }
    // 末尾必须是 assistant（currentMessage 是 user）
    if normalized.last().is_some_and(|(is_user, _)| *is_user) {
        normalized.push((false, ACKNOWLEDGE_PADDING.to_string()));
    }

    // ⑤ 超上限：把最老的折成一条文本 turn
    if normalized.len() > MAX_HISTORY_ENTRIES {
        let fold_count = normalized.len() - MAX_HISTORY_ENTRIES + 1;
        let folded: Vec<String> = normalized
            .drain(..fold_count)
            .map(|(is_user, text)| {
                format!("{}: {text}", if is_user { "user" } else { "assistant" })
            })
            .collect();
        normalized.insert(0, (true, folded.join("\n")));
        if normalized.get(1).is_some_and(|(is_user, _)| *is_user) {
            normalized.insert(1, (false, ACKNOWLEDGE_PADDING.to_string()));
        }
    }

    // ⑥ 组装
    let mut current = Map::new();
    current.insert("content".into(), Value::String(current_text));
    current.insert("modelId".into(), Value::String(model.to_string()));
    current.insert("origin".into(), Value::String("AI_EDITOR".to_string()));
    if let Some(context) = user_context(body) {
        current.insert("userInputMessageContext".into(), context);
    }

    let mut conversation = Map::new();
    conversation.insert("chatTriggerType".into(), Value::String("MANUAL".to_string()));
    conversation.insert(
        "conversationId".into(),
        Value::String(conversation_id(body)),
    );
    conversation.insert(
        "currentMessage".into(),
        json!({ "userInputMessage": Value::Object(current) }),
    );
    if !normalized.is_empty() {
        let entries: Vec<Value> = normalized
            .into_iter()
            .map(|(is_user, text)| {
                if is_user {
                    json!({ "userInputMessage": {
                        "content": text,
                        "modelId": model,
                        "origin": "AI_EDITOR"
                    }})
                } else {
                    json!({ "assistantResponseMessage": { "content": text } })
                }
            })
            .collect();
        conversation.insert("history".into(), Value::Array(entries));
    }

    let mut request = Map::new();
    if let Some(arn) = profile_arn.filter(|value| !value.is_empty()) {
        request.insert("profileArn".into(), Value::String(arn.to_string()));
    }
    request.insert("conversationState".into(), Value::Object(conversation));
    Ok(Value::Object(request))
}

/// 工具声明挂到 currentMessage 的 `userInputMessageContext.tools`。
fn user_context(body: &Value) -> Option<Value> {
    let tools = body.get("tools").and_then(Value::as_array)?;
    let mut mapped = Vec::new();
    for tool in tools {
        let function = tool.get("function").unwrap_or(tool);
        let name = function.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let parameters = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        mapped.push(json!({
            "toolSpecification": {
                "name": name,
                "description": description,
                "inputSchema": { "json": parameters }
            }
        }));
    }
    if mapped.is_empty() {
        return None;
    }
    Some(json!({ "tools": mapped }))
}

/// content 可能是字符串，也可能是 `[{type:"text",text:"…"}]` 数组。
fn flatten_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 会话 id：优先用客户端给的，其次随机一个。
fn conversation_id(body: &Value) -> String {
    if let Some(id) = body
        .get("conversation_id")
        .or_else(|| body.get("conversationId"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return id.to_string();
    }
    let mut raw = [0u8; 16];
    let _ = getrandom::getrandom(&mut raw);
    raw.iter().map(|byte| format!("{byte:02x}")).collect()
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::wire
```

Expected: 6 passed。若 `getrandom` 未在依赖里，`Cargo.toml` 已有 `getrandom = "0.2"`，无需新增。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro/wire.rs
git commit -m "feat(kiro): OpenAI 请求体到 Kiro 会话请求的序列化"
```

---

### Task 6: 帧翻译（Kiro 事件 → OpenAI Chat SSE）

**Files:**

- Modify: `crates/providers/src/providers/kiro/stream.rs`（整体实现）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `pub struct FrameTranslator { … }`
  - `FrameTranslator::new(model: &str) -> Self`
  - `FrameTranslator::feed(&mut self, frame: &KiroFrame) -> Vec<String>`（每项是一整帧 SSE 文本）
  - `FrameTranslator::finish(&mut self) -> Vec<String>`（收尾帧 + `[DONE]`）

- [ ] **Step 1: 写失败测试**

`stream.rs` 末尾：

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::providers::kiro::eventstream::KiroFrame;

    fn frame(event: &str, payload: &str) -> KiroFrame {
        KiroFrame {
            event: event.to_string(),
            message_type: "event".to_string(),
            payload: payload.to_string(),
        }
    }

    #[test]
    fn assistant_text_becomes_openai_delta() {
        let mut translator = FrameTranslator::new("claude-sonnet-4.5");
        let frames = translator.feed(&frame(
            "assistantResponseEvent",
            r#"{"content":"你好"}"#,
        ));
        assert_eq!(frames.len(), 1);
        assert!(frames[0].starts_with("data: "));
        assert!(frames[0].contains(r#""content":"你好""#));
        assert!(frames[0].contains(r#""model":"claude-sonnet-4.5""#));
    }

    #[test]
    fn metadata_becomes_finish_frame_with_usage() {
        let mut translator = FrameTranslator::new("m");
        let frames = translator.feed(&frame(
            "messageMetadataEvent",
            r#"{"tokenUsage":{"totalTokens":42}}"#,
        ));
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(r#""finish_reason":"stop""#));
        assert!(frames[0].contains(r#""total_tokens":42"#));
    }

    #[test]
    fn tool_use_events_accumulate_then_emit_one_call() {
        let mut translator = FrameTranslator::new("m");
        assert!(translator
            .feed(&frame(
                "toolUseEvent",
                r#"{"toolUseId":"t1","name":"read_file"}"#
            ))
            .is_empty());
        assert!(translator
            .feed(&frame(
                "toolUseEvent",
                r#"{"toolUseId":"t1","input":"{\"path\":"}"#
            ))
            .is_empty());
        let frames = translator.feed(&frame(
            "toolUseEvent",
            r#"{"toolUseId":"t1","input":"\"a.txt\"}","stop":true}"#,
        ));
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(r#""tool_calls""#));
        assert!(frames[0].contains(r#""name":"read_file""#));
        assert!(frames[0].contains("a.txt"));
    }

    #[test]
    fn finish_always_ends_with_done() {
        let mut translator = FrameTranslator::new("m");
        let frames = translator.finish();
        assert_eq!(frames.last().map(String::as_str), Some("data: [DONE]\n\n"));
    }

    #[test]
    fn exception_frames_become_an_error_payload() {
        let mut translator = FrameTranslator::new("m");
        let mut exception = frame("ValidationException", "{}");
        exception.message_type = "exception".to_string();
        let frames = translator.feed(&exception);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains("ValidationException"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::stream
```

Expected: 编译失败。

- [ ] **Step 3: 实现 stream.rs**

```rust
//! Kiro 事件帧 → OpenAI Chat SSE 帧。
//!
//! ── 为什么在这里翻译而不是交给网关 ──────────────────────────
//! 网关的 `UpstreamKind` 没有 Event Stream 这一档；插件自己解码二进制帧、
//! 自己产出 OpenAI 帧、报 `UpstreamKind::Chat`，与 `kuku` 同一范式。
//!
//! ── 工具调用的形状 ──────────────────────────────────────────
//! 上游把一次调用拆成多帧：首帧给 `name` + `toolUseId`，中间帧给 `input`
//! 片段（**字符串**，不是对象），末帧带 `stop: true`。本模块把片段拼成完整
//! JSON 后再发一个 `tool_calls` 增量帧 —— 客户端拿到的是完整参数，
//! 而不是需要自己拼接的半截。

use serde_json::{json, Value};

use super::eventstream::KiroFrame;

/// 一次进行中的工具调用。
#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    input: String,
}

/// 有状态翻译器：一次响应一个实例。
pub struct FrameTranslator {
    model: String,
    id: String,
    created: i64,
    index: u64,
    pending: Option<PendingTool>,
    finished: bool,
}

impl FrameTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            id: format!("chatcmpl-kiro-{}", crate::http::now_ms()),
            created: crate::http::now_ms() / 1000,
            index: 0,
            pending: None,
            finished: false,
        }
    }

    /// 吃一帧，产出零到多个 SSE 帧。
    pub fn feed(&mut self, frame: &KiroFrame) -> Vec<String> {
        if frame.message_type == "exception" {
            return vec![error_frame(&format!(
                "Kiro 上游返回异常：{}",
                frame.event
            ))];
        }
        let Ok(payload) = serde_json::from_str::<Value>(&frame.payload) else {
            return Vec::new();
        };
        match frame.event.as_str() {
            "assistantResponseEvent" => payload
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| vec![self.text_frame(text, None)])
                .unwrap_or_default(),
            "reasoningContentEvent" => payload
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| vec![self.text_frame("", Some(text))])
                .unwrap_or_default(),
            "toolUseEvent" => self.tool_frames(&payload),
            "messageMetadataEvent" => {
                self.finished = true;
                let usage = payload.get("tokenUsage").cloned().unwrap_or(Value::Null);
                vec![self.finish_frame("stop", &usage)]
            }
            _ => Vec::new(),
        }
    }

    /// 收尾：没收到 metadata 时补一个 finish 帧，最后一定带 `[DONE]`。
    pub fn finish(&mut self) -> Vec<String> {
        let mut frames = Vec::new();
        if let Some(pending) = self.pending.take() {
            frames.push(self.tool_call_frame(&pending));
        }
        if !self.finished {
            self.finished = true;
            frames.push(self.finish_frame("stop", &Value::Null));
        }
        frames.push("data: [DONE]\n\n".to_string());
        frames
    }

    fn tool_frames(&mut self, payload: &Value) -> Vec<String> {
        let id = payload.get("toolUseId").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return Vec::new();
        }
        let pending = self.pending.get_or_insert_with(PendingTool::default);
        if pending.id.is_empty() {
            pending.id = id.to_string();
        }
        if let Some(name) = payload.get("name").and_then(Value::as_str) {
            if !name.is_empty() {
                pending.name = name.to_string();
            }
        }
        if let Some(fragment) = payload.get("input").and_then(Value::as_str) {
            pending.input.push_str(fragment);
        }
        let stop = payload.get("stop").and_then(Value::as_bool).unwrap_or(false);
        if !stop {
            return Vec::new();
        }
        let done = self.pending.take().unwrap_or_default();
        vec![self.tool_call_frame(&done)]
    }

    fn tool_call_frame(&mut self, tool: &PendingTool) -> String {
        let arguments = if tool.input.trim().is_empty() {
            "{}".to_string()
        } else {
            tool.input.clone()
        };
        let index = self.index;
        self.index += 1;
        sse(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": index,
                        "id": tool.id,
                        "type": "function",
                        "function": { "name": tool.name, "arguments": arguments }
                    }]
                },
                "finish_reason": Value::Null
            }]
        }))
    }

    fn text_frame(&self, content: &str, reasoning: Option<&str>) -> String {
        let mut delta = serde_json::Map::new();
        if !content.is_empty() {
            delta.insert("content".into(), Value::String(content.to_string()));
        }
        if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
            delta.insert(
                "reasoning_content".into(),
                Value::String(reasoning.to_string()),
            );
        }
        sse(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": Value::Object(delta),
                "finish_reason": Value::Null
            }]
        }))
    }

    fn finish_frame(&self, reason: &str, usage: &Value) -> String {
        let mut frame = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }]
        });
        if let Some(total) = usage.get("totalTokens").and_then(Value::as_i64) {
            let input = usage
                .get("uncachedInputTokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output = usage.get("outputTokens").and_then(Value::as_i64).unwrap_or(0);
            frame["usage"] = json!({
                "prompt_tokens": input,
                "completion_tokens": output,
                "total_tokens": total
            });
        }
        sse(&frame)
    }
}

/// 一个 `data: …\n\n` 帧。
fn sse(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

/// 错误帧：客户端会把它当一次失败的答复。
fn error_frame(message: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({ "error": { "message": message, "type": "upstream_error" } })
    )
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::stream
```

Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro/stream.rs
git commit -m "feat(kiro): Kiro 事件帧翻译为 OpenAI SSE"
```

---

### Task 7: 对话转发（最小可对话闭环）

**Files:**

- Modify: `crates/providers/src/providers/kiro/mod.rs`（`conversation` 方法 + 私有 `run_chat`）
- Modify: `crates/providers/src/providers/kiro/endpoints.rs`（补 `chat_url`）

**Interfaces:**

- Consumes: Task 3 `FrameDecoder`、Task 4 `credential_from_secret`、Task 5 `wire::build`、Task 6 `FrameTranslator`
- Produces: 可用的 `conversation`

- [ ] **Step 1: 补 endpoints**

`endpoints.rs` 追加：

```rust
/// 推理端点：API Key 走 Kiro CLI 的 runtime 主机，OAuth 走 q / codewhisperer。
pub fn chat_url(cred: &super::auth::KiroCredential) -> String {
    match cred.kind {
        super::auth::CredentialKind::ApiKey => format!("{}/", api_key_base(&cred.region).trim_end_matches('/')),
        super::auth::CredentialKind::OAuth => {
            format!("https://q.{}.amazonaws.com/generateAssistantResponse", cred.region)
        }
    }
}
```

- [ ] **Step 2: 实现 conversation**

在 `impl Provider for KiroProvider` 里加：

```rust
    fn conversation<'a>(
        &'a self,
        ctx: &'a agent2api_provider_sdk::PluginCtx,
        req: proto::ConversationRequest,
        sink: agent2api_provider_sdk::ConversationSink<'a>,
    ) -> agent2api_provider_sdk::SendFuture<'a, ()> {
        Box::pin(async move { run_chat(ctx, req, sink).await })
    }
```

`mod.rs` 追加：

```rust
/// 一次对话：取凭证 → 拼请求 → 发上游 → 解码帧 → 翻译 → 推 sink。
async fn run_chat(
    ctx: &agent2api_provider_sdk::PluginCtx,
    req: proto::ConversationRequest,
    mut sink: agent2api_provider_sdk::ConversationSink<'_>,
) -> Result<(), String> {
    let account = ctx
        .store_get(ID, &req.account)
        .await
        .map_err(String::from)?;
    let cred = credential_from_secret(&account.secret)?;
    let client_model = req
        .body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("claude-sonnet-4.5")
        .to_string();
    let wire_model = if req.wire_model.is_empty() {
        client_model.clone()
    } else {
        req.wire_model.clone()
    };
    let body = wire::build(&req.body, &wire_model, None)?;
    let url = endpoints::chat_url(&cred);
    let mut headers = auth::auth_headers(&cred);
    headers.push(("content-type".to_string(), "application/json".to_string()));

    let client = crate::http::client(req.proxy.as_ref(), "");
    let response = crate::http::post_stream(
        &client,
        &url,
        &headers,
        &body,
        "application/vnd.amazon.eventstream",
    )
    .await?;

    if !response.is_success() {
        let status = response.status;
        let body = response.error_body.clone().unwrap_or_default();
        return Err(classify_error_text(status, &body));
    }

    sink.head(proto::StreamHead {
        status: 200,
        headers: [(
            "content-type".to_string(),
            "text/event-stream".to_string(),
        )]
        .into_iter()
        .collect(),
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;

    let Some(upstream) = response.response else {
        return Err("Kiro 上游没有返回响应体".to_string());
    };
    let mut decoder = eventstream::FrameDecoder::new();
    let mut translator = stream::FrameTranslator::new(&client_model);
    let mut source = upstream.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = source.next().await {
        let bytes = chunk.map_err(|error| format!("读 Kiro 上游失败：{error}"))?;
        for frame in decoder.push(&bytes)? {
            for text in translator.feed(&frame) {
                sink.send(text.as_bytes()).await?;
            }
        }
    }
    decoder.finish()?;
    for text in translator.finish() {
        sink.send(text.as_bytes()).await?;
    }
    Ok(())
}

/// 把上游错误体折成一句可读的失败原因（分类在 `classify` 里做）。
fn classify_error_text(status: u16, body: &str) -> String {
    let head: String = body.chars().take(240).collect();
    format!("Kiro 上游返回 HTTP {status}：{head}")
}
```

- [ ] **Step 3: 编译**

```bash
cd /root/programming/github/Agent2Providers
cargo check -p agent2api-providers --features kiro
```

Expected: 0 error。

- [ ] **Step 4: 冒烟（需要真 Key，没有则跳过并记录）**

```bash
cd /root/programming/github/Agent2Providers
KIRO_API_KEY='ksk_…' cargo run -p agent2api-providers --features kiro -- --selftest
```

若插件没有 `--selftest` 入口，改用网关 `scripts/probe-provider.mjs`（Task 12）。
**没有真 Key 时本步记录「未实测」并在 Task 12 补测，不得声称通过。**

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro
git commit -m "feat(kiro): 对话转发闭环（API Key → Event Stream → OpenAI SSE）"
```

---

### Task 8: 错误分类

**Files:**

- Modify: `crates/providers/src/providers/kiro/mod.rs`（`classify` 方法）
- Test: `mod.rs` 的 `#[cfg(test)]`

**Interfaces:**

- Consumes: `proto::ErrorClass`
- Produces: 四档分类，spec 第 7 节的表

- [ ] **Step 1: 写失败测试**

`mod.rs` 末尾：

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify(status: u16, message: &str) -> proto::ErrorClass {
        KiroProvider.classify(status, &json!({ "message": message }))
    }

    #[test]
    fn quota_429_is_quota_limited() {
        assert!(matches!(classify(429, "quota exceeded"), proto::ErrorClass::QuotaLimited { .. }));
    }

    #[test]
    fn overage_402_is_quota_limited_with_overage_wording() {
        match classify(402, "overage limit reached") {
            proto::ErrorClass::QuotaLimited { message, .. } => {
                assert!(message.contains("加量包"), "文案应区分加量包：{message}");
            }
            other => panic!("期望 QuotaLimited，得到 {other:?}"),
        }
    }

    #[test]
    fn suspended_account_is_fatal() {
        assert!(matches!(
            classify(403, "account temporarily_suspended"),
            proto::ErrorClass::Fatal { .. }
        ));
    }

    #[test]
    fn input_too_long_is_fatal_and_not_quota() {
        assert!(matches!(
            classify(400, "Input is too long."),
            proto::ErrorClass::Fatal { .. }
        ));
    }

    #[test]
    fn auth_errors_are_token_expired() {
        assert!(matches!(classify(401, "unauthorized"), proto::ErrorClass::TokenExpired { .. }));
        assert!(matches!(classify(403, "invalid_grant"), proto::ErrorClass::TokenExpired { .. }));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::tests
```

Expected: 部分 FAIL（`classify` 还是默认 `Fatal`）。

- [ ] **Step 3: 实现 classify**

`impl Provider for KiroProvider` 里加：

```rust
    fn classify(&self, status: u16, body: &serde_json::Value) -> proto::ErrorClass {
        let message = body
            .get("message")
            .or_else(|| body.get("Message"))
            .or_else(|| body.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Kiro 上游返回错误")
            .to_string();
        let lower = message.to_lowercase();
        let upstream_code = body.get("code").and_then(serde_json::Value::as_i64);

        // ① 封号：不是限额，必须让账号停下来
        if lower.contains("temporarily_suspended")
            || lower.contains("temporarily is suspended")
            || lower.contains("account suspended")
        {
            return proto::ErrorClass::Fatal {
                status,
                message: "Kiro 账号已被上游暂停，请在官方客户端确认账号状态".to_string(),
                upstream_code,
            };
        }

        // ② 上下文过长：客户端自己的问题，绝不能罚账号
        if lower.contains("input is too long") || lower.contains("too many tokens") {
            return proto::ErrorClass::Fatal {
                status,
                message: "本次请求的上下文超过 Kiro 上限（history 最多 198 条），请开启新会话".to_string(),
                upstream_code,
            };
        }

        // ③ 加量包耗尽：与订阅耗尽分开表述
        if status == 402 && lower.contains("overage") {
            return proto::ErrorClass::QuotaLimited {
                status,
                message: "Kiro 加量包额度已用完（订阅额度可能仍在）".to_string(),
                upstream_code,
                reset_at: None,
            };
        }

        // ④ 凭证失效
        if status == 401
            || lower.contains("invalid_grant")
            || lower.contains("token expired")
            || lower.contains("unauthorized")
        {
            return proto::ErrorClass::TokenExpired {
                message: "Kiro 凭证不可用，请重新添加账号".to_string(),
            };
        }

        // ⑤ 额度 / 速率
        if status == 429 || lower.contains("quota") || lower.contains("rate limit") {
            return proto::ErrorClass::QuotaLimited {
                status,
                message: "Kiro 账号额度或速率已达上限".to_string(),
                upstream_code,
                reset_at: None,
            };
        }

        proto::ErrorClass::Fatal { status, message, upstream_code }
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::tests
```

Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro/mod.rs
git commit -m "feat(kiro): 四档错误分类（封号/上下文过长/加量包/凭证）"
```

---

### Task 9: 模型目录

**Files:**

- Modify: `crates/providers/src/providers/kiro/models.rs`（整体实现）
- Modify: `crates/providers/src/providers/kiro/mod.rs`（`models` / `refresh_models`）
- Test: `models.rs` 的 `#[cfg(test)]`（只测纯解析函数）

**Interfaces:**

- Produces:
  - `pub fn parse_models(payload: &serde_json::Value) -> Vec<serde_json::Value>`
  - `pub fn model_page_token(payload: &serde_json::Value) -> Option<String>`
  - `pub fn profile_arn_of(payload: &serde_json::Value) -> Option<String>`

- [ ] **Step 1: 写失败测试**

`models.rs` 末尾：

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_model_list_with_capabilities() {
        let payload = json!({
            "models": [
                { "modelId": "claude-sonnet-4.5", "modelName": "Claude Sonnet 4.5",
                  "tokenLimits": { "maxInputTokens": 200000 },
                  "supportedInputTypes": ["TEXT", "IMAGE"] },
                { "modelId": "glm-5", "modelName": "GLM-5",
                  "tokenLimits": { "maxInputTokens": 128000 },
                  "supportedInputTypes": ["TEXT"] }
            ]
        });
        let models = parse_models(&payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "claude-sonnet-4.5");
        assert_eq!(models[0]["context_length"], 200000);
        assert_eq!(models[0]["vision"], true);
        assert_eq!(models[1]["vision"], false);
    }

    #[test]
    fn empty_next_token_is_none() {
        assert!(model_page_token(&json!({ "nextToken": "" })).is_none());
        assert!(model_page_token(&json!({})).is_none());
        assert_eq!(
            model_page_token(&json!({ "nextToken": "abc" })),
            Some("abc".to_string())
        );
    }

    #[test]
    fn profile_arn_is_read_from_profiles_list() {
        let payload = json!({ "profiles": [{ "arn": "arn:aws:codewhisperer:us-east-1:1:profile/x" }] });
        assert_eq!(
            profile_arn_of(&payload),
            Some("arn:aws:codewhisperer:us-east-1:1:profile/x".to_string())
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::models
```

Expected: 编译失败。

- [ ] **Step 3: 实现 models.rs**

```rust
//! Kiro 模型目录：`ListAvailableModels` + `ListAvailableProfiles`。
//!
//! ── 分页陷阱 ────────────────────────────────────────────────
//! 实测上游**每页都返回空 `nextToken`**。必须有三重停止条件：
//! token 为空 / token 已用过 / 本页无新增 —— 否则会无限翻页。
//!
//! ── 图片能力读字段不猜模型名 ─────────────────────────────────
//! 同一账号里 `glm-5` / `minimax-m2.5` 只支持文本，其余多数支持图片。
//! 按模型名猜必然出错。

use serde_json::{json, Value};

use super::auth::KiroCredential;
use super::endpoints;

/// 每页条数。
const PAGE_SIZE: u32 = 50;
/// 最多翻多少页（防上游行为异常）。
const MAX_PAGES: usize = 20;

/// 把一页模型记录归一成网关认的形状。
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let Some(models) = payload.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|model| {
            let id = model.get("modelId").and_then(Value::as_str)?;
            let name = model
                .get("modelName")
                .and_then(Value::as_str)
                .unwrap_or(id);
            let context_length = model
                .get("tokenLimits")
                .and_then(|limits| limits.get("maxInputTokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let vision = model
                .get("supportedInputTypes")
                .and_then(Value::as_array)
                .is_some_and(|types| types.iter().any(|kind| kind.as_str() == Some("IMAGE")));
            Some(json!({
                "id": id,
                "name": name,
                "context_length": context_length,
                "vision": vision,
            }))
        })
        .collect()
}

/// 下一页 token；空串与缺失都返回 `None`。
pub fn model_page_token(payload: &Value) -> Option<String> {
    payload
        .get("nextToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// 从 `ListAvailableProfiles` 的响应里取第一个 ARN。
pub fn profile_arn_of(payload: &Value) -> Option<String> {
    payload
        .get("profiles")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|profile| {
            profile
                .get("arn")
                .or_else(|| profile.get("profileArn"))
                .and_then(Value::as_str)
                .filter(|arn| !arn.is_empty())
                .map(str::to_string)
        })
}

/// 拉一次完整目录（自动翻页）。
pub async fn fetch_all(
    client: &reqwest::Client,
    cred: &KiroCredential,
) -> Result<Vec<Value>, String> {
    let base = format!("https://q.{}.amazonaws.com", cred.region);
    let headers = super::auth::auth_headers(cred);
    let mut models: Vec<Value> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut next_token: Option<String> = None;
    for page in 0..MAX_PAGES {
        let mut url = format!(
            "{base}/ListAvailableModels?origin=AI_EDITOR&maxResults={PAGE_SIZE}"
        );
        if let Some(token) = &next_token {
            url.push_str("&nextToken=");
            url.push_str(&urlencode(token));
        }
        let response = crate::http::get_json(client, &url, &headers).await?;
        if !(200..300).contains(&response.status) {
            if models.is_empty() {
                return Err(format!(
                    "Kiro 模型目录拉取失败：HTTP {} {}",
                    response.status,
                    response.error_head()
                ));
            }
            break;
        }
        let payload = response.body.unwrap_or(Value::Null);
        let added = parse_models(&payload);
        let mut new_count = 0;
        for model in added {
            let id = model.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if seen.insert(id) {
                models.push(model);
                new_count += 1;
            }
        }
        next_token = model_page_token(&payload);
        let Some(token) = next_token.clone() else { break };
        if page > 0 && new_count == 0 {
            break;
        }
    }
    Ok(models)
}

/// 最小 query 编码（token 里可能带 `+` `/` `=`）。
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
```

`mod.rs` 里加：

```rust
    fn models<'a>(
        &'a self,
        ctx: &'a agent2api_provider_sdk::PluginCtx,
        account: &'a str,
    ) -> agent2api_provider_sdk::SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            let stored = ctx.store_get(ID, account).await.map_err(String::from)?;
            let cred = credential_from_secret(&stored.secret)?;
            let client = crate::http::client(None, "");
            match models::fetch_all(&client, &cred).await {
                Ok(list) if !list.is_empty() => Ok(proto::ModelList {
                    models: list,
                    meta: proto::CatalogMeta {
                        remote: true,
                        refreshed_at: crate::http::now_ms(),
                    },
                }),
                _ => Ok(proto::ModelList {
                    models: default_models(),
                    meta: proto::CatalogMeta { remote: false, refreshed_at: 0 },
                }),
            }
        })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a agent2api_provider_sdk::PluginCtx,
        account: &'a str,
        _force: bool,
    ) -> agent2api_provider_sdk::SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let stored = ctx.store_get(ID, account).await.map_err(String::from)?;
            let cred = credential_from_secret(&stored.secret)?;
            let client = crate::http::client(None, "");
            let list = models::fetch_all(&client, &cred).await?;
            Ok(proto::RefreshOutcome::refreshed(list.len()))
        })
    }
```

- [ ] **Step 4: 跑测试 + 编译**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::models && cargo check -p agent2api-providers --features kiro
```

Expected: 3 passed，0 error。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro
git commit -m "feat(kiro): 模型目录拉取（ListAvailableModels）"
```

---

### Task 10: 余额查询

**Files:**

- Modify: `crates/providers/src/providers/kiro/usage.rs`（整体实现）
- Modify: `crates/providers/src/providers/kiro/mod.rs`（`usage`）
- Test: `usage.rs` 的 `#[cfg(test)]`

**Interfaces:**

- Produces: `pub fn normalize(payload: &serde_json::Value) -> Result<serde_json::Value, String>`

- [ ] **Step 1: 写失败测试**

`usage.rs` 末尾：

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subscription_window_is_normalized() {
        let payload = json!({
            "usageBreakdownList": [{
                "resourceType": "AGENTIC_REQUEST",
                "usageLimit": 100,
                "currentUsage": 30
            }],
            "nextDateReset": 1_900_000_000,
            "subscriptionInfo": { "subscriptionTitle": "Kiro Pro" }
        });
        let report = normalize(&payload).expect("应能归一");
        assert_eq!(report["plan"], "Kiro Pro");
        assert_eq!(report["windows"][0]["used"], 30);
        assert_eq!(report["windows"][0]["limit"], 100);
    }

    #[test]
    fn unbounded_limit_is_marked_not_zero() {
        let payload = json!({
            "usageBreakdownList": [{ "resourceType": "AGENTIC_REQUEST", "currentUsage": 5 }]
        });
        let report = normalize(&payload).expect("应能归一");
        assert_eq!(report["windows"][0]["unlimited"], true);
        assert!(report["windows"][0].get("limit").is_none());
    }

    #[test]
    fn empty_breakdown_is_an_error() {
        assert!(normalize(&json!({})).is_err());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::usage
```

Expected: 编译失败。

- [ ] **Step 3: 实现 usage.rs**

```rust
//! Kiro 余额：`getUsageLimits` 归一化。
//!
//! ── 四类额度都要覆盖 ────────────────────────────────────────
//! 订阅窗口（`usageBreakdownList`）、新手试用（`freeTrialInfo`，仅 ACTIVE）、
//! 具名 bonus、购买的 overage。读不到的量留空而不是写 0 —— 与网关
//! 「读不到的数值留空显示，不是 0」的口径一致。

use serde_json::{json, Value};

use super::auth::KiroCredential;

/// 拉一次并归一。
pub async fn fetch(
    client: &reqwest::Client,
    cred: &KiroCredential,
) -> Result<Value, String> {
    let url = format!(
        "https://codewhisperer.{}.amazonaws.com/getUsageLimits\
         ?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST&isEmailRequired=true",
        cred.region
    );
    let headers = super::auth::auth_headers(cred);
    let response = crate::http::get_json(client, &url, &headers).await?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "Kiro 余额查询失败：HTTP {} {}",
            response.status,
            response.error_head()
        ));
    }
    normalize(&response.body.unwrap_or(Value::Null))
}

/// 把 `getUsageLimits` 的响应归一成网关认的形状。
pub fn normalize(payload: &Value) -> Result<Value, String> {
    let breakdowns = payload
        .get("usageBreakdownList")
        .and_then(Value::as_array)
        .ok_or_else(|| "Kiro 余额响应里没有 usageBreakdownList".to_string())?;

    let reset_at = payload
        .get("nextDateReset")
        .or_else(|| payload.get("resetDate"))
        .and_then(Value::as_i64)
        .map(|seconds| seconds * 1000);

    let mut windows = Vec::new();
    for item in breakdowns {
        let used = item
            .get("currentUsage")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let limit = item
            .get("usageLimitWithPrecision")
            .and_then(Value::as_i64)
            .or_else(|| item.get("usageLimit").and_then(Value::as_i64));
        let mut window = json!({
            "name": item
                .get("resourceType")
                .and_then(Value::as_str)
                .unwrap_or("订阅额度"),
            "used": used,
            "unit": "requests",
        });
        match limit {
            Some(limit) if limit > 0 => {
                window["limit"] = json!(limit);
            }
            // 无上限：标记而不是折算成 0
            _ => {
                window["unlimited"] = json!(true);
            }
        }
        if let Some(reset_at) = reset_at {
            window["resetsAt"] = json!(reset_at);
        }
        windows.push(window);
    }

    // 新手试用只在 ACTIVE 时算数
    if let Some(trial) = payload.get("freeTrialInfo") {
        let active = trial
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status == "ACTIVE");
        if active {
            windows.push(json!({
                "name": "新手试用",
                "used": trial.get("currentUsage").and_then(Value::as_i64).unwrap_or(0),
                "limit": trial.get("usageLimit").and_then(Value::as_i64),
                "unit": "requests",
            }));
        }
    }

    let plan = payload
        .get("subscriptionInfo")
        .and_then(|info| info.get("subscriptionTitle"))
        .and_then(Value::as_str)
        .unwrap_or("Kiro");

    Ok(json!({
        "plan": plan,
        "windows": windows,
    }))
}
```

`mod.rs` 里加：

```rust
    fn usage<'a>(
        &'a self,
        ctx: &'a agent2api_provider_sdk::PluginCtx,
        account: &'a str,
    ) -> agent2api_provider_sdk::SendFuture<'a, proto::UsageReport> {
        Box::pin(async move {
            let stored = ctx.store_get(ID, account).await.map_err(String::from)?;
            let cred = credential_from_secret(&stored.secret)?;
            let client = crate::http::client(None, "");
            usage::fetch(&client, &cred).await
        })
    }
```

- [ ] **Step 4: 跑测试 + 编译**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::usage && cargo check -p agent2api-providers --features kiro
```

Expected: 3 passed，0 error。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro
git commit -m "feat(kiro): 余额查询（getUsageLimits）"
```

---

### Task 11: OAuth 登录（Builder ID device code + refresh）

**Files:**

- Modify: `crates/providers/src/providers/kiro/auth.rs`（`register_client` / `start_device` / `poll_token` / `refresh`）
- Modify: `crates/providers/src/providers/kiro/mod.rs`（`login_start` / `login_poll` / `refresh_token`）
- Test: `auth.rs` 的纯函数部分

**Interfaces:**

- Produces:
  - `pub async fn refresh(client, cred: &KiroCredential) -> Result<KiroCredential, String>`
  - `pub struct DeviceFlow { pub flow: String, pub url: String, pub interval_ms: u64 }`

- [ ] **Step 1: 写失败测试（只测纯逻辑）**

```rust
#[test]
fn oauth_credential_uses_q_endpoint() {
    let cred = KiroCredential {
        access_token: "at".into(),
        api_key: None,
        refresh_token: Some("rt".into()),
        region: "us-east-1".into(),
        kind: CredentialKind::OAuth,
    };
    assert!(super::endpoints::chat_url(&cred).contains("q.us-east-1.amazonaws.com"));
    assert!(auth_headers(&cred)
        .iter()
        .all(|(key, _)| key != "tokentype"));
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro kiro::auth
```

Expected: FAIL（`chat_url` 未实现或行为不符）。

- [ ] **Step 3: 实现 OAuth**

`auth.rs` 追加：

```rust
/// OIDC 基址。
pub fn oidc_base(region: &str) -> String {
    format!("https://oidc.{region}.amazonaws.com")
}

/// 注册 OIDC 客户端（device code 流程第一步）。
pub async fn register_client(
    client: &reqwest::Client,
    region: &str,
) -> Result<(String, String), String> {
    let url = format!("{}/client/register", oidc_base(region));
    let body = serde_json::json!({
        "clientName": "Kiro",
        "clientType": "public",
        "scopes": ["codewhisperer:completions", "codewhisperer:analysis"],
        "grantTypes": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
        "issuerUrl": "https://view.awsapps.com/start"
    });
    let response = crate::http::post_json(client, &url, &[], &body).await?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Kiro 客户端注册失败：HTTP {} {}", response.status, response.error_head()));
    }
    let payload = response.body.unwrap_or_default();
    let client_id = payload.get("clientId").and_then(|v| v.as_str()).unwrap_or("");
    let client_secret = payload.get("clientSecret").and_then(|v| v.as_str()).unwrap_or("");
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("Kiro 客户端注册响应缺少 clientId / clientSecret".to_string());
    }
    Ok((client_id.to_string(), client_secret.to_string()))
}

/// 发起设备授权，返回用户要打开的 URL 与轮询间隔。
pub async fn start_device(
    client: &reqwest::Client,
    region: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<(String, String, u64), String> {
    let url = format!("{}/device_authorization", oidc_base(region));
    let body = serde_json::json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "startUrl": "https://view.awsapps.com/start"
    });
    let response = crate::http::post_json(client, &url, &[], &body).await?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Kiro 设备授权失败：HTTP {} {}", response.status, response.error_head()));
    }
    let payload = response.body.unwrap_or_default();
    let device_code = payload.get("deviceCode").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let verification_uri = payload
        .get("verificationUriComplete")
        .or_else(|| payload.get("verificationUri"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let interval = payload.get("interval").and_then(|v| v.as_u64()).unwrap_or(5) * 1000;
    if device_code.is_empty() || verification_uri.is_empty() {
        return Err("Kiro 设备授权响应缺少 deviceCode / verificationUri".to_string());
    }
    Ok((device_code, verification_uri, interval))
}

/// 轮询 `/token`；`Ok(None)` = 还在等用户授权。
pub async fn poll_device(
    client: &reqwest::Client,
    region: &str,
    client_id: &str,
    client_secret: &str,
    device_code: &str,
) -> Result<Option<KiroCredential>, String> {
    let url = format!("{}/token", oidc_base(region));
    let form = [
        ("clientId", client_id),
        ("clientSecret", client_secret),
        ("deviceCode", device_code),
        ("grantType", "urn:ietf:params:oauth:grant-type:device_code"),
    ];
    let body: String = form
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = crate::http::send_raw(
        client,
        "POST",
        &url,
        &[("content-type".to_string(), "application/x-www-form-urlencoded".to_string())],
        Some(body.as_bytes()),
    )
    .await?;
    let payload = response.body.unwrap_or_default();
    if (200..300).contains(&response.status) {
        let access = payload.get("accessToken").and_then(|v| v.as_str()).unwrap_or("");
        let refresh = payload.get("refreshToken").and_then(|v| v.as_str()).unwrap_or("");
        if access.is_empty() {
            return Err("Kiro 授权成功但没拿到 accessToken".to_string());
        }
        return Ok(Some(KiroCredential {
            access_token: access.to_string(),
            api_key: None,
            refresh_token: (!refresh.is_empty()).then(|| refresh.to_string()),
            region: region.to_string(),
            kind: CredentialKind::OAuth,
        }));
    }
    let error = payload.get("error").and_then(|v| v.as_str()).unwrap_or("");
    if matches!(error, "authorization_pending" | "slow_down") {
        return Ok(None);
    }
    Err(format!("Kiro 授权失败：{error}"))
}

/// 用 refresh token 换新 access token。
pub async fn refresh(
    client: &reqwest::Client,
    cred: &KiroCredential,
) -> Result<KiroCredential, String> {
    let Some(refresh_token) = cred.refresh_token.as_deref() else {
        return Err("这个 Kiro 账号没有 refresh token，无法续期".to_string());
    };
    let url = format!("{}/token", oidc_base(&cred.region));
    let form = [
        ("clientId", "kiro"),
        ("grantType", "refresh_token"),
        ("refreshToken", refresh_token),
    ];
    let body: String = form
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = crate::http::send_raw(
        client,
        "POST",
        &url,
        &[("content-type".to_string(), "application/x-www-form-urlencoded".to_string())],
        Some(body.as_bytes()),
    )
    .await?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Kiro 续期失败：HTTP {}", response.status));
    }
    let payload = response.body.unwrap_or_default();
    let access = payload.get("accessToken").and_then(|v| v.as_str()).unwrap_or("");
    if access.is_empty() {
        return Err("Kiro 续期响应里没有 accessToken".to_string());
    }
    Ok(KiroCredential {
        access_token: access.to_string(),
        api_key: None,
        refresh_token: payload
            .get("refreshToken")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| cred.refresh_token.clone()),
        region: cred.region.clone(),
        kind: CredentialKind::OAuth,
    })
}

fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
```

`mod.rs` 补 `login_start` / `login_poll` / `refresh_token`，流程状态存 `ctx.settings_*`（键 `login-flow:{flow}`）。
**注意**：`login_start` 里要真调 `register_client` + `start_device` 并把 `clientId/clientSecret/deviceCode/region` 存进 settings；
`login_poll` 读回来调 `poll_device`。具体写法参照 `providers/kuku/mod.rs` 的登录实现（同一套 settings 存取的用法）。

- [ ] **Step 4: 跑测试 + 编译**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features kiro && cargo check -p agent2api-providers --features kiro
```

Expected: 全部通过，0 error。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/kiro
git commit -m "feat(kiro): Builder ID 设备授权登录与 OAuth 续期"
```

---

### Task 12: 文档、i18n 与端到端验收

**Files:**

- Modify: `agent2api/README.md` 与 `docs/README.{zh-CN,zh-Hant,ja,ko,pt-BR}.md`（支持矩阵 + 「内置 N 家」数字）
- Modify: `agent2api/desktop-tauri/i18n/i18n-src/{en,ja,ko,pt-BR,zh-Hant}/add-account.json`（删三条孤儿键）
- Modify: `Agent2Providers/README.md` / `README.zh-CN.md`（22 家）

- [ ] **Step 1: 六份 README 加一行**

在「Supported Providers」表按字母序插入（以 `README.md` 英文为例，其余五份按各自语言措辞）：

```markdown
| Kiro | ✓ | ✓ | ✓ | ✓ | — | — |
```

并在「核心特性」里把「内置 N 家提供商」的数字 +1（注意：Kiro 在表里占 1 行，`PROVIDERS` 不变，因为它不是 `ProviderKind`）。

- [ ] **Step 2: 清理孤儿 i18n 键**

五个 `add-account.json` 里删掉这三条（源码里已无对应中文键，`npm run check` 会报空值）：

```
"Kiro API key（ksk_…）。"
"只读引用本机 Kiro CLI/IDE；每次读取校验同一邮箱，不复制 token、不主动续期或回写，失效后由原客户端重登或同步。"
"独立 Kiro OAuth 支持续期；保留原登录验证，不接受任意 bearer token。"
```

- [ ] **Step 3: i18n 校验 + 重建**

```bash
cd /root/programming/github/agent2api/desktop-tauri/i18n
npm run scan && npm run check && npm run build
```

Expected: `check` 无空值。

- [ ] **Step 4: 端到端验收（需要真 Key）**

前置：面板「网关 Key」页建一把**只允许 kiro** 的 Key。

```bash
cd /root/programming/github/agent2api
node scripts/probe-provider.mjs \
  --base http://127.0.0.1:3065 \
  --key sk-dev-kiro \
  --provider kiro \
  --model claude-sonnet-4.5
```

判定：`stream` / `tool_call` / `tool_roundtrip` 必须 PASS（Skill 6.1）。
退出码 `0` 才算通过；`2` = 前置条件没满足（模型不在清单 / 不由 kiro 承载）。

- [ ] **Step 5: 记录实测结论**

把下面两项的**实测结果**写进 `providers/kiro/mod.rs` 模块头（替换「待实测」措辞）：

- 签到：打过哪些接口、返回什么、为什么判定为 ✗
- 领取类：同上

- [ ] **Step 6: 提交**

```bash
cd /root/programming/github/agent2api
git add README.md docs/README.*.md desktop-tauri/i18n/i18n-src
git commit -m "docs(kiro): 支持矩阵补入 Kiro 并清理孤儿译文"

cd /root/programming/github/Agent2Providers
git add README.md README.zh-CN.md crates/providers/src/providers/kiro/mod.rs
git commit -m "docs(kiro): 补 README 与实测结论"
```

---

## 自检结果

**Spec 覆盖**

| Spec 章节          | 覆盖任务                                       |
| ------------------ | ---------------------------------------------- |
| 1 端点与头         | Task 4（auth_headers）、Task 7（chat_url）     |
| 2 Event Stream     | Task 3                                         |
| 3 请求体四条硬约束 | Task 5                                         |
| 4 模型目录         | Task 9                                         |
| 5 余额             | Task 10                                        |
| 6 认证             | Task 4（API Key）、Task 11（OAuth）            |
| 7 错误分类         | Task 8                                         |
| 8 README 六列      | Task 12                                        |
| 9 与网关接法       | Task 1（解禁）、Task 7（`UpstreamKind::Chat`） |

**已知缺口（必须由执行者补齐，不得跳过）**

1. Task 11 的 `login_start` / `login_poll` 只给了 `auth.rs` 的底层函数，`settings_*` 存取的具体写法要求执行者照 `providers/kuku/mod.rs` 抄 —— 因为登录流程状态机的键名要与网关 `api::session` 的轮询契约一致，写错的表现是「浏览器打开后永远停在等待」。
2. Task 7 的冒烟与 Task 12 的 probe 都需要**真实 `ksk_` Key**。没有 Key 时只能验到编译与单测，`stream` / `tool_call` / `tool_roundtrip` 必须如实标「未实测」。
3. 图片输入（`images` 字段）与思考档位（`additionalModelRequestFields`）**不在本计划范围**：它们依赖 `ListAvailableModels` 返回的模型 schema，建议在 Task 9 落地后另开一份计划。

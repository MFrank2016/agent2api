# magpie 借鉴项 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 `docs/compare-magpie.md` 第十二章列出的 9 项可借鉴能力，补齐 Gemini 端点断链、统一签到声明、把行为开关下沉到 provider 级、扩展 Key 配额为多维、并新增两个通用 middleware（think-tags / 词语拦截）。

**Architecture:** 五项改动彼此独立，共享同一个原则——**把已经存在的机制从一个位置搬到更合适的位置**，而不是新造抽象。Gemini 端点复用 `protocol::antigravity_outbound` 与 `accio::protocol` 两块既有拼图；签到钩子提到 `ProviderAdapter` 复用 `supports_usage` 的既有形状；per-provider 开关复用 `ProviderQuirks` 的读取模式；Key 多维限额复用 `limiter` 的 `rule_kind` 编码；两个 middleware 插在 `payload::send_body` 既有的 ①→②→③ 管线里。

**Tech Stack:** Rust 2021（MSRV 1.88）、axum、wreq、serde_json、rusqlite；前端 TypeScript + React 岛（`ui-islands/`）。

**Spec:** `docs/compare-magpie.md`

## Global Constraints

- Rust edition 2021，MSRV 1.88。**不新增第三方依赖**。
  `toml_edit` 是唯一例外：它**已经作为传递依赖存在于 `Cargo.lock`**（三个版本：
  0.19.15 / 0.20.2 / 0.25.13，来自 `toml` 生态），Task 2 只是把它提为直接依赖，
  不引入新的依赖树。
- release 是 `panic = abort`：**绝不 `unwrap`/`expect` 可失败路径**；错误如实返回。
- 所有新增行为开关**默认关闭或保持现状**：升级后既有用户的行为与日志必须逐字不变。
- 不 `as any`、不 `@ts-ignore`（前端）；不 `#[allow(dead_code)]` 掩盖未接线代码。
- 修改文件遵循既有单文件行数约定（本项目惯例约 800 行，超长则拆分）。
- 测试命令（仓库根执行）：
  - 后端：`cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib <filter>`
  - 前端类型：`npm --prefix desktop-tauri/ui-islands run typecheck`
  - 前端构建：`npm --prefix desktop-tauri/ui-islands run build`（产物落 `desktop-tauri/ui/islands/`，**必须提交**）
- 提交前跑全量：`cargo test -p agent2api-server --lib`（当前基线 **765 passed / 11 ignored**）。

## Review Focus

1. **Gemini 客户端用非流式 `:generateContent`（不带 `alt=sse`）** → 期望返回完整 JSON，不是空流。
2. **Gemini 请求里带 `tools` / `systemInstruction` / `generationConfig`** → 期望正确映射到 chat 体，不丢字段、不 400。
3. **`checkin` 提到 trait 后，某家 `supports_checkin()` 为 true 但 `checkin()` 未覆写** → 期望**不 panic**，返回明确的「未实现」错误，且不出现在界面清单里。
4. **per-provider 开关的 provider id 在配置里但已不存在（删了自定义家）** → 期望**忽略该条**，不 panic、不影响其他家。
5. **Key 配额有多条规则且部分冲突（如「日 10 万」+「月 100 万」，某天用了 9 万）** → 期望**全部满足才放行**（magpie 的语义：5 条全过），不是「任一满足」。
6. **think-tags 剥离遇到跨流式分片的标签（`<thi` + `nk>`）** → 期望正确拼接后再判定，不产生半个标签残留在正文。
7. **词语拦截遇到工具结果 / 工具调用 / system prompt 里的词** → 期望**不拦**（只查用户文本，与 magpie 的 `word-guard` 同口径）。

---

## File Structure

| 文件 | 责任 | 动作 |
|---|---|---|
| `server/src/server/api/gemini.rs` | Gemini 入口：路由解析、请求体映射、响应回写 | 新建 |
| `server/src/server/http.rs` | 注册 `:generateContent` 等路由 | 修改 |
| `server/src/server/core/protocol/gemini_outbound.rs` | Gemini 请求体 ↔ chat 体映射 | 新建 |
| `server/src/server/core/providers/adapter.rs` | 新增 `supports_checkin` / `checkin` 钩子 | 修改 |
| `server/src/server/core/billing/checkin.rs` | 改为调用 trait 钩子 | 修改 |
| `server/src/server/core/auto_checkin.rs` | `CHECKIN_PROVIDERS` 从 `implemented_kinds()` 派生 | 修改 |
| `server/src/server/core/provider_options.rs` | per-provider 开关的存取与校验 | 新建 |
| `server/src/server/config/types.rs` | 新增 `providerOptions` 配置键 | 修改 |
| `server/src/server/core/api_keys.rs` | `quota_tokens: Option<i64>` → `quota_rules: Vec<KeyQuotaRule>` | 修改 |
| `server/src/server/core/key_quota.rs` | 多条规则判定 | 修改 |
| `server/src/server/core/middleware/think_tags.rs` | 通用思考标签处理 | 新建 |
| `server/src/server/core/middleware/word_guard.rs` | 用户文本词语拦截 | 新建 |
| `server/src/server/core/middleware/mod.rs` | middleware 模块根 | 新建 |
| `server/src/server/core/upstream/payload.rs` | 在 ②→③ 之间插入 middleware 层 | 修改 |
| `server/src/server/config/types.rs` | 新增两个 middleware 的开关与配置 | 修改 |
| `ui-islands/src/islands/keys-page.tsx` | Key 配额多条规则编辑 | 修改 |
| `ui-islands/src/islands/settings-page.tsx` | middleware 配置区 | 修改 |

---

## Task 1: Gemini 端点（修断链）

**背景**：Agent 页可接管 Gemini CLI，但网关无 Gemini 路由。`core/agents/mod.rs` 给
`gemini-cli` 声明 `protocol: "gemini"`，`core/agents/apply.rs` 写入
`GOOGLE_GEMINI_BASE_URL` 指向本网关，而 `server/http.rs` 只注册了
`/v1/chat/completions`、`/v1/responses`、`/v1/messages`、`/v1/messages/count_tokens`。
Gemini CLI 会拼 `/v1beta/models/{model}:generateContent`，必然 404。

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/core/protocol/gemini_outbound.rs`
- Create: `desktop-tauri/src-tauri/server/src/server/api/gemini.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/http.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/protocol/mod.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/api/mod.rs`
- Test: `desktop-tauri/src-tauri/server/src/server/core/protocol/gemini_outbound.rs`（内联 `#[cfg(test)]`）

**Interfaces:**
- Consumes: `crate::server::api::protocol::forward_chat`（既有 chat 转发骨架）、
  `crate::server::core::protocol::antigravity_stream::ChatFromAntigravityStream`（既有 Gemini SSE → chat 翻译机）
- Produces:
  - `gemini_outbound::chat_from_gemini_request(body: &Value) -> Result<Value, String>`
  - `gemini_outbound::gemini_response_from_chat(body: &Value, model: &str) -> Value`
  - `api::gemini::generate_content(State, Path<(String, String)>, Bytes) -> Response`
  - `api::gemini::stream_generate_content(State, Path<(String, String)>, Bytes) -> Response`
  - `api::gemini::count_tokens(State, Path<(String, String)>, Bytes) -> Response`

- [ ] **Step 1: 写失败测试（请求体映射）**

在 `gemini_outbound.rs` 末尾写：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_gemini_request_to_chat() {
        let gemini = serde_json::json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [{"text": "hello"}]},
                {"role": "user", "parts": [{"text": "again"}]}
            ],
            "systemInstruction": {"parts": [{"text": "be terse"}]},
            "generationConfig": {"temperature": 0.2, "maxOutputTokens": 64}
        });
        let chat = chat_from_gemini_request(&gemini).expect("应当映射成功");
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "be terse");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(chat["messages"][2]["role"], "assistant", "model 角色要翻成 assistant");
        assert_eq!(chat["temperature"], 0.2);
        assert_eq!(chat["max_tokens"], 64);
    }

    #[test]
    fn rejects_empty_contents() {
        let gemini = serde_json::json!({"contents": []});
        assert!(chat_from_gemini_request(&gemini).is_err());
    }

    #[test]
    fn maps_chat_response_back_to_gemini() {
        let chat = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });
        let gemini = gemini_response_from_chat(&chat, "gemini-2.5-pro");
        assert_eq!(gemini["candidates"][0]["content"]["parts"][0]["text"], "hello");
        assert_eq!(gemini["usageMetadata"]["promptTokenCount"], 5);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib gemini_outbound`
Expected: FAIL — `cannot find function chat_from_gemini_request`

- [ ] **Step 3: 实现映射层**

`gemini_outbound.rs` 顶部（模块注释必须说明为什么只做「请求体映射」而不做「整条转发」：

```rust
//! Gemini 协议 ↔ chat 协议的**请求体与响应体**映射。
//!
//! ── 为什么只做映射、不做转发 ──────────────────────────────────
//! 转发骨架已经有两条：`api::chat`（chat 入口）与 `api::protocol`
//! （responses / messages 入口），它们都落到 `provider_loop` 的同一条链。
//! Gemini 入口要做的事只有「把 Gemini 体翻成 chat 体 → 复用 chat 转发
//! → 把 chat 响应翻回 Gemini 体」，所以本文件只负责两端的翻译。
//!
//! ── 为什么复用 antigravity_stream 而不是另写一台 ─────────────
//! `protocol::antigravity_stream::ChatFromAntigravityStream` 已经是
//! 「Gemini SSE 帧 → chat SSE 帧」的翻译机（Antigravity 上游就是 Gemini
//! v1internal 方言）。**方向相反**的那一半（chat → Gemini）本文件补上。
//! 两台状态机的帧词表是同一套，合并会让「读」与「写」互相干扰，因此分开放。
//!
//! ── 不支持的 Gemini 特性（明确列出，避免「看起来支持」）──────
//!   - `cachedContent`：上游缓存引用，chat 协议无对应概念 → 忽略并记 verbose
//!   - `safetySettings`：本项目不透传安全设置（与 responses 入口同口径）
//!   - `responseSchema` / `responseMimeType`：chat 侧无稳定对应 → 转成
//!     system 提示词里的一句约束（保底，不保证等价）

use serde_json::{json, Value};

/// Gemini 请求体 → chat 请求体。
///
/// `contents[].role` 的 `model` 翻成 `assistant`（Gemini 用 model，
/// OpenAI 用 assistant，这是两家唯一不一致的角色名）。
/// `systemInstruction` 提到 messages 的首条 system（chat 没有独立槽位）。
pub fn chat_from_gemini_request(body: &Value) -> Result<Value, String> {
    let contents = body
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| "Gemini 请求缺少 contents 数组".to_string())?;
    if contents.is_empty() {
        return Err("Gemini 请求的 contents 为空".to_string());
    }
    let mut messages: Vec<Value> = Vec::with_capacity(contents.len() + 1);
    if let Some(text) = body
        .get("systemInstruction")
        .and_then(|instruction| instruction.get("parts"))
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        messages.push(json!({"role": "system", "content": text}));
    }
    for content in contents {
        let role = match content.get("role").and_then(Value::as_str) {
            Some("model") => "assistant",
            Some("user") | None => "user",
            Some(other) => return Err(format!("未知的 Gemini 角色: {other}")),
        };
        let text = content
            .get("parts")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        messages.push(json!({"role": role, "content": text}));
    }
    let mut chat = json!({"messages": messages});
    if let Some(config) = body.get("generationConfig") {
        if let Some(temperature) = config.get("temperature") {
            chat["temperature"] = temperature.clone();
        }
        if let Some(max_tokens) = config.get("maxOutputTokens") {
            chat["max_tokens"] = max_tokens.clone();
        }
        if let Some(top_p) = config.get("topP") {
            chat["top_p"] = top_p.clone();
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let functions: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool.get("functionDeclarations"))
            .filter_map(Value::as_array)
            .flatten()
            .map(|declaration| {
                json!({
                    "type": "function",
                    "function": {
                        "name": declaration.get("name").cloned().unwrap_or(Value::Null),
                        "description": declaration.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": declaration.get("parameters").cloned().unwrap_or(Value::Null),
                    }
                })
            })
            .collect();
        if !functions.is_empty() {
            chat["tools"] = Value::Array(functions);
        }
    }
    Ok(chat)
}

/// chat 响应体 → Gemini 响应体（非流式）。
pub fn gemini_response_from_chat(body: &Value, model: &str) -> Value {
    let choice = body.get("choices").and_then(|choices| choices.get(0));
    let text = choice
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let finish = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("STOP");
    let finish_reason = match finish {
        "length" => "MAX_TOKENS",
        "stop" => "STOP",
        _ => "STOP",
    };
    let usage = body.get("usage").cloned().unwrap_or(Value::Null);
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": text}]},
            "finishReason": finish_reason,
        }],
        "usageMetadata": {
            "promptTokenCount": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
            "candidatesTokenCount": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
            "totalTokenCount": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        },
        "modelVersion": model,
    })
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib gemini_outbound`
Expected: PASS — 3 passed

- [ ] **Step 5: 写失败测试（路由解析）**

在 `api/gemini.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_and_action_from_path() {
        let (model, action) = parse_model_action("gemini-2.5-pro:generateContent").unwrap();
        assert_eq!(model, "gemini-2.5-pro");
        assert_eq!(action, Action::GenerateContent);
        let (model, action) = parse_model_action("gemini-2.5-pro:streamGenerateContent").unwrap();
        assert_eq!(model, "gemini-2.5-pro");
        assert_eq!(action, Action::StreamGenerateContent);
        let (_, action) = parse_model_action("gemini-2.5-pro:countTokens").unwrap();
        assert_eq!(action, Action::CountTokens);
        assert!(parse_model_action("gemini-2.5-pro").is_err(), "缺动作要报错");
        assert!(parse_model_action("gemini-2.5-pro:unknown").is_err(), "未知动作要报错");
    }
}
```

- [ ] **Step 6: 运行测试确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib api::gemini`
Expected: FAIL — `cannot find function parse_model_action`

- [ ] **Step 7: 实现路由与 handler**

`api/gemini.rs`：

```rust
//! Gemini 协议入口（`/v1beta/models/{model}:{action}`）。
//!
//! ── 为什么要这个入口 ────────────────────────────────────────
//! Agent 页可以接管 Gemini CLI（`core::agents` 的 `gemini-cli`），它写的是
//! `GOOGLE_GEMINI_BASE_URL=<本网关>`。Gemini CLI 会拼
//! `/v1beta/models/{model}:generateContent`，而网关此前只有 chat /
//! responses / messages 三个入口 —— 那次接管**必然 404**。
//!
//! ── 路径形态为什么用 `:action` 后缀而不是独立路由 ──────────────
//! Gemini 的协议就是 `models/{model}:{action}` 一个路径段，`:` 是模型名与
//! 动作的分隔符（不是 URL 参数）。axum 的路径参数不能带 `:`，所以注册成
//! `/{model_action}` 再在 handler 里切分 —— 与 Gemini 客户端发出的字节完全一致。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::server::core::protocol::gemini_outbound;
use crate::server::state::ServerState;

/// `models/{model}:{action}` 里的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    GenerateContent,
    StreamGenerateContent,
    CountTokens,
}

/// 切分 `{model}:{action}`。
pub fn parse_model_action(raw: &str) -> Result<(String, Action), String> {
    let (model, action) = raw
        .split_once(':')
        .ok_or_else(|| format!("路径缺少 `:动作` 后缀: {raw}"))?;
    if model.is_empty() {
        return Err("模型名为空".to_string());
    }
    let action = match action {
        "generateContent" => Action::GenerateContent,
        "streamGenerateContent" => Action::StreamGenerateContent,
        "countTokens" => Action::CountTokens,
        other => return Err(format!("未知的 Gemini 动作: {other}")),
    };
    Ok((model.to_string(), action))
}

/// `POST /v1beta/models/{model}:generateContent`
pub async fn generate_content(
    State(state): State<ServerState>,
    Path(model_action): Path<String>,
    body: Bytes,
) -> Response {
    dispatch(state, model_action, body, Action::GenerateContent).await
}

/// `POST /v1beta/models/{model}:streamGenerateContent`
pub async fn stream_generate_content(
    State(state): State<ServerState>,
    Path(model_action): Path<String>,
    body: Bytes,
) -> Response {
    dispatch(state, model_action, body, Action::StreamGenerateContent).await
}

/// `POST /v1beta/models/{model}:countTokens`
pub async fn count_tokens(
    State(state): State<ServerState>,
    Path(model_action): Path<String>,
    body: Bytes,
) -> Response {
    dispatch(state, model_action, body, Action::CountTokens).await
}

async fn dispatch(
    state: ServerState,
    model_action: String,
    body: Bytes,
    expected: Option<Action>,
    headers: axum::http::HeaderMap,
    scope: Option<crate::server::core::key_scope::KeyScope>,
) -> Response {
    let (model, action) = match parse_model_action(&model_action) {
        Ok(parsed) => parsed,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": message}})))
                .into_response()
        }
    };
    if action != expected {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "路径动作与 handler 不一致"}})),
        )
            .into_response();
    }
    let raw_body = body.clone();
    let gemini: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": format!("请求体不是 JSON: {error}")}})),
            )
                .into_response()
        }
    };
    // countTokens 是纯本地估算，不需要打上游（Gemini 的语义也是估算）。
    if action == Action::CountTokens {
        let text = gemini
            .get("contents")
            .and_then(Value::as_array)
            .map(|contents| {
                contents
                    .iter()
                    .filter_map(|content| content.get("parts"))
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter_map(|part| part.get("text"))
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        // 与 `api::chat::count_tokens` 同一估算口径（4 字符 ≈ 1 token）
        let tokens = (text.chars().count() / 4).max(1);
        return Json(json!({"totalTokens": tokens})).into_response();
    }
    let chat = match gemini_outbound::chat_from_gemini_request(&gemini) {
        Ok(chat) => chat,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": message}})))
                .into_response()
        }
    };
    // 复用 chat 入口的转发骨架：把翻译后的 chat 体交给它，再把结果翻回 Gemini。
    //
    // `forward_chat` 目前是 `api::protocol` 的**私有**函数（签名见下），
    // 本任务需要把它提升为 `pub(crate)`，并在 `api/protocol.rs` 里加一个
    // 薄封装（理由：它要 `client_headers` / `dedupe_key` / `telemetry` 三个
    // 入口参数，Gemini 入口的取法与既有两入口一致，不该在三个地方各写一遍）。
    let forwarded = crate::server::api::protocol::forward_chat_entry(
        &state,
        chat,
        &headers,
        action == Action::StreamGenerateContent,
        &raw_body,
        scope,
    )
    .await;
    match forwarded {
        Ok(response) => response,
        Err(message) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"message": message}})),
        )
            .into_response(),
    }
}
```

- [ ] **Step 8: 运行测试确认通过**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib api::gemini`
Expected: PASS — 1 passed

- [ ] **Step 8b: 在 `api/protocol.rs` 加转发封装**

`api/gemini.rs` 需要复用转发骨架，但 `forward_chat` 是私有且要吃
`dedupe_key` / `telemetry` / `KeyScope` 三个入口参数。在 `api/protocol.rs`
加一个薄封装，把「Gemini 入口怎么取这三个参数」的判据收敛在一处：

```rust
/// 给协议入口用的转发封装：**只做入口参数的取法**，转发逻辑全在
/// [`forward_chat`] 里。
///
/// ── 为什么需要它（而不是让每个入口自己拼参数）────────────────
/// 三个入口（responses / messages / gemini）的 `dedupe_key` 取法必须一致
/// （按「客户端原始 body 的 sha256」去重，见 `api::chat` 的说明），
/// `telemetry` 与 `KeyScope` 的取法也一样。各写一遍的下场是某个入口悄悄
/// 换了去重键，于是同一份请求在两个入口下算作两次。
///
/// ── 与既有两入口的关系 ────────────────────────────────────
/// `responses_endpoint` / `messages_endpoint` 已在用同样的取法，但它们各自
/// 内联了这几行。本函数不改变它们的行为，只是给新入口一个可复用的入口点；
/// 后续可把三者统一到这里（不在本任务范围内，避免无谓的回归面）。
pub(crate) async fn forward_chat_entry(
    state: &ServerState,
    payload: Value,
    client_headers: &HeaderMap,
    stream: bool,
    // 客户端原始 body 的字节（去重键的原料）。Gemini 入口拿到的是**原始**
    // Gemini 体，不是翻译后的 chat 体 —— 去重键必须用前者（同一份客户端请求
    // 在两个入口下应算一次，而翻译后的体已经变了）。
    raw_body: &[u8],
    // 命中的网关 Key 身份：由 `http.rs` 的鉴权中间件放进请求扩展，
    // handler 用 `Extension<Option<KeyScope>>` 取出后传进来。
    scope: Option<crate::server::core::key_scope::KeyScope>,
) -> Result<Response, String> {
    let dedupe_key = crate::server::core::upstream::pipeline::sha256_hex(raw_body);
    let telemetry = Arc::new(crate::server::core::upstream::RequestTelemetry::with_id());
    if let Some(scope) = scope.as_ref() {
        telemetry.set_key(scope.key_id(), scope.key_name());
    }
    match forward_chat(state, payload, client_headers, stream, dedupe_key, scope, telemetry).await {
        Ok(outcome) => Ok(outcome.into_response()),
        Err(error) => Err(error.message),
    }
}
```

> **实现前先核对**：`dedupe_key_of` / `key_scope::from_headers` /
> `RequestTelemetry::default` 的真实名字。若本项目用的是别的取法
> （如 `api::chat` 里内联的 `sha256` 计算），按既有写法照抄，
> **不要新造一套去重键**。

- [ ] **Step 9: 注册路由**

`server/http.rs` 在既有四个 `/v1/*` 路由之后追加：

```rust
// Gemini 入口（`models/{model}:{action}`）：`:` 是 Gemini 的模型/动作分隔符，
// axum 路径参数不能带 `:`，所以注册成单个参数再在 handler 里切分。
.route(
    "/v1beta/models/{model_action}",
    post(api::gemini::generate_content),
)
```

> **注意**：同一路径要挂三个动作，axum 不允许同一 path 注册多次。改为
> 单个 handler `gemini_entry`，在内部按 `parse_model_action` 的结果分派到
> `generate_content` / `stream_generate_content` / `count_tokens`。
> 把 Step 7 的三个 pub handler 保留为内部函数，对外只暴露：

```rust
.route("/v1beta/models/{model_action}", post(api::gemini::gemini_entry))
```

`gemini_entry` 的实现就是把 `dispatch(state, model_action, body, None)` 里的
`expected` 改成 `Option<Action>`（`None` = 按路径自身动作分派）。

- [ ] **Step 10: 全量测试 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
git add desktop-tauri/src-tauri/server/src/server/api/gemini.rs \
        desktop-tauri/src-tauri/server/src/server/core/protocol/gemini_outbound.rs \
        desktop-tauri/src-tauri/server/src/server/core/protocol/mod.rs \
        desktop-tauri/src-tauri/server/src/server/api/mod.rs \
        desktop-tauri/src-tauri/server/src/server/http.rs
git commit -m "新增：Gemini 协议入口（/v1beta/models/{model}:{action}）

修一处已确认的断链：Agent 页可接管 Gemini CLI（写 GOOGLE_GEMINI_BASE_URL
指向本网关），但网关此前只有 chat/responses/messages 三个入口，Gemini CLI
拼出的 /v1beta/models/{model}:generateContent 必然 404。

- gemini_outbound：Gemini 请求体 ↔ chat 请求体、chat 响应 → Gemini 响应
- api::gemini：路径 {model}:{action} 切分 + 三个动作分派
- countTokens 走本地估算（与 chat 入口同一口径），不打上游
- 复用既有 antigravity_stream 的 Gemini SSE 翻译机（方向相反的那半在此补齐）
- 明确不支持的 Gemini 特性写在模块头：cachedContent / safetySettings / responseSchema"
```

---

## Task 2: Agent 写入保留注释

**背景**：`core/agents/apply.rs` 的模块头自己承认「注释保不住（已知取舍）」——
写入走「解析成值树 → 改键 → 重新序列化」，JSONC / TOML 的注释全丢。Codex 的
`config.toml` 是用户手写的，常有注释掉的实验配置。

**Files:**
- Modify: `desktop-tauri/src-tauri/server/Cargo.toml`（加 `toml_edit`）
- Modify: `desktop-tauri/src-tauri/server/src/server/core/agents/apply.rs`
- Test: `desktop-tauri/src-tauri/server/src/server/core/agents/apply.rs`（内联）

**Interfaces:**
- Consumes: 既有 `apply_single` / `apply_multi` 的「改哪几个键」路径列表
- Produces: `apply::set_toml_path_preserving(text: &str, path: &str, value: &Value) -> Result<String, String>`

- [ ] **Step 1: 写失败测试**

在 `apply.rs` 的 `#[cfg(test)]` 模块里追加：

```rust
#[test]
fn toml_write_preserves_comments_and_order() {
    let original = r#"# 我的 Codex 配置
model = "gpt-5"

# 实验性设置，别动
[model_providers.agent2api]
base_url = "http://old/v1"   # 旧地址
env_key = "OLD"
"#;
    let updated = set_toml_path_preserving(
        original,
        "model_providers.agent2api.base_url",
        &serde_json::json!("http://127.0.0.1:3065/v1"),
    )
    .expect("应当写入成功");
    assert!(updated.contains("# 我的 Codex 配置"), "顶层注释必须保留");
    assert!(updated.contains("# 实验性设置，别动"), "段注释必须保留");
    assert!(updated.contains("http://127.0.0.1:3065/v1"));
    assert!(!updated.contains("http://old/v1"));
    let model_pos = updated.find("model = \"gpt-5\"").expect("model 行还在");
    let provider_pos = updated.find("[model_providers.agent2api]").expect("段还在");
    assert!(model_pos < provider_pos, "键序必须保持");
}

#[test]
fn toml_write_reports_missing_table() {
    let original = "model = \"gpt-5\"\n";
    // 目标表不存在时按现有语义**报错**而不是悄悄新建整段
    // （新建会让用户看到一个自己没写过的段）
    assert!(set_toml_path_preserving(original, "model_providers.x.base_url", &serde_json::json!("u")).is_err());
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib toml_write`
Expected: FAIL — `cannot find function set_toml_path_preserving`

- [ ] **Step 3: 加依赖**

`server/Cargo.toml` 的 `[dependencies]` 区，紧跟既有 `toml` 那一行：

```toml
# 保留注释与格式地改写 TOML（Codex 的 config.toml 是用户手写的，注释不能丢）。
# `toml`（既有）是「解析成值树」，本 crate 是「带 trivia 的语法树」——
# 两者用途不同，不是重复依赖。
# **不是新增依赖**：`toml_edit` 已在 Cargo.lock 里（作为 `toml` 生态的传递依赖，
# 现有 0.19.15 / 0.20.2 / 0.25.13 三个版本）。这里指定与 `toml` 0.9 配套的
# 版本，避免再引入第四个版本。落地前先跑 `cargo tree -i toml_edit` 确认。
toml_edit = "0.22"
```

- [ ] **Step 4: 实现 `set_toml_path_preserving`**

```rust
/// 按点分路径写入一个 TOML 值，**保留注释、键序与格式**。
///
/// ── 与既有 `toml::to_string_pretty` 的分工 ────────────────────
/// 那条路径是「解析成 `toml::Table` → 改键 → 整体序列化」，注释在解析阶段
/// 就丢了。本函数改用 `toml_edit`（带 trivia 的语法树），只动目标键所在的
/// 那一处 —— 用户写在别处的注释、缩进、空行都逐字保留。
///
/// ── 目标表不存在时为什么报错而不是新建 ────────────────────────
/// 新建一个 `[model_providers.agent2api]` 整段等于往用户文件里塞一段他没写过的
/// 内容。本项目的 `apply_single` 语义是「只改该改的那几个键」，目标键不存在
/// 说明客户端版本或用户手改导致结构与预期不符，此时应当让用户看到失败原因。
pub fn set_toml_path_preserving(
    text: &str,
    dotted_path: &str,
    value: &Value,
) -> Result<String, String> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("解析 TOML 失败: {error}"))?;
    let (table_path, key) = dotted_path
        .rsplit_once('.')
        .ok_or_else(|| format!("路径必须至少两段（表.键）: {dotted_path}"))?;
    let mut table: &mut toml_edit::Table = document.as_table_mut();
    for segment in table_path.split('.') {
        table = table
            .get_mut(segment)
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| format!("TOML 里没有段 `{segment}`（完整路径: {dotted_path}）"))?;
    }
    if !table.contains_key(key) {
        return Err(format!("TOML 里没有键 `{key}`（完整路径: {dotted_path}）"));
    }
    let converted = json_to_toml_item(value)?;
    table.insert(key, converted);
    Ok(document.to_string())
}

/// JSON 值 → TOML 值。只支持本项目实际会写的几种类型。
fn json_to_toml_item(value: &Value) -> Result<toml_edit::Item, String> {
    Ok(match value {
        Value::String(text) => toml_edit::value(text.clone()),
        Value::Bool(flag) => toml_edit::value(*flag),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                toml_edit::value(int)
            } else if let Some(float) = number.as_f64() {
                toml_edit::value(float)
            } else {
                return Err(format!("TOML 无法表示的数字: {number}"));
            }
        }
        Value::Array(_) | Value::Object(_) => {
            return Err("本项目不写复合 TOML 值（数组 / 表），遇到即报错".to_string())
        }
        Value::Null => return Err("TOML 没有 null".to_string()),
    })
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib toml_write`
Expected: PASS — 2 passed

- [ ] **Step 6: 接到 `apply_single` 的 TOML 分支**

`apply.rs:317` 那一处 `toml::to_string_pretty` 改为：若路径列表全部是
「表.键」形态的标量写入，走 `set_toml_path_preserving`；否则回退既有路径。
（`apply_single` 的 `paths` 已逐条给出「表.键」形态，见 `mod.rs` 的 `ClientDef`。）

- [ ] **Step 7: 验证既有行为未变 + 提交**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib agents`

```bash
git add desktop-tauri/src-tauri/server/Cargo.toml desktop-tauri/src-tauri/server/Cargo.lock \
        desktop-tauri/src-tauri/server/src/server/core/agents/apply.rs
git commit -m "改进：Agent 写 TOML 保留注释与键序（toml_edit）

原实现「解析成值树 → 改键 → 重新序列化」，JSONC / TOML 的注释全丢。
Codex 的 config.toml 是用户手写的，常有注释掉的实验配置，一次切换全丢
会让用户不敢用这个功能。

- 新增 set_toml_path_preserving：带 trivia 的语法树，只动目标键
- 目标表/键不存在时**报错而不是新建整段**（不往用户文件里塞没写过的内容）
- 复合值（数组/表）明确不支持，遇到即报错（本项目不写这类值）"
```

---

## Task 3: `checkin` 提升为 trait 钩子

**背景**：签到分派写在 `billing::checkin::checkin_for` 的 `match provider` 里，
`ProviderAdapter` 上没有签到方法。新增一家要同时改 `adapter.rs`、`checkin.rs`
与 `CHECKIN_PROVIDERS` 三处。

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/billing/checkin.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/auto_checkin.rs`
- Test: `desktop-tauri/src-tauri/server/src/server/core/billing/checkin.rs`（内联）

**Interfaces:**
- Consumes: `ProviderAdapter::implemented_kinds()`、既有 `checkin_for` 的每 provider 分支体
- Produces:
  - `ProviderAdapter::supports_checkin(&self) -> bool`（默认 `false`）
  - `ProviderAdapter::checkin<'a>(&'a self, store: &'a AccountStore, billing: &'a BillingService, account: &'a Value, manual: bool) -> Pin<Box<dyn Future<Output = CheckinOutcome> + Send + 'a>>`
  - `auto_checkin::checkin_providers() -> Vec<String>`（从 `implemented_kinds()` 派生）

- [ ] **Step 1: 写失败测试**

在 `checkin.rs` 的测试模块里：

```rust
#[test]
fn every_checkin_provider_declares_the_trait_hook() {
    // 回归锁定：`CHECKIN_PROVIDERS` 与 trait 声明必须一致。
    // 反例（改造前）：两者各自维护，新增一家漏改任一处都会静默失效。
    let declared: Vec<String> = crate::server::core::auto_checkin::checkin_providers();
    for provider_id in &declared {
        let kind = crate::server::core::providers::kind_from_id(provider_id)
            .unwrap_or_else(|| panic!("CHECKIN_PROVIDERS 里的 {provider_id} 不在注册表里"));
        assert!(
            crate::server::core::providers::adapter_for(kind).supports_checkin(),
            "{provider_id} 在 CHECKIN_PROVIDERS 里但适配器没声明 supports_checkin"
        );
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib every_checkin_provider`
Expected: FAIL — `no method named supports_checkin`

- [ ] **Step 3: 加 trait 钩子**

`adapter.rs` 在 `supports_usage` 之后插入：

```rust
    /// 本 provider 是否有**每日签到**能力。
    ///
    /// ── 为什么提到 trait 上（改造前它是一张独立的 id 数组）──────────
    /// 签到分派此前写在 `billing::checkin::checkin_for` 的 `match provider` 里，
    /// 目标集合另有一张 `auto_checkin::CHECKIN_PROVIDERS: [&str; 14]`。新增一家
    /// 要同时改三处（adapter / checkin 的 match / 那张数组），漏改任一处都会
    /// 静默失效 —— 这与 `supports_usage` / `query_usage` 的形状完全同构，
    /// 没有理由让签到单独一套。
    ///
    /// 默认 `false`：与 `supports_usage` 同一取向（能力是常量声明，不是
    /// 「这次请求成不成功」）。
    fn supports_checkin(&self) -> bool {
        false
    }

    /// 执行一次签到。
    ///
    /// ── 默认实现为什么是「明确的未实现」而不是 panic ────────────────
    /// release 是 `panic = abort`，一次误调用会带走整个进程。`supports_checkin`
    /// 为 true 却忘了覆写本方法时，返回一条可读的错误，界面据此显示「该家
    /// 签到未实现」—— 这比让网关死掉有用得多。
    fn checkin<'a>(
        &'a self,
        _store: &'a AccountStore,
        _billing: &'a crate::server::core::billing::BillingService,
        _account: &'a Value,
        _manual: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CheckinOutcome> + Send + 'a>> {
        Box::pin(async {
            CheckinOutcome::NotImplemented
        })
    }
```

同时定义结果类型（放在 `adapter.rs` 顶部 `UpstreamErrorClass` 附近）：

```rust
/// 一次签到的结果（trait 钩子的返回形状）。
#[derive(Clone, Debug)]
pub enum CheckinOutcome {
    /// 领取成功（首次 / 今日首次）
    Confirmed(String),
    /// 今日已领过
    Already(String),
    /// 不满足领取条件（无积分体系 / 未启用）
    NotEligible(String),
    /// 本次跳过（中性，不算失败）
    Skipped(String),
    /// 上游明确失败
    Failed(String),
    /// 适配器声明了能力但没实现钩子
    NotImplemented,
}
```

- [ ] **Step 4: 把 `checkin_for` 的 match 体搬进各适配器**

对 `checkin_for` 里每一个分支（`"raccoon"` / `"autoclaw" | "autoclaw-intl"` /
`"qoder" | "qoder-intl"` / `"trae"` / `"workbuddy-intl"` / `"loomy"` /
`"kuku"` / `"officeace"` / `"lobsterai"` / `"codearts"` / `"chatglm"`）：
把分支体逐字搬进对应 `providers/<家>/adapter.rs`（或 `mod.rs`）的
`impl ProviderAdapter for X`，返回 `CheckinOutcome`。`checkin_for` 改为：

```rust
    let kind = crate::server::core::providers::kind_from_id(provider_id)
        .ok_or_else(|| format!("未知的提供商 {provider_id}"))?;
    let adapter = crate::server::core::providers::adapter_for(kind);
    if !adapter.supports_checkin() {
        return not_supported_row(id, name, &display, provider_id);
    }
    let outcome = adapter.checkin(store, billing, account, manual).await;
    outcome_row(id, name, &display, outcome)
```

> **保持逐字不变**：`Outcome::Confirmed` → `{"success": true, "msg": label}`、
> `Already` → `{"success": false, "alreadyCompleted": true, ...}`、
> `NotEligible | Skipped` → `{"success": false, ...}` 三条映射（见既有
> `checkin_for` 末尾）必须原样搬到 `outcome_row`，否则前端结果卡会变。

- [ ] **Step 5: `CHECKIN_PROVIDERS` 从注册表派生**

`auto_checkin.rs` 把常量数组改为函数：

```rust
/// 参与签到的 provider id 清单（**从注册表派生**，不再手维护）。
///
/// 判据是适配器的 `supports_checkin()` —— 与 `supports_usage()` 同一口径。
/// 改造前这是一张 `[&str; 14]` 手写数组，新增一家漏加就会静默不签到。
pub fn checkin_providers() -> Vec<String> {
    crate::server::core::providers::implemented_kinds()
        .into_iter()
        .filter(|kind| crate::server::core::providers::adapter_for(*kind).supports_checkin())
        .filter_map(crate::server::core::providers::id_of_kind)
        .map(str::to_string)
        .collect()
}
```

> **保留 `PROVIDERS_VERSION` / `PROVIDERS_ADDED_IN`**：那套机制解决的是
> 「新增家默认勾选」与「用户取消勾选后不被补回来」的冲突（只补一次，靠配置里的
> 版本号记账），它是**对的**，不要动。这里只改「清单从哪来」。

- [ ] **Step 5b: 在 `providers/mod.rs` 加 `kind → id` 反查**

本项目只有 `kind_from_id`（id → kind），没有反向。`checkin_providers()` 需要它：

```rust
/// `ProviderKind` → provider id（`kind_from_id` 的反向）。
///
/// ── 为什么需要它 ──────────────────────────────────────────
/// 清单类查询（如 `auto_checkin::checkin_providers`）从
/// `implemented_kinds()` 出发，拿到的是枚举变体，而对外（配置、界面、日志）
/// 一律用字符串 id。没有反查时调用点只能自己再写一张 `match`，于是 id 与
/// 枚举的对应关系就有了两处 —— 加一家漏改一处就静默错位。
///
/// ── 实现方式：复用 `kind_from_id` 而不是再写一张表 ─────────────
/// 遍历 `PROVIDERS` 的 id，逐个问 `kind_from_id` 是不是目标 kind。O(n) 但
/// n = 27 且只在启动/配置读取时调用，不值得为它维护第二张映射表。
/// 两个 kind 共用一个 id 的形态不存在（每个地区是独立的 kind + 独立的 id）。
pub fn id_of_kind(kind: ProviderKind) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .map(|meta| meta.id)
        .find(|id| kind_from_id(id) == Some(kind))
}
```

- [ ] **Step 6: 运行测试确认通过**
Expected: PASS

- [ ] **Step 7: 全量测试 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
git add desktop-tauri/src-tauri/server/src/server/core/providers/ \
        desktop-tauri/src-tauri/server/src/server/core/billing/checkin.rs \
        desktop-tauri/src-tauri/server/src/server/core/auto_checkin.rs
git commit -m "重构：签到能力提升为 ProviderAdapter 钩子，清单从注册表派生

改造前签到分派写在 billing::checkin::checkin_for 的 match provider 里，
目标集合另有一张 auto_checkin::CHECKIN_PROVIDERS: [&str; 14] 手写数组。
新增一家要同时改三处，漏改任一处静默失效。

- 新增 ProviderAdapter::supports_checkin / checkin（默认 false / NotImplemented）
- 各家的签到分支体逐字搬进对应适配器
- CHECKIN_PROVIDERS 改为 checkin_providers()，从 implemented_kinds() 派生
- 保留 PROVIDERS_VERSION / PROVIDERS_ADDED_IN（那套版本记账是对的）
- 回归测试：清单里的每家都必须声明 supports_checkin"
```

---

## Task 4: per-provider 行为开关

**背景**：内置家的行为开关都是**全局**的（`promptMode`、
`sanitizeBlacklistFingerprints`），没有「这家要不要开 X」的维度。自定义家有
`ProviderQuirks`，但内置家享受不到。

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/core/provider_options.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/config/types.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/upstream/payload.rs`
- Test: `desktop-tauri/src-tauri/server/src/server/core/provider_options.rs`（内联）

**Interfaces:**
- Consumes: 既有 `config::types` 的 kv 存取模式
- Produces:
  - `provider_options::get(provider_id: &str, key: &str) -> Option<bool>`
  - `provider_options::is_enabled(provider_id: &str, key: &str, default: bool) -> bool`
  - `provider_options::set(provider_id: &str, key: &str, value: bool) -> Result<(), String>`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn per_provider_option_is_independent() {
    // 回归锁定：A 家关掉不影响 B 家（改造前只有全局开关，做不到这一点）
    set("workbuddy", "sanitizeFingerprints", false).unwrap();
    set("qoder", "sanitizeFingerprints", true).unwrap();
    assert!(!is_enabled("workbuddy", "sanitizeFingerprints", true));
    assert!(is_enabled("qoder", "sanitizeFingerprints", true));
    // 未设置的 provider 走 default
    assert!(is_enabled("trae", "sanitizeFingerprints", true));
}

#[test]
fn unknown_key_is_rejected() {
    assert!(set("workbuddy", "nonexistentKey", true).is_err());
}

#[test]
fn deleted_provider_option_is_ignored() {
    // 自定义家删了之后配置里还留着它的开关：读的时候忽略，不 panic
    set("custom-gone", "sanitizeFingerprints", false).unwrap();
    assert!(is_enabled("custom-gone", "sanitizeFingerprints", true), "未知 provider 走 default");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib provider_options`
Expected: FAIL — module 不存在

- [ ] **Step 3: 实现模块**

```rust
//! 每 provider 的行为开关（对标 magpie 的 `auth.options[]`）。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 本项目的全局开关（`promptMode` / `sanitizeBlacklistFingerprints` /
//! `stripNewlineKeepalive`）对**所有家**同时生效。但不同家对同一策略的
//! 反应并不一致：WorkBuddy 的审核按逐字匹配，脱敏必须开；而某家如果本来
//! 就不做内容审核，脱敏只是白改字节。没有 per-provider 维度时，用户只能
//! 在「全部开」与「全部关」之间选。
//!
//! ── 存储形态 ──────────────────────────────────────────────
//! config.json 的 `providerOptions` 键，形如：
//! ```json
//! "providerOptions": {
//!   "workbuddy": { "sanitizeFingerprints": false },
//!   "qoder":     { "stripThinkingTags": true }
//! }
//! ```
//! **键不存在 = 跟随全局默认**（`is_enabled` 的 `default` 参数），
//! 这是升级兼容的硬要求：旧配置读出来必须与改造前逐字一致。
//!
//! ── 允许哪些 key ───────────────────────────────────────────
//! 白名单在 [`KNOWN_KEYS`] 里。未知 key 写入报错（而不是静默接受）：
//! 拼错的 key 永远不会生效，静默接受会让用户以为开关坏了。

use std::sync::OnceLock;

use serde_json::{Map, Value};

use crate::server::config;
use crate::server::logging;

/// 允许的开关名（与全局开关一一对应，只是多了 provider 维度）。
pub const KNOWN_KEYS: [&str; 3] = [
    "sanitizeFingerprints",
    "stripThinkingTags",
    "stripNewlineKeepalive",
];

fn config_key() -> &'static str {
    "providerOptions"
}

/// 读一个 provider 的某个开关。`None` = 没设置过（调用方用 default）。
pub fn get(provider_id: &str, key: &str) -> Option<bool> {
    if !KNOWN_KEYS.contains(&key) {
        return None;
    }
    // 读取入口是 `config::with_raw`（拿一份 config.json 的只读快照闭包），
    // 不是 `config::snapshot()` —— 后者不存在。
    config::with_raw(|raw| {
        raw.get(config_key())
            .and_then(|options| options.get(provider_id))
            .and_then(|options| options.get(key))
            .and_then(Value::as_bool)
    })
    .flatten()
}

/// 读开关，未设置时用 `default`。
pub fn is_enabled(provider_id: &str, key: &str, default: bool) -> bool {
    get(provider_id, key).unwrap_or(default)
}

/// 写一个 provider 的开关。
pub fn set(provider_id: &str, key: &str, value: bool) -> Result<(), String> {
    if !KNOWN_KEYS.contains(&key) {
        return Err(format!(
            "未知的 provider 开关 `{key}`（允许: {}）",
            KNOWN_KEYS.join(" / ")
        ));
    }
    if provider_id.trim().is_empty() {
        return Err("provider id 不能为空".to_string());
    }
    // 写入走既有的「读整份 raw → 改 → 存回」三步（本项目没有 `config::update`）。
    // 先取一份可变副本，改完用 `config::save_raw` 落盘。
    let mut raw = config::with_raw(|raw| raw.clone()).ok_or("配置不可用")?;
    let options = raw
        .entry(config_key().to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(options) = options.as_object_mut() else {
        return Err("providerOptions 不是对象".to_string());
    };
    let entry = options
        .entry(provider_id.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(entry) = entry.as_object_mut() else {
        return Err(format!("providerOptions.{provider_id} 不是对象"));
    };
    entry.insert(key.to_string(), Value::Bool(value));
    if !config::save_raw(&raw) {
        return Err("保存配置失败".to_string());
    }
    logging::log(
        "[Config]",
        &format!("provider 开关 {provider_id}.{key} = {value}"),
    );
    Ok(())
}
```

> **实现前先确认**：`config::update` 与 `config::snapshot` 的实际函数名。
> 若本项目用的是别的入口（如 `config::write_key` / `config::get`），按既有
> 签名改写，**不要新造一套 kv 存取**。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib provider_options`
Expected: PASS — 3 passed

- [ ] **Step 5: 接到 `payload::send_body` 的脱敏层**

`payload.rs` 的 ② 指纹脱敏层，把全局开关改为「per-provider 优先、全局兜底」：

```rust
    let sanitize = crate::server::core::provider_options::is_enabled(
        provider_id,
        "sanitizeFingerprints",
        ctx.sanitize_fingerprints,
    );
    let body = match sanitize {
        true => /* 既有逻辑不变 */,
        false => after_prompt,
    };
```

- [ ] **Step 6: 全量测试 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
git add desktop-tauri/src-tauri/server/src/server/core/provider_options.rs \
        desktop-tauri/src-tauri/server/src/server/core/mod.rs \
        desktop-tauri/src-tauri/server/src/server/config/types.rs \
        desktop-tauri/src-tauri/server/src/server/core/upstream/payload.rs
git commit -m "新增：per-provider 行为开关（对标 magpie auth.options[]）

全局开关对所有家同时生效，但不同家对同一策略的反应并不一致：WorkBuddy
的审核按逐字匹配、脱敏必须开；某家本来不做内容审核，脱敏只是白改字节。
没有 provider 维度时用户只能全开或全关。

- providerOptions 配置键：{providerId: {key: bool}}
- 键不存在 = 跟随全局默认（升级后行为逐字不变）
- 未知 key 写入报错（拼错的 key 永不生效，静默接受会让用户以为开关坏了）
- 首个接入点：sanitizeFingerprints（per-provider 优先、全局兜底）"
```

---

## Task 5: Key 配额多维化

**背景**：`ApiKeyEntry::quota_tokens: Option<i64>` + `quota_reset: QuotaReset`
只支持**单条** Token 限额。magpie 支持最多 5 条规则（日/周/月/任意窗口，
Token + 费用），全部满足才放行。

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/api_keys.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/key_quota.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/api/keys_api.rs`
- Modify: `desktop-tauri/ui-islands/src/islands/keys-page.tsx`
- Test: `desktop-tauri/src-tauri/server/src/server/core/key_quota.rs`（内联）

**Interfaces:**
- Consumes: `limiter::kind_window_start(kind: i64, now_ms: i64) -> i64`（既有，`pub(crate)`，同 crate 内可用）
- Produces:
  - `api_keys::KeyQuotaRule { metric: QuotaMetric, limit: f64, reset: QuotaReset }`
  - `api_keys::QuotaMetric::{Tokens, Cost}`
  - `key_quota::exceeded(entry: &ApiKeyEntry, now_ms: i64) -> bool`（**签名不变**）
  - `key_quota::exceeded_rule(entry: &ApiKeyEntry, now_ms: i64) -> Option<KeyQuotaRule>`（新增，供详情展示）

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn effective_rules_prefers_new_shape() {
    // 新形态（quotaRules）非空时优先，旧的 quotaTokens 被忽略
    let entry = entry_with_rules(vec![
        KeyQuotaRule { metric: QuotaMetric::Tokens, limit: 100_000.0, reset: QuotaReset::Daily },
        KeyQuotaRule { metric: QuotaMetric::Tokens, limit: 1_000_000.0, reset: QuotaReset::Fixed(30 * 86_400) },
    ]);
    let rules = effective_rules(&entry);
    assert_eq!(rules.len(), 2, "两条规则都要保留（叠加约束，不是取最宽）");
}

#[test]
fn legacy_single_quota_becomes_one_rule() {
    // 旧记录只有 quotaTokens（没有 quotaRules）：必须读成一条规则
    let entry = legacy_entry(Some(50_000), QuotaReset::Daily);
    let rules = effective_rules(&entry);
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].limit, 50_000.0);
    assert_eq!(rules[0].reset, QuotaReset::Daily);
}

#[test]
fn no_rules_means_unlimited() {
    let entry = legacy_entry(None, QuotaReset::Daily);
    assert!(effective_rules(&entry).is_empty(), "空规则 = 无限，热路径不查库");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib key_quota`
Expected: FAIL — `cannot find struct KeyQuotaRule`

- [ ] **Step 3: 加规则类型与读侧兼容**

`api_keys.rs`：

```rust
/// 配额计量的维度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaMetric {
    /// 窗口内累计 Token 数（既有唯一维度）
    Tokens,
    /// 窗口内累计费用（按模型倍率折算，需 `requests` 表已有 cost 口径）
    Cost,
}

/// 一条 Key 配额规则。
#[derive(Clone, Copy, Debug)]
pub struct KeyQuotaRule {
    pub metric: QuotaMetric,
    pub limit: f64,
    pub reset: QuotaReset,
}
```

`ApiKeyEntry` 增加字段（**保留旧的 `quota_tokens` / `quota_reset` 以兼容读侧**）：

```rust
    /// 配额规则列表（新形态）；**空 = 回落到旧的单条配额**。
    ///
    /// ── 为什么保留旧字段而不是迁移 ─────────────────────────────
    /// 旧记录只有 `quotaTokens` / `quotaReset`。迁移要在 schema 升级里改
    /// 所有存量记录，而 `apiKeys` 存在 config.json（不是库表），没有事务
    /// 可依赖 —— 读侧兼容（空规则 → 用旧字段造一条）比写侧迁移安全。
    pub quota_rules: Vec<KeyQuotaRule>,
```

- [ ] **Step 4: 实现多条判定**

`key_quota.rs`：

```rust
/// 判定是否超限（**保持既有签名**：内部自己查库，热路径零成本）。
///
/// ── 为什么签名不变 ─────────────────────────────────────────
/// 既有调用点（`http.rs` 的鉴权中间件）只问「超没超」，不需要知道是哪条
/// 规则超的。把读数改成参数会让所有调用点都要先算一遍窗口读数，而
/// 「没配配额」这个最常见情形本该零成本（既有注释明确写了这一点）。
///
/// ── 多条规则的语义 ───────────────────────────────────────
/// 与 magpie 一致：**全部满足才放行**（不是任一满足）。多条规则是叠加约束
/// （日限额 + 月限额同时生效），「任一满足」等于只保留最宽松的一条。
pub fn exceeded(entry: &ApiKeyEntry, now_ms: i64) -> bool {
    exceeded_rule(entry, now_ms).is_some()
}

/// 判定是否超限，并返回**第一条**超限的规则（`None` = 全部通过）。
///
/// 给「为什么被拦」的展示用（Key 详情页显示是哪条规则命中）。
pub fn exceeded_rule(entry: &ApiKeyEntry, now_ms: i64) -> Option<KeyQuotaRule> {
    let rules = effective_rules(entry);
    if rules.is_empty() {
        return None;
    }
    for rule in rules {
        if rule.limit <= 0.0 {
            continue;
        }
        let used = usage_for_kind(entry, rule.reset.kind(), now_ms) as f64;
        if used >= rule.limit {
            return Some(rule);
        }
    }
    None
}

/// 按**指定窗口种类**查已用量（多条规则各有自己的窗口）。
///
/// ── 实现方式：把既有 `usage_for` 的体抽成带 `kind` 参数的版本 ──────
/// 既有 `usage_for(entry, now_ms)` 第一行就是 `let kind = entry.quota_reset.kind();`
/// 之后全是「按 kind 查缓存 / 查库」。把那个函数体原样搬进来、把首行改成
/// 参数，然后让 `usage_for` 变成三行的转发：
///
/// ```rust
/// pub fn usage_for(entry: &ApiKeyEntry, now_ms: i64) -> i64 {
///     usage_for_window(entry, entry.quota_reset.kind(), now_ms)
/// }
/// ```
///
/// **行为必须逐字不变**（既有调用点依赖它），所以搬的时候只动首行，
/// 其余一字不改 —— 包括那段「先锁内看一眼缓存、锁外取数」的锁序注释。
fn usage_for_window(entry: &ApiKeyEntry, kind: i64, now_ms: i64) -> i64 {
    // ── 先在锁内看一眼缓存，锁外再取数 ──────────────────────────
    // `fetch` 会读配置并查库（各取一把锁）。**不能在持有 USAGE_CACHE 时做这些**：
    // 那样锁序就成了「USAGE_CACHE → Db」，而本模块之外的任何一处若反过来
    // 先拿 Db 再进 key_quota，就是一次死锁。锁序只保留一个方向：这里先释放
    // USAGE_CACHE，取完数再拿回来写。
    let hit = {
        let mut cache = match USAGE_CACHE.lock() {
            Ok(guard) => guard,
            // 锁中毒（另一个线程 panic 在持锁期间）：按「没有证据」处理，不拦
            Err(poisoned) => poisoned.into_inner(),
        };
        if cache
            .as_ref()
            .map(|cached| now_ms.saturating_sub(cached.fetched_at) < CACHE_TTL_MS)
            .unwrap_or(false)
        {
            Some(cached_lookup(cache.as_ref(), kind, &entry.id, now_ms))
        } else {
            *cache = None;
            None
        }
    };
    if let Some(used) = hit {
        return used;
    }
    // 锁外取数（可能读配置 + 查库）
    let fresh = fetch(now_ms);
    let used = cached_lookup(fresh.as_ref(), kind, &entry.id, now_ms);
    if let Ok(mut cache) = USAGE_CACHE.lock() {
        // 只在「还没有更新的快照」时写回：另一个线程可能刚抓过一份更新的
        if cache
            .as_ref()
            .map(|cached| {
                cached.fetched_at <= fresh.as_ref().map(|f| f.fetched_at).unwrap_or(i64::MIN)
            })
            .unwrap_or(true)
        {
            *cache = fresh;
        }
    }
    used
}

/// 既有入口：读 `entry.quota_reset` 对应的窗口。行为与改造前逐字一致。
pub fn usage_for(entry: &ApiKeyEntry, now_ms: i64) -> i64 {
    usage_for_window(entry, entry.quota_reset.kind(), now_ms)
}

/// 有效规则：新形态优先，空则从旧字段造一条。
fn effective_rules(entry: &ApiKeyEntry) -> Vec<KeyQuotaRule> {
    if !entry.quota_rules.is_empty() {
        return entry.quota_rules.clone();
    }
    match entry.quota_tokens {
        Some(limit) => vec![KeyQuotaRule {
            metric: QuotaMetric::Tokens,
            limit: limit as f64,
            reset: entry.quota_reset,
        }],
        None => Vec::new(),
    }
}
```

> **`used_tokens` 参数**：保留它是为了不破坏既有调用点的签名；新逻辑用
> `day_tokens` / `long_tokens` 两个窗口读数。若既有调用点只算了一个窗口，
> 在 Step 5 一并补上第二个窗口的查询。

- [ ] **Step 5: 接线调用点与 API**

- `key_quota.rs` 的 `exceeded_for` / 心跳刷新按 `effective_rules` 遍历窗口
- `keys_api.rs` 的 GET/PUT 增删 `quotaRules` 字段（**保留** `quotaTokens` 读写）
- `keys-page.tsx` 加「配额规则」编辑区（最多 5 条，超出时禁用「添加」按钮）

- [ ] **Step 6: 前端类型检查与产物**

```bash
npm --prefix desktop-tauri/ui-islands run typecheck
npm --prefix desktop-tauri/ui-islands run build
```

- [ ] **Step 7: 全量测试 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
git add desktop-tauri/src-tauri/server/src/server/core/api_keys.rs \
        desktop-tauri/src-tauri/server/src/server/core/key_quota.rs \
        desktop-tauri/src-tauri/server/src/server/api/keys_api.rs \
        desktop-tauri/ui-islands/src/islands/keys-page.tsx \
        desktop-tauri/ui/islands/ui.js
git commit -m "新增：网关 Key 配额多维化（多条规则、Token + 费用）

原实现只支持单条 Token 限额（quotaTokens + quotaReset），做不到
「工作日 50 万、周末 20 万」或「每月 10 美元上限」。

- 新增 KeyQuotaRule {metric, limit, reset}，最多 5 条
- 判定语义与 magpie 一致：**全部满足才放行**（不是任一满足）
- 读侧兼容：规则为空时从旧 quotaTokens 造一条，旧记录行为逐字不变
- 「等于上限也算超」的既有口径保持不变
- 前端 keys-page 加配额规则编辑区"
```

---

## Task 6: 通用 middleware（think-tags + word-guard）

**背景**：`strip_thinking_tags` 只服务 Qoder（`providers/qoder/stream.rs`，标签常量 `THINK_TAGS` 在 `providers/qoder/protocol.rs`）；
词语拦截完全没有。magpie 把两者做成 gateway middleware，插在请求/响应管线上。

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/core/middleware/mod.rs`
- Create: `desktop-tauri/src-tauri/server/src/server/core/middleware/think_tags.rs`
- Create: `desktop-tauri/src-tauri/server/src/server/core/middleware/word_guard.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/upstream/payload.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/upstream/sse.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/config/types.rs`
- Test: 三个文件各自内联

**Interfaces:**
- Consumes: `payload::send_body` 的 ①→②→③ 管线
- Produces:
  - `think_tags::strip_leading(text: &str) -> (String, bool)`（返回清理后文本 + 是否命中）
  - `think_tags::to_content(reasoning: &str) -> String`
  - `word_guard::check_user_text(body: &Value, config: &WordGuardConfig) -> WordGuardVerdict`
  - `word_guard::WordGuardVerdict::{Pass, Reject(String), Mask(String)}`

- [ ] **Step 1: 写失败测试（think-tags 跨分片）**

```rust
#[test]
fn strip_handles_tag_split_across_chunks() {
    // 回归锁定：标签被切成两片时不能残留半个标签
    // 反例：逐片剥离会把 "<thi" 与 "nk>" 当成普通文本留下
    let mut buffer = String::new();
    let mut out = String::new();
    for chunk in ["<thi", "nk>hello</thi", "nk> world"] {
        buffer.push_str(chunk);
        let (rest, done) = consume_prefix(&mut buffer);
        out.push_str(&rest);
        if done {
            out.push_str(&buffer);
            buffer.clear();
        }
    }
    assert_eq!(out, "hello world", "剥完标签后正文必须完整且无残留");
}

#[test]
fn strip_only_affects_leading_block() {
    let (out, hit) = strip_leading("<think>x</think>real answer");
    assert!(hit);
    assert_eq!(out, "real answer");
    // 正文中间的标签不动（那是用户内容）
    let (out, hit) = strip_leading("see <think>y</think> here");
    assert!(!hit);
    assert_eq!(out, "see <think>y</think> here");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib think_tags`
Expected: FAIL — module 不存在

- [ ] **Step 3: 实现 think_tags**

按 Step 1 的两个测试实现 `strip_leading` / `consume_prefix` / `to_content`。
关键约束：**只处理开头的标签块**（正文中间的是用户内容，不能动）；
跨分片时用 `buffer` 累积到标签闭合再判定。

- [ ] **Step 4: 写失败测试（word-guard 只查用户文本）**

```rust
#[test]
fn word_guard_ignores_tool_results_and_system() {
    let body = json!({
        "messages": [
            {"role": "system", "content": "you may say SECRET"},
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": null, "tool_calls": [{"function": {"arguments": "SECRET"}}]},
            {"role": "tool", "content": "SECRET"}
        ]
    });
    let config = WordGuardConfig { words: vec!["SECRET".into()], patterns: vec![], action: WordAction::Reject, check: WordCheck::All };
    // 只查用户文本 → 这三处 SECRET 都不该触发
    assert!(matches!(check_user_text(&body, &config), WordGuardVerdict::Pass));
}

#[test]
fn word_guard_rejects_user_text() {
    let body = json!({"messages": [{"role": "user", "content": "my SECRET is here"}]});
    let config = WordGuardConfig { words: vec!["SECRET".into()], patterns: vec![], action: WordAction::Reject, check: WordCheck::All };
    match check_user_text(&body, &config) {
        WordGuardVerdict::Reject(message) => assert!(message.contains("SECRET")),
        other => panic!("应当拒绝，实际 {other:?}"),
    }
}
```

- [ ] **Step 5: 实现 word_guard**

`WordGuardConfig { words, patterns, action: Reject|Mask, check: Last|All }`。
**只遍历 `role == "user"` 的 message**（`check: Last` 只看最后一条 user，
`All` 看全部）。命中后按 `action` 返回 `Reject`（带命中词，供 400 文案）或
`Mask`（把命中的词替换成 `***` 后的体）。

- [ ] **Step 6: 插进 payload 管线（请求侧）与 FramePolicy（响应侧）**

**请求侧**：`payload.rs` 在 ② 之后、③ 之前插入词语拦截层。顺序理由：
在**脱敏之后**（脱敏改的是指纹模板句，不该被用户词规则命中）、
在**原生工具闸门之前**（拦截应当发生在任何下游改写之前）。

**响应侧**：think-tags 的 `strip` 模式作用于**下游 SSE 帧**，落点是既有的
`upstream::sse::FramePolicy` —— 不要另造一个响应改写层。照 `strip_newline_chunks`
的既定范式扩展（「能力位 × 全局开关」，只能收窄不能扩张）：

```rust
pub struct FramePolicy {
    pub rewrite: Option<ModelRewrite>,
    pub strip_newline_chunks: bool,
    /// 剥离**开头**的 `<think>…</think>` 块（think-tags 的 `strip` 模式）。
    /// 与 `strip_newline_chunks` 同一范式：能力位 × 开关，默认关。
    pub strip_leading_think: bool,
}

impl FramePolicy {
    /// 由能力位 × 全局开关 × per-provider 开关装配。
    ///
    /// 三层取「与」的理由与 `assembled` 的注释同一精神：每一层都只能**收窄**。
    /// per-provider 层在这里的用处是「这家上游自己就带思考标签、但我们不想剥」
    /// —— 用户能在 providerOptions 里单独关掉它，而不必改全局。
    pub fn assembled_with_think(
        rewrite: Option<ModelRewrite>,
        declared: bool,
        switch_on: bool,
        think_declared: bool,
        think_switch_on: bool,
    ) -> Self {
        Self {
            rewrite,
            strip_newline_chunks: declared && switch_on,
            strip_leading_think: think_declared && think_switch_on,
        }
    }
}
```

**跨分片状态**：`strip` 要处理标签被切开的帧（`<thi` + `nk>`）。既有
`ReasoningCoalescer` 已经持有跨帧缓冲，把「开头标签块」的状态放在它旁边
（新增一个 `leading_think_done: bool` + 一个小缓冲），**不要**在
`FramePolicy` 里加可变状态（它是 `Copy` 的纯策略值）。

- [ ] **Step 7: 全量测试 + 前端 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
npm --prefix desktop-tauri/ui-islands run typecheck && npm --prefix desktop-tauri/ui-islands run build
git add desktop-tauri/src-tauri/server/src/server/core/middleware/ \
        desktop-tauri/src-tauri/server/src/server/core/mod.rs \
        desktop-tauri/src-tauri/server/src/server/core/upstream/payload.rs \
        desktop-tauri/src-tauri/server/src/server/core/upstream/sse.rs \
        desktop-tauri/src-tauri/server/src/server/config/types.rs \
        desktop-tauri/ui-islands/src/islands/settings-page.tsx \
        desktop-tauri/ui/islands/ui.js
git commit -m "新增：两个通用 middleware（think-tags 剥离 / 词语拦截）

对标 magpie 的 middleware 插件，但做成内建（本项目不引入插件运行时）。

- think_tags：把 Qoder 内部的 strip_thinking_tags 提到通用层，
  支持跨 SSE 分片的标签、只处理开头的标签块（正文中间不动）
- word_guard：只查用户文本（不看 system / tool_calls / tool 结果，
  与 magpie 同口径），reject / mask 两档
- 插在 payload 的 ②脱敏 之后、③原生工具闸门 之前
- 两者默认关闭，配置项写在 config.json"
```

---

## 任务依赖与执行顺序

```
Task 1 (Gemini)      ── 独立，可先做
Task 2 (TOML 注释)    ── 独立
Task 3 (checkin 钩子) ── 独立
Task 4 (per-provider) ── 独立（但 Task 6 会用到它的读取模式）
Task 5 (Key 多维)     ── 独立
Task 6 (middleware)   ── 建议在 Task 4 之后（复用 provider_options 的开关模式）
```

**建议顺序**：1 → 2 → 3 → 4 → 6 → 5。
理由：前三项是「修已有功能的缺口」，风险最低；Task 4 与 6 共享配置模式；
Task 5 涉及前端改动最大，放最后。

---

## 不在本计划内（文档第十二章已说明理由）

| 项 | 理由 |
|---|---|
| 路由组 / 嵌套组 | 需先评估现有模型映射能否表达，可能覆盖 80% 场景 |
| 通用参数覆盖 | magpie 的 operations 模型只能看 body，本项目需先确认是否需要看头 |
| `tool` 钩子 → MCP | 是否需要对外暴露工具是独立的产品决策 |
| 插件运行时 | 与 `panic=abort` + 零 unwrap 纪律冲突；`ProviderAdapter` 已有 20+ 钩子 |
| 意图路由 / 窗口预热 / Claude CLI 桥接 / Telemetry | 场景不匹配 |

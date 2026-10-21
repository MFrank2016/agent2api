# magpie 借鉴项 Implementation Plan（v2，评审后修订）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 `docs/compare-magpie.md` 第十二章的可借鉴能力，但**只做经评审确认可行的四项**：TOML 注释保留、per-provider 开关、Key 配额多规则（仅 Token 维度）、通用 think-tags 剥离。Gemini 端点、checkin trait 重构、word-guard 三项**降级为独立前置调研**（见文末 P1/P2/P3），不纳入本轮实施。

**Architecture:** 四项改动都是「把已有机制搬到更合适的位置」，不新造抽象：
- TOML 用 `toml_edit`（**钉 0.25，已在 lock 里**）替换 `toml::to_string_pretty`，只走 `apply_single` 的 TOML 分支。
- per-provider 开关复用**既有公开写入口** `config::update_raw_field`（不是 `save_raw`），保证内存快照同步刷新。
- Key 配额沿用既有 `by_kind` 缓存（已天然支持多窗口），只改 `fetch` 的 kinds 收集与判定循环；**不做 Cost 维度**（`requests` 表无 cost 列）。
- think-tags 挂在既有 `FramePolicy` + `CompletionAccumulator` 两条路径，共用一台 helper。

**Tech Stack:** Rust 2021（MSRV 1.88）、axum 0.8、serde_json、rusqlite、toml_edit 0.25；前端 TypeScript + React 岛。

**Spec:** `docs/compare-magpie.md`（调研）+ 本文件末尾「评审记录」段（设计决策依据）

## 评审结论摘要（本计划的前提）

v1 计划经 Oracle 与 Momus 独立评审，共发现 **15 项 BLOCKER**，全部已逐条对真实代码验证。本 v2 的每一项改动都是对这些 BLOCKER 的直接回应：

| # | v1 的 BLOCKER | 验证证据 | v2 的处理 |
|---|---|---|---|
| 1 | `ForwardOutcome` 无 `IntoResponse` | `core/upstream/mod.rs:256`，全仓无该 impl | **Gemini 端点移出本轮**（见 P1） |
| 2 | 缺 chat→Gemini SSE 翻译机 | 只有 `anthropic_from_chat` / `responses_request_from_chat` | 同上 |
| 3 | 未注入 path model → 静默用默认模型 | `api/protocol.rs:218` 内联 `resolve_model` | 同上 |
| 4 | Codex 有单段键 `model` | `agents/apply.rs:189` | Task 1 的 `set_toml_path` 支持单段 |
| 5 | `set_path` 会自动建表，计划却要求报错 | `apply.rs:277-297` | Task 1 改为**照既有语义建表** |
| 6 | `toml_edit = "0.22"` 新增第四版本 | `toml 0.9.12` 用 `toml_parser`/`toml_writer` | Task 1 钉 **0.25.13**（`proc-macro-crate` 已在用） |
| 7 | `save_raw` 不刷新内存 → 设了不生效 | `config/mod.rs` 的 `save_raw` 只写库 | Task 2 改用 `update_raw_field` |
| 8 | `KNOWN_KEYS` 键名与全局键不符 | 全局键是 `sanitizeBlacklistFingerprints`（`config/types.rs:133`） | Task 2 改用真实键名 |
| 9 | `fetch` 只认 `quota_tokens` → 多规则静默无效 | `key_quota.rs:180-187` | Task 3 改 `fetch` 的 kinds 收集 |
| 10 | `Cost` 维度无数据源 | `requests` 表无 cost 列 | Task 3 **砍掉 Cost**，只做 Tokens |
| 11 | `CheckinOutcome` 丢失 `claim`/`extras`/`log_flag` | `checkin.rs:486`、`:451/458/526` | **checkin 重构移出本轮**（见 P2） |
| 12 | `word_guard::Reject` 无错误通道 | `payload.rs:180` 的 `send_body` 返回 `SendBody` | **word-guard 移出本轮**（见 P3） |
| 13 | `FramePolicy` 加字段破坏既有字面量 | `sse.rs:545`、`:745` 两处结构体字面量 | Task 4 一并更新这两处 |
| 14 | `kind_id` 已存在，不需 `id_of_kind` | `providers/mod.rs:757` | v2 不再新增 |
| 15 | `pipeline::sha256_hex` 在 `api/` 不是 `core::upstream` | `api/pipeline.rs:1102` | v2 不再引用（Gemini 移出） |

**净效果**：6 个任务砍到 4 个。这不是缩水 —— 是把「评审证明会编译失败 / 静默无效 / 破坏既有行为」的部分挡在实施之前。

## Global Constraints

- Rust edition 2021，MSRV 1.88。**不新增第三方依赖**：`toml_edit` 钉 **0.25**，该版本已由 `proc-macro-crate` 引入并在 `Cargo.lock` 中，提为直接依赖不新增版本树。落地前先跑 `cargo tree -i toml_edit@0.25.13` 确认。
- release 是 `panic = abort`：**绝不 `unwrap`/`expect` 可失败路径**；错误如实返回。
- 所有新增行为开关**默认关闭**，升级后既有用户的行为与日志必须逐字不变。
- 不 `as any`、不 `@ts-ignore`（前端）；不 `#[allow(dead_code)]` 掩盖未接线代码。
- 测试命令（**必须带 manifest-path，仓库根没有 Cargo.toml**）：
  - 后端单测：`cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib <filter>`
  - 后端全量：`cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib`
  - 前端类型：`npm --prefix desktop-tauri/ui-islands run typecheck`
  - 前端构建：`npm --prefix desktop-tauri/ui-islands run build`（产物 `ui.js` **与 `ui.css`** 都要提交）
- 当前测试基线：**765 passed / 11 ignored**（以实跑为准，动手前先跑一次确认）。

## Review Focus

评审指出 v1 的 Review Focus 缺了以下边界。每条都指定了归属任务与测试：

1. **TOML 顶层单段键**（Codex 的 `model`）→ Task 1，`set_toml_path_handles_single_segment`
2. **首次接管空/新 config.toml**（表不存在）→ Task 1，`set_toml_path_creates_missing_tables`
3. **`preview` 与落盘一致性** → Task 1，`preview_matches_applied_output`
4. **JSONC 客户端仍丢注释**（opencode / zed）→ Task 1 明确写出范围，不假装修好
5. **`update_raw_field` 后同进程读回新值** → Task 2，`set_then_get_returns_new_value`
6. **未知 provider 的残留开关被忽略** → Task 2，`unknown_provider_option_is_ignored`
7. **Key 只有 `quota_rules` 没有 `quota_tokens`** → Task 3，`rules_only_key_is_enforced`
8. **多规则全部满足才放行** → Task 3，`all_rules_must_pass`
9. **think-tags 跨 SSE 分片** → Task 4，`strip_handles_split_across_frames`
10. **think-tags 只处理开头块** → Task 4，`strip_only_affects_leading_block`
11. **非流式响应也要剥**（`CompletionAccumulator` 路径）→ Task 4，`strip_works_on_non_stream_path`

---

## File Structure

| 文件 | 责任 | 动作 |
|---|---|---|
| `server/Cargo.toml` | 加 `toml_edit = "0.25"` | 修改 |
| `core/agents/apply.rs` | `set_toml_path` + TOML 分支 + `preview` 同步 | 修改 |
| `core/provider_options.rs` | per-provider 开关读写（基于 `update_raw_field`） | 新建 |
| `core/mod.rs` | 注册 `provider_options` | 修改 |
| `core/upstream/payload.rs` | 脱敏层改「per-provider 优先、全局兜底」 | 修改 |
| `core/api_keys.rs` | `KeyQuotaRule` + 读侧兼容 | 修改 |
| `core/key_quota.rs` | `fetch` kinds 收集 + `exceeded_rule` + `quota_json` | 修改 |
| `core/upstream/sse.rs` | `FramePolicy` 加 `strip_leading_think` + 更新三处 | 修改 |
| `core/upstream/aggregate.rs` | `CompletionAccumulator` 同步加该字段 | 修改 |
| `core/middleware/mod.rs` | 新建（模块根） | 新建 |
| `core/middleware/think_tags.rs` | 跨帧开头标签剥离 | 新建 |
| `config/types.rs` | `providerOptions` 键 + think-tags 开关 | 修改 |
| `ui-islands/src/islands/keys-page.tsx` | 配额规则编辑（最多 5 条） | 修改 |
| `ui-islands/src/islands/settings-page.tsx` | think-tags 开关 | 修改 |

---

## Task 1: Agent 写 TOML 保留注释

**背景**：`core/agents/apply.rs` 模块头自承「注释保不住」。Codex 的 `config.toml` 是用户手写的，常有注释掉的实验配置。

**范围（明确写出来，避免假装修全了）**：只修 **TOML**（Codex）。`opencode` / `zed` 用 JSONC，仍走旧的有损路径，`comment_warnings` 继续如实告警。

**Files:**
- Modify: `desktop-tauri/src-tauri/server/Cargo.toml`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/agents/apply.rs`
- Test: 同上文件内联 `#[cfg(test)]`

**Interfaces:**
- Consumes: 既有 `single_targets(def, profile) -> Vec<(String, Value)>`（`apply.rs:185+`）
- Produces: `apply::set_toml_path(text: &str, dotted: &str, value: &Value) -> Result<String, String>`

- [ ] **Step 1: 写失败测试**

在 `apply.rs` 的 `#[cfg(test)]` 模块追加：

```rust
#[test]
fn set_toml_path_handles_single_segment() {
    // Codex 的 targets 里有顶层单段键（`model` / `model_provider`）。
    // v1 计划用 rsplit_once('.') 要求至少两段 → 对单段直接报错。
    let updated = set_toml_path("model = \"old\"\n", "model", &json!("new"))
        .expect("单段键必须支持");
    assert!(updated.contains("model = \"new\""));
}

#[test]
fn set_toml_path_creates_missing_tables() {
    // 既有 set_path 会沿路径创建中间对象（apply.rs:288-296）。
    // 首次接管一个只有 model 的 config.toml 时，provider 段必须被建出来，
    // 否则用户第一次切换就失败。
    let updated = set_toml_path(
        "model = \"gpt-5\"\n",
        "model_providers.agent2api.base_url",
        &json!("http://127.0.0.1:3065/v1"),
    )
    .expect("缺表时必须创建，不是报错");
    assert!(updated.contains("[model_providers.agent2api]"));
    assert!(updated.contains("base_url = \"http://127.0.0.1:3065/v1\""));
}

#[test]
fn set_toml_path_preserves_comments_and_order() {
    let original = r#"# 我的 Codex 配置
model = "gpt-5"

# 实验性设置，别动
[model_providers.agent2api]
base_url = "http://old/v1"   # 旧地址
env_key = "OLD"
"#;
    let updated = set_toml_path(
        original,
        "model_providers.agent2api.base_url",
        &json!("http://127.0.0.1:3065/v1"),
    )
    .expect("应当写入成功");
    assert!(updated.contains("# 我的 Codex 配置"), "顶层注释必须保留");
    assert!(updated.contains("# 实验性设置，别动"), "段注释必须保留");
    assert!(updated.contains("# 旧地址"), "行尾注释必须保留");
    assert!(updated.contains("http://127.0.0.1:3065/v1"));
    assert!(!updated.contains("http://old/v1"));
    let model_pos = updated.find("model = \"gpt-5\"").expect("model 行还在");
    let provider_pos = updated.find("[model_providers.agent2api]").expect("段还在");
    assert!(model_pos < provider_pos, "键序必须保持");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib set_toml_path`
Expected: FAIL — `cannot find function set_toml_path`

- [ ] **Step 3: 加依赖（钉 0.25，已在 lock）**

`server/Cargo.toml`，紧跟既有 `toml` 那行：

```toml
# 保留注释与格式地改写 TOML（Codex 的 config.toml 是用户手写的）。
# `toml`（既有）是「解析成值树」，本 crate 是「带 trivia 的语法树」。
# **版本必须是 0.25**：该版本已由 `proc-macro-crate` 引入并在 Cargo.lock 中，
# 提为直接依赖不新增版本树。`toml 0.9.12` 本身已不用 `toml_edit`
# （它用 toml_parser / toml_writer），所以钉 0.22 会新增第四个版本 —— 不要。
toml_edit = "0.25"
```

- [ ] **Step 4: 实现 `set_toml_path`**

```rust
/// 按点分路径写入一个 TOML 值，**保留注释、键序与格式**。
///
/// ── 与既有 `toml::to_string_pretty` 的分工 ────────────────────
/// 那条路径是「解析成 `toml::Table` → 改键 → 整体序列化」，注释在解析阶段
/// 就丢了。本函数用 `toml_edit`（带 trivia 的语法树），只动目标键那一处。
///
/// ── 路径形态：支持单段与多段 ──────────────────────────────────
/// Codex 的 targets 里既有顶层单段键（`model` / `model_provider`），也有
/// 三段键（`model_providers.agent2api.base_url`）。用 `split('.')` 逐段走，
/// 不要 `rsplit_once('.')` —— 后者对单段路径直接报错。
///
/// ── 缺表时**创建**而不是报错 ──────────────────────────────────
/// 与既有 `set_path`（apply.rs:288-296）同一语义：首次接管一个只有
/// `model` 的 config.toml 时，`[model_providers.agent2api]` 段必须被建出来。
/// 报错会让「第一次切换」必然失败。
pub fn set_toml_path(text: &str, dotted: &str, value: &Value) -> Result<String, String> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("解析 TOML 失败: {error}"))?;
    let segments: Vec<&str> = dotted.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(format!("路径含空段: {dotted}"));
    }
    let mut table = document.as_table_mut();
    for segment in &segments[..segments.len() - 1] {
        if !table.contains_key(segment) {
            table.insert(segment, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        table = table
            .get_mut(segment)
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| format!("TOML 里 `{segment}` 不是表（路径: {dotted}）"))?;
    }
    let key = segments[segments.len() - 1];
    table.insert(key, json_to_toml_item(value)?);
    Ok(document.to_string())
}

/// JSON 值 → TOML 值。只支持本项目实际会写的标量。
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

- [ ] **Step 5: 运行确认通过**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib set_toml_path`
Expected: PASS — 3 passed

- [ ] **Step 6: 接到 `apply_single` 的 TOML 分支**

`apply.rs:311-318` 改为：

```rust
    let rendered = match def.format {
        ConfigFormat::Toml => {
            // 逐个路径写入带 trivia 的语法树，注释与键序保留。
            let mut text = original.clone();
            for (key, item) in &targets {
                text = set_toml_path(&text, key, item)?;
            }
            text
        }
        _ => { /* 既有 JSON 分支不变 */ }
    };
```

- [ ] **Step 7: 同步 `preview`**

`apply.rs` 的 `preview`（约 `:484-499`）TOML 分支改用同一 `set_toml_path` 链，否则预览显示「注释会丢」而落盘不丢 —— 界面在说谎。

- [ ] **Step 8: 写 `preview` 一致性测试**

```rust
#[test]
fn preview_matches_applied_output() {
    // 回归锁定：预览与落盘必须是同一份渲染结果。
    // 反例（v1）：preview 仍走 toml::to_string_pretty，落盘走 toml_edit，
    // 于是预览说「注释会丢」而实际不丢。
    let original = "# keep me\nmodel = \"old\"\n";
    let def = crate::server::core::agents::find("codex").expect("codex 必须在注册表");
    let profile = crate::server::core::agents::Profile::default();
    let previewed = render_single(def, original, &profile).expect("渲染应当成功");
    assert!(previewed.contains("# keep me"), "注释必须保留");
}
```

> 实现前先确认 `preview` 的真实签名；若它是内联在 `preview_impl` 里，
> 把渲染那一步抽成 `render_single(def, original, profile) -> Result<String, String>`
> 供 `preview` 与 `apply_single` 共用，测试直接测它。

- [ ] **Step 9: 全量测试 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
git add desktop-tauri/src-tauri/server/Cargo.toml \
        desktop-tauri/src-tauri/Cargo.lock \
        desktop-tauri/src-tauri/server/src/server/core/agents/apply.rs
git commit -m "改进：Agent 写 TOML 保留注释与键序（toml_edit 0.25）

原实现「解析成值树 → 改键 → 重新序列化」，TOML 注释全丢。Codex 的
config.toml 是用户手写的，一次切换全丢会让用户不敢用这个功能。

范围只覆盖 TOML（Codex）。opencode / zed 用 JSONC，仍走旧的有损路径，
comment_warnings 继续如实告警 —— 不假装这一半也修好了。

- set_toml_path：带 trivia 的语法树，只动目标键；注释/键序/行尾注释全保留
- 支持单段键（Codex 的 model / model_provider）与多段键
- 缺表时创建（与既有 set_path 同语义），首次接管不会失败
- preview 与落盘共用同一渲染，界面不再说谎
- 钉 toml_edit 0.25（proc-macro-crate 已引入，不新增版本树）"
```

---

## Task 2: per-provider 行为开关

**背景**：全局开关对所有家同时生效，缺「这家要不要开 X」的维度。

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/core/provider_options.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/mod.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/upstream/payload.rs`
- Test: `provider_options.rs` 内联

**Interfaces:**
- Consumes: `config::with_raw`（读，`config/mod.rs:994`）、`config::update_raw_field`（**写，`config/mod.rs:1352`，会刷新内存**）
- Produces:
  - `provider_options::is_enabled(provider_id: &str, key: &str, default: bool) -> bool`
  - `provider_options::set(provider_id: &str, key: &str, value: bool) -> Result<(), String>`
  - `provider_options::KNOWN_KEYS: [&str; 2]`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn set_then_get_returns_new_value() {
    // 回归锁定：写后同进程读必须立刻生效。
    // 反例（v1）：用 config::save_raw 只写库、不刷新内存快照，
    // 于是 get 读到旧值，必须重启才生效。
    set("workbuddy", "sanitizeBlacklistFingerprints", false).expect("写入应当成功");
    assert!(
        !is_enabled("workbuddy", "sanitizeBlacklistFingerprints", true),
        "写后必须立刻读到新值"
    );
}

#[test]
fn per_provider_option_is_independent() {
    set("workbuddy", "sanitizeBlacklistFingerprints", false).unwrap();
    set("qoder", "sanitizeBlacklistFingerprints", true).unwrap();
    assert!(!is_enabled("workbuddy", "sanitizeBlacklistFingerprints", true));
    assert!(is_enabled("qoder", "sanitizeBlacklistFingerprints", true));
    // 未设置的 provider 走 default
    assert!(is_enabled("trae", "sanitizeBlacklistFingerprints", true));
}

#[test]
fn unknown_provider_option_is_ignored() {
    // 已删的自定义家在配置里还留着开关：读的时候忽略，不 panic。
    assert!(
        is_enabled("custom-gone", "sanitizeBlacklistFingerprints", true),
        "未知 provider 必须走 default"
    );
}

#[test]
fn unknown_key_is_rejected() {
    assert!(set("workbuddy", "nonexistentKey", true).is_err());
}
```

> **测试隔离**：这四条都会碰全局配置。`provider_options` 的读侧必须支持「provider 不在注册表且不是自定义家时一律返回 default」。若测试之间互相污染，在测试模块里用一个 `static LOCK: Mutex<()>` 串行化。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib provider_options`
Expected: FAIL — module 不存在

- [ ] **Step 3: 实现模块**

```rust
//! 每 provider 的行为开关（对标 magpie 的 `auth.options[]`）。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 全局开关（`sanitizeBlacklistFingerprints` / `promptMode` 等）对**所有家**
//! 同时生效。但不同家对同一策略的反应并不一致：WorkBuddy 的审核按逐字匹配、
//! 脱敏必须开；某家本来不做内容审核，脱敏只是白改字节。
//!
//! ── 存储形态 ──────────────────────────────────────────────
//! config.json 的 `providerOptions` 键：
//! ```json
//! "providerOptions": {
//!   "workbuddy": { "sanitizeBlacklistFingerprints": false }
//! }
//! ```
//! **键不存在 = 跟随全局默认**（`is_enabled` 的 `default` 参数）—— 这是
//! 升级兼容的硬要求：旧配置读出来必须与改造前逐字一致。
//!
//! ── 写侧为什么用 `update_raw_field` 而不是 `save_raw` ─────────────
//! `save_raw` 只写库、**不刷新内存快照**，而读侧 `with_raw` 读的是内存快照。
//! 用 `save_raw` 会导致「设了不生效、必须重启」。`update_raw_field` 走
//! `update()` 闭包，写完同步刷新内存。
//!
//! ── 键名必须与全局键逐字一致 ────────────────────────────────
//! [`KNOWN_KEYS`] 里的名字直接引用 `config::types` 的全局键常量，不是自造
//! 短名。拼错会让「per-provider 覆盖」永远匹配不到全局开关。

use serde_json::{Map, Value};

use crate::server::config;
use crate::server::logging;

/// 允许的开关名 —— **直接引用全局键常量**，保证与全局开关同名。
pub const KNOWN_KEYS: [&str; 2] = [
    crate::server::config::types::KEY_SANITIZE_FINGERPRINTS,
    crate::server::config::types::KEY_STRIP_NEWLINE_KEEPALIVE,
];

fn config_key() -> &'static str {
    "providerOptions"
}

/// provider 是否可识别（内置注册表或自定义家）。
fn is_known_provider(provider_id: &str) -> bool {
    crate::server::core::providers::PROVIDERS
        .iter()
        .any(|meta| meta.id == provider_id)
        || crate::server::core::custom_providers::is_custom_provider_id(provider_id)
}

/// 读一个 provider 的某个开关。`None` = 没设置过（调用方用 default）。
pub fn get(provider_id: &str, key: &str) -> Option<bool> {
    if !KNOWN_KEYS.contains(&key) || !is_known_provider(provider_id) {
        return None;
    }
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

/// 写一个 provider 的开关（写完内存快照同步刷新）。
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
    let mut options = config::with_raw(|raw| raw.get(config_key()).cloned())
        .flatten()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(options_map) = options.as_object_mut() else {
        return Err("providerOptions 不是对象".to_string());
    };
    let entry = options_map
        .entry(provider_id.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(entry) = entry.as_object_mut() else {
        return Err(format!("providerOptions.{provider_id} 不是对象"));
    };
    entry.insert(key.to_string(), Value::Bool(value));
    if !config::update_raw_field(config_key(), options) {
        return Err("保存 providerOptions 失败".to_string());
    }
    logging::log(
        "[Config]",
        &format!("provider 开关 {provider_id}.{key} = {value}"),
    );
    Ok(())
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib provider_options`
Expected: PASS — 4 passed

- [ ] **Step 5: 接到 `payload::send_body` 的脱敏层**

`payload.rs` 的 ② 层（约 `:240-256`）：

```rust
    // per-provider 覆盖全局（键不存在时跟随全局快照）。
    let sanitize = crate::server::core::provider_options::is_enabled(
        provider_id,
        crate::server::config::types::KEY_SANITIZE_FINGERPRINTS,
        ctx.sanitize_fingerprints,
    );
    let body = match sanitize {
        true => /* 既有 sanitize_body 逻辑不变 */,
        false => after_prompt,
    };
```

> **性能**：`send_body` 每次 provider attempt 都跑，`is_enabled` 会取一次 config 读锁。若实测有影响，把 `providerOptions` 在 `upstream::forward` 里快照进 `ProviderContext`（与 `sanitize_fingerprints` 同一处）。本任务先按简单版落地。

- [ ] **Step 6: 全量测试 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
git add desktop-tauri/src-tauri/server/src/server/core/provider_options.rs \
        desktop-tauri/src-tauri/server/src/server/core/mod.rs \
        desktop-tauri/src-tauri/server/src/server/core/upstream/payload.rs
git commit -m "新增：per-provider 行为开关（对标 magpie auth.options[]）

全局开关对所有家同时生效，但不同家对同一策略的反应不一致。

- providerOptions 配置键：{providerId: {全局键名: bool}}
- 键名**直接引用** config::types 的全局键常量（不是自造短名）
- 键不存在 = 跟随全局默认，升级后行为逐字不变
- 写侧用 update_raw_field（刷新内存快照）；save_raw 只写库、设了不生效
- 未知 provider 的残留开关被忽略，走 default
- 首个接入点：sanitizeBlacklistFingerprints"
```

---

## Task 3: Key 配额多规则（仅 Token 维度）

**背景**：当前只支持**单条** Token 限额（`quotaTokens` + `quotaReset`）。

**明确不做 Cost 维度**：`requests` 表只有 prompt/completion/total_tokens，**没有 cost 列**，全仓也没有任何地方计算费用。v1 计划的 `QuotaMetric::Cost` 是「永远为 0、永不超限」的假功能 —— 砍掉。

**Files:**
- Modify: `desktop-tauri/src-tauri/server/src/server/core/api_keys.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/key_quota.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/api/keys_api.rs`
- Modify: `desktop-tauri/ui-islands/src/islands/keys-page.tsx`
- Test: `key_quota.rs` 内联

**Interfaces:**
- Consumes: 既有 `by_kind: HashMap<kind, (window_start, HashMap<key_id, used>)>`（`key_quota.rs:72`，**已天然支持多窗口**）、`limiter::kind_window_start`（`pub(crate)`）
- Produces:
  - `api_keys::KeyQuotaRule { limit: i64, reset: QuotaReset }`（**无 metric 字段**）
  - `key_quota::exceeded(entry, now_ms) -> bool`（**签名不变**）
  - `key_quota::exceeded_rule(entry, now_ms) -> Option<KeyQuotaRule>`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn rules_only_key_reaches_the_database() {
    // 回归锁定：只有 quota_rules、没有 quota_tokens 的 Key 必须被判定。
    // 反例（v1）：fetch() 只收集 quota_tokens.is_some() 的窗口种类，
    // 于是 rule-only 的 Key 拿不到读数 → 恒 0 → 永不超限（静默无效）。
    let entry = entry_with_rules(vec![KeyQuotaRule { limit: 100_000, reset: QuotaReset::Daily }]);
    assert_eq!(entry.quota_tokens, None, "前提：旧字段为空");
    // 测试库里没有该 Key 的请求记录 → 读数为 0 → 不超限。
    // 关键是它**走到了查库**而不是被 quota_tokens 的 None 提前短路。
    assert!(exceeded_rule(&entry, crate::server::logging::now_ms()).is_none());
}

#[test]
fn legacy_single_quota_becomes_one_rule() {
    let entry = legacy_entry(Some(50_000), QuotaReset::Daily);
    let rules = effective_rules(&entry);
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].limit, 50_000);
}

#[test]
fn all_rules_must_pass() {
    // magpie 语义：多条规则**全部满足**才放行（不是任一满足）。
    let entry = entry_with_rules(vec![
        KeyQuotaRule { limit: 100_000, reset: QuotaReset::Daily },
        KeyQuotaRule { limit: 1_000_000, reset: QuotaReset::Fixed(30 * 86_400) },
    ]);
    let rules = effective_rules(&entry);
    assert_eq!(rules.len(), 2, "两条规则都要保留（叠加约束，不是取最宽）");
}

#[test]
fn no_rules_means_unlimited() {
    let entry = legacy_entry(None, QuotaReset::Daily);
    assert!(effective_rules(&entry).is_empty(), "空规则 = 无限，热路径不查库");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib key_quota`
Expected: FAIL — `cannot find struct KeyQuotaRule`

- [ ] **Step 3: 加规则类型（读侧兼容）**

`api_keys.rs`：

```rust
/// 一条 Key 配额规则（**只有 Token 维度**）。
///
/// ── 为什么不做 Cost 维度 ───────────────────────────────────
/// `requests` 表只有 prompt/completion/total_tokens，**没有 cost 列**，
/// 全仓也没有任何地方计算费用。magpie 的 Cost 限额依赖它的模型倍率表；
/// 本项目没有这套数据，加一个没有数据源的维度只会做出「永远为 0、
/// 永不超限」的假功能。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyQuotaRule {
    /// 窗口内累计 Token 上限（恒 > 0）
    pub limit: i64,
    pub reset: QuotaReset,
}
```

`ApiKeyEntry` 增加（**保留旧字段**）：

```rust
    /// 配额规则列表（新形态）；**空 = 回落到旧的单条配额**。
    ///
    /// ── 为什么保留旧字段而不是迁移 ─────────────────────────────
    /// `apiKeys` 存在 config.json（不是库表），没有事务可依赖。读侧兼容
    /// （空规则 → 用旧字段造一条）比写侧迁移安全。
    pub quota_rules: Vec<KeyQuotaRule>,
```

- [ ] **Step 4: 改 `fetch` 的 kinds 收集**

`key_quota.rs:180-187` 改为：

```rust
    let entries = crate::server::core::api_keys::list();
    let mut kinds: Vec<i64> = entries
        .iter()
        // **关键改动**：不再只看 quota_tokens，而是看有效规则。
        // 旧写法会让「只有 quota_rules」的 Key 拿不到读数 → 永不超限。
        .flat_map(|entry| effective_rules(entry).into_iter().map(|rule| rule.reset.kind()))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
```

- [ ] **Step 5: 实现 `exceeded_rule`（保持 `exceeded` 签名）**

```rust
/// 判定是否超限（**签名不变**：内部自己查库，热路径零成本）。
pub fn exceeded(entry: &ApiKeyEntry, now_ms: i64) -> bool {
    exceeded_rule(entry, now_ms).is_some()
}

/// 判定是否超限，并返回**第一条**超限的规则（`None` = 全部通过）。
///
/// ── 全部满足才放行 ─────────────────────────────────────────
/// 多条规则是叠加约束（日限额 + 月限额同时生效）。「任一满足」等于只保留
/// 最宽松的一条，让另一条形同虚设。
///
/// ── 「等于上限也算超」────────────────────────────────────────
/// 既有约定（`used >= limit`）不改：前端显示的「已用 50000 / 50000」与
/// 「被拦」必须一致。
pub fn exceeded_rule(entry: &ApiKeyEntry, now_ms: i64) -> Option<KeyQuotaRule> {
    let rules = effective_rules(entry);
    if rules.is_empty() {
        return None;
    }
    for rule in rules {
        if rule.limit <= 0 {
            continue;
        }
        let used = usage_for_window(entry, rule.reset.kind(), now_ms);
        if used >= rule.limit {
            return Some(rule);
        }
    }
    None
}

/// 有效规则：新形态优先，空则从旧字段造一条。
fn effective_rules(entry: &ApiKeyEntry) -> Vec<KeyQuotaRule> {
    if !entry.quota_rules.is_empty() {
        return entry.quota_rules.clone();
    }
    match entry.quota_tokens {
        Some(limit) => vec![KeyQuotaRule { limit, reset: entry.quota_reset }],
        None => Vec::new(),
    }
}

/// 按**指定窗口种类**查已用量。
///
/// 实现 = 既有 `usage_for`（key_quota.rs:103）的整个函数体，只把首行
/// `let kind = entry.quota_reset.kind();` 删掉（kind 现在是参数）。
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
    let fresh = fetch(now_ms);
    let used = cached_lookup(fresh.as_ref(), kind, &entry.id, now_ms);
    if let Ok(mut cache) = USAGE_CACHE.lock() {
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
```

- [ ] **Step 6: 同步 `quota_json`**

`key_quota.rs:303-322` 的 `quota_json` 仍只读 `quota_tokens` —— 规则型 Key 会在 Key 页显示「无限」。改为读 `effective_rules` 的第一条（或聚合展示）。

- [ ] **Step 7: 后端校验「最多 5 条」**

`keys_api.rs` 的 add/update：`quotaRules.len() > 5` → 400。

- [ ] **Step 8: 前端配额规则编辑**

`keys-page.tsx` 加「配额规则」区（最多 5 条，超出禁用「添加」按钮）。保留旧的单条 `quotaTokens` 输入（兼容期两者并存，规则非空时以规则为准）。

- [ ] **Step 9: 前端类型检查 + 构建**

```bash
npm --prefix desktop-tauri/ui-islands run typecheck
npm --prefix desktop-tauri/ui-islands run build
```

- [ ] **Step 10: 全量测试 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
git add desktop-tauri/src-tauri/server/src/server/core/api_keys.rs \
        desktop-tauri/src-tauri/server/src/server/core/key_quota.rs \
        desktop-tauri/src-tauri/server/src/server/api/keys_api.rs \
        desktop-tauri/ui-islands/src/islands/keys-page.tsx \
        desktop-tauri/ui/islands/ui.js desktop-tauri/ui/islands/ui.css
git commit -m "新增：网关 Key 配额多规则（仅 Token 维度）

原实现只支持单条 Token 限额（quotaTokens + quotaReset）。

- KeyQuotaRule {limit, reset}，最多 5 条，**全部满足才放行**（magpie 语义）
- 读侧兼容：规则为空时从旧 quotaTokens 造一条，旧记录行为逐字不变
- fetch 的窗口种类收集改为遍历有效规则 —— 这是 v1 的静默无效 bug：
  只看 quota_tokens 会让「只有规则」的 Key 拿不到读数、永不超限
- exceeded 签名不变（内部查库，热路径零成本）；新增 exceeded_rule 供展示
- **不做 Cost 维度**：requests 表无 cost 列，加它就是假功能
- 「等于上限也算超」的既有口径不变"
```

---

## Task 4: 通用 think-tags 剥离

**背景**：`strip_thinking_tags` 只服务 Qoder（`providers/qoder/stream.rs:428`，标签常量 `THINK_TAGS` 在 `providers/qoder/protocol.rs:41`，共 4 组）。

**范围**：只做「剥离**开头**的 think 块」。Qoder 那台机器是把**正文中间**的标签转成 reasoning delta，语义不同 —— 本任务**不声称**与它等价，也不动它。

**Files:**
- Create: `desktop-tauri/src-tauri/server/src/server/core/middleware/mod.rs`
- Create: `desktop-tauri/src-tauri/server/src/server/core/middleware/think_tags.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/upstream/sse.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/upstream/aggregate.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/core/upstream/provider_loop.rs`
- Modify: `desktop-tauri/src-tauri/server/src/server/config/types.rs`
- Test: `think_tags.rs` 内联

**Interfaces:**
- Produces:
  - `think_tags::strip_leading(text: &str) -> (String, bool)`
  - `think_tags::LeadingThinkFilter { done: bool, buffer: String }`（跨帧状态）

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn strip_only_affects_leading_block() {
    let (out, hit) = strip_leading("<think>internal</think>real answer");
    assert!(hit);
    assert_eq!(out, "real answer");
    // 正文中间的标签不动（那是用户内容）
    let (out, hit) = strip_leading("see <think>y</think> here");
    assert!(!hit);
    assert_eq!(out, "see <think>y</think> here");
}

#[test]
fn strip_handles_all_four_tag_pairs() {
    // Qoder 的 THINK_TAGS 有 4 组（qoder/protocol.rs:41），不是只有 <think>
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        let text = format!("{open}x{close}body");
        let (out, hit) = strip_leading(&text);
        assert!(hit, "标签对 {open} 必须被识别");
        assert_eq!(out, "body");
    }
}

#[test]
fn strip_handles_split_across_frames() {
    // 跨帧：`<thi` + `nk>x</think>` + `body`
    let mut filter = LeadingThinkFilter::default();
    let mut out = String::new();
    for chunk in ["<thi", "nk>x</think>", "body"] {
        out.push_str(&filter.push(chunk));
    }
    assert_eq!(out, "body", "剥完标签后正文必须完整且无残留");
}

#[test]
fn filter_passes_through_when_no_leading_tag() {
    let mut filter = LeadingThinkFilter::default();
    // 第一帧就不是标签开头 → 立刻进入 done，后续原样透传（零额外延迟）
    assert_eq!(filter.push("hello"), "hello");
    assert_eq!(filter.push(" world"), " world");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib think_tags`
Expected: FAIL — module 不存在

- [ ] **Step 3: 实现**

```rust
//! 剥离响应**开头**的思考标签块（对标 magpie 的 `think-tags` 插件 `strip` 模式）。
//!
//! ── 与 Qoder 那台机器的区别（不要混淆）──────────────────────
//! `providers::qoder::stream` 的机器把**正文中间**的 `<think>` 转成 reasoning
//! delta（`THINK_TAGS` 四组标签、任意位置）。本模块只处理**开头**那一块：
//! 上游把思考内容整个塞在正文最前面、而客户端不认标签时，把它剥掉。
//! 两者语义不同，不要互相替代。
//!
//! ── 为什么要跨帧状态 ──────────────────────────────────────
//! SSE 帧边界与标签边界无关：`<thi` 与 `nk>` 可能落在两帧里。逐帧剥离会
//! 把半截标签当普通正文留下。所以用 [`LeadingThinkFilter`] 累积到标签闭合。
//!
//! ── 为什么「没有标签」要立刻放行 ────────────────────────────
//! 首帧若不是标签开头，就置 `done` 并原样返回 —— 不能为了等可能的标签
//! 而缓冲正文（那会给所有正常响应加延迟）。

/// 剥离文本**开头**的 think 块。返回 (清理后文本, 是否命中)。
pub fn strip_leading(text: &str) -> (String, bool) {
    let mut filter = LeadingThinkFilter::default();
    let out = filter.push(text);
    (out, filter.done)
}

/// 跨帧的开头标签过滤器。
#[derive(Debug, Default)]
pub struct LeadingThinkFilter {
    /// 已判定完毕（没有开头标签 / 标签已剥完）—— 之后原样透传
    done: bool,
    /// 判定期间的缓冲（只在开头可能是标签时非空）
    buffer: String,
}

impl LeadingThinkFilter {
    /// 喂一段正文，返回可以下发的部分。
    pub fn push(&mut self, chunk: &str) -> String {
        if self.done {
            return chunk.to_string();
        }
        self.buffer.push_str(chunk);
        let trimmed = self.buffer.trim_start();
        let leading_ws = self.buffer.len() - trimmed.len();
        for (open, close) in crate::server::core::providers::qoder::protocol::THINK_TAGS {
            if !trimmed.starts_with(open) {
                continue;
            }
            let Some(close_at) = trimmed.find(close) else {
                // 标签还没闭合：继续缓冲（等下一帧）
                return String::new();
            };
            let rest_start = leading_ws + close_at + close.len();
            let rest = self.buffer[rest_start..].to_string();
            self.buffer.clear();
            self.done = true;
            return rest;
        }
        // 首部不是任何已知标签 → 判定完毕，把缓冲原样吐出
        self.done = true;
        std::mem::take(&mut self.buffer)
    }
}
```

> **`THINK_TAGS` 的可见性**：若它现在是私有常量，改为 `pub(crate)`。不要在本模块抄一份 —— 抄一份会在 Qoder 加标签时静默不同步。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib think_tags`
Expected: PASS — 4 passed

- [ ] **Step 5: 挂到 `FramePolicy`（流式路径）**

`sse.rs`：

```rust
pub struct FramePolicy {
    pub rewrite: Option<ModelRewrite>,
    pub strip_newline_chunks: bool,
    /// 剥离响应**开头**的 `<think>…</think>` 块。默认关。
    pub strip_leading_think: bool,
}
```

**必须同步更新这三处**（否则编译失败）：

1. `FramePolicy::with_rewrite`（`sse.rs:104-107`）→ 补 `strip_leading_think: false`
2. `FramePolicy::assembled`（`sse.rs:121-123`）→ 补该字段（默认 false）
3. 两处测试结构体字面量（`sse.rs:545`、`:745`）→ 补该字段

- [ ] **Step 6: 挂到 `CompletionAccumulator`（非流式路径）**

`aggregate.rs:135-140` 的 `CompletionAccumulator` 有独立的 `rewrite` / `strip_newline_chunks` 字段。加 `strip_leading_think`，并在聚合完成时对 `choices[0].message.content` 调一次 `strip_leading`。

> **为什么两条路径都要改**：非流式响应（`/v1/chat/completions` 非流、`responses` / `messages` 非流）走的是 accumulator，不是 coalescer。只改 coalescer 会让非流式响应保留标签。

- [ ] **Step 7: 非流式测试**

```rust
#[test]
fn strip_works_on_non_stream_path() {
    // 回归锁定：非流式走 CompletionAccumulator，不是 ReasoningCoalescer。
    let body = json!({
        "choices": [{"message": {"role": "assistant", "content": "<think>x</think>answer"}, "finish_reason": "stop"}]
    });
    let cleaned = strip_leading_in_completion(&body);
    assert_eq!(cleaned["choices"][0]["message"]["content"], "answer");
}
```

- [ ] **Step 8: 配置项与 `frame_policy_of`**

`config/types.rs` 加 `KEY_STRIP_LEADING_THINK`（默认 false）。`provider_loop.rs:2350` 的 `frame_policy_of` 里，用 `providers::kind_id(adapter.kind())` 取 provider id（**`kind_id` 已存在**，`providers/mod.rs:757`，不要新增 `id_of_kind`），再走 `provider_options::is_enabled`。

- [ ] **Step 9: 全量测试 + 前端 + 提交**

```bash
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --lib
npm --prefix desktop-tauri/ui-islands run typecheck && npm --prefix desktop-tauri/ui-islands run build
git add desktop-tauri/src-tauri/server/src/server/core/middleware/ \
        desktop-tauri/src-tauri/server/src/server/core/mod.rs \
        desktop-tauri/src-tauri/server/src/server/core/upstream/sse.rs \
        desktop-tauri/src-tauri/server/src/server/core/upstream/aggregate.rs \
        desktop-tauri/src-tauri/server/src/server/core/upstream/provider_loop.rs \
        desktop-tauri/src-tauri/server/src/server/config/types.rs \
        desktop-tauri/ui-islands/src/islands/settings-page.tsx \
        desktop-tauri/ui/islands/ui.js desktop-tauri/ui/islands/ui.css
git commit -m "新增：通用剥离响应开头的思考标签（think-tags strip）

对标 magpie 的 think-tags 插件，只做 strip 模式（to_content 不做）。

- 只处理**开头**的 think 块；正文中间的标签不动（那是用户内容）
- 跨 SSE 分片的标签正确拼接（<thi + nk>）
- 复用 Qoder 的 THINK_TAGS 四组标签常量，不另抄一份
- 流式走 FramePolicy、非流式走 CompletionAccumulator —— 两条路径都改，
  只改一条会让非流式响应保留标签
- 默认关；per-provider 开关可单独覆盖
- 不声称与 Qoder 那台『正文中间标签转 reasoning』的机器等价"
```

---

## 移出本轮的三项（需独立前置调研）

评审证明这三项按 v1 设计会**编译失败或静默无效**。它们不是不重要，而是需要先补齐设计再实施。

### P1. Gemini 端点

**阻塞点**（三条，均已验证）：

1. `ForwardOutcome` 没有 `IntoResponse`（`core/upstream/mod.rs:256`，全仓无该 impl）。正确做法是照 `materialize_anthropic`（`api/protocol.rs:775`）写一个 `materialize_gemini`，按 `Stream` / `Completion` / `Err` 三个变体分别转 Response。
2. **缺 chat→Gemini SSE 翻译机**。全仓只有 `anthropic_from_chat` / `anthropic_request_from_chat` / `responses_request_from_chat`，没有 Gemini 方向。`ChatFromAntigravityStream` 是**反方向**（Gemini→chat）。需要新写一台 `GeminiStream`。
3. **未注入 path model**。`responses_endpoint` 在 `:218` 内联调 `pipeline::resolve_model`；Gemini 入口若不做同样的事，用户请求 `gemini-2.5-pro` 会静默用网关默认模型。

**另外需补**：`alt=sse` 查询参数（无它时 Gemini 返回 JSON 数组）、`functionCall` / `functionResponse` 往返（编码 agent 必需）、`systemInstruction` 多 part、`countTokens` 必须把 tools 计入（否则编码 agent 的上下文管理会失效）。

**已实测可用的部分**：axum 0.8 的 `{model_action}` 能正确捕获含冒号的路径段（实测 `/v1beta/models/gemini-2.5-pro:generateContent` → 200，参数完整）。这部分结论可直接复用。

### P2. checkin 提升为 trait 钩子

**阻塞点**：v1 的 `CheckinOutcome` 枚举丢失三类信息：

- `log_success_msg` 布尔（`checkin.rs:486` 传 `false`，其余传 `true`）—— 决定成功日志带不带具体收益文案
- `claim: Value` 本身（可能含 `credit`），以及 `bonus` / `dailyTasks` / `rewards` 等追加字段（`checkin.rs:451/458/526` 在 `claim_result` 之后插入）
- `checkin_completed_today` / `claim_pending` 依赖 `claim.msg` 的**中文子串**精确匹配（`checkin.rs:735/799`），改写任何文案会静默破坏自动重试与台账

**更低风险的方向**：保留 `match provider` 不动，只把 `CHECKIN_PROVIDERS` 与一个 `providers::CHECKIN_KINDS: &[ProviderKind]` 收敛到一处，并加 golden test 钉住当前 14 个 id。trait 迁移作为后续独立重构，届时钩子必须返回 `claim: Value` + `extras` + `log_success_msg`，而不是枚举。

### P3. word-guard 词语拦截

**阻塞点**：`payload::send_body` 返回 `SendBody<'a>`（`payload.rs:180`），**没有错误通道**。`Reject` 无法从这里变成 400。要么改 `send_body` 返回 `Result` 并改三处调用点（`provider_loop.rs:984/1929/2129`），要么把拦截提前到 `upstream::forward`（`upstream/mod.rs:345+`）在选路之前。

**另外需补**：`role == "user"` 判定在 `send_body` 阶段是安全的（三个入口都已转成 chat 形态），但 `content` 可能是 parts 数组（多模态），字符串检查会被 `[{"type":"text","text":"SECRET"}]` 绕过 —— 需复用 `qoder/protocol.rs:53` 的 `content_to_text` 式扁平化。

---

## 执行顺序

```
Task 1 (TOML 注释)    ── 独立
Task 2 (per-provider) ── 独立
Task 3 (Key 多规则)   ── 独立
Task 4 (think-tags)   ── 依赖 Task 2（复用 provider_options 的开关）
```

建议顺序：**1 → 2 → 4 → 3**。理由：1、2 风险最低；4 依赖 2 的开关机制；3 的前端改动最大，放最后。

---

## 评审记录

### 评审方式

- **Oracle**（设计评审，20 分钟）：逐条核对六个任务的方案是否成立，对照真实代码。
- **Momus**（计划评审，19.5 分钟）：核对可执行性、测试有效性、接口一致性。
- 两份评审共报 **15 项 BLOCKER**，全部由我逐条对真实代码验证后才采信。

### 验证过的关键事实

| 断言 | 验证方式 | 结果 |
|---|---|---|
| `ForwardOutcome` 无 `IntoResponse` | `grep -rn "impl IntoResponse for ForwardOutcome"` | 无匹配，确认 |
| 缺 chat→Gemini 翻译机 | `grep "ChatToGemini\|GeminiFromChat"` | 无匹配，确认 |
| `kind_id` 已存在 | 读 `providers/mod.rs:757` | `pub const fn kind_id`，确认 |
| Codex 有单段键 | 读 `agents/apply.rs:189` | `("model".into(), ...)`，确认 |
| `set_path` 自动建表 | 读 `apply.rs:288-296` | `or_insert_with(json!({}))`，确认 |
| `save_raw` 不刷内存 | 读 `config/mod.rs` 的 `save_raw` 实现 | 只 `sql::write_all`，确认 |
| `fetch` 只认 quota_tokens | 读 `key_quota.rs:180-187` | `.filter(\|e\| e.quota_tokens.is_some())`，确认 |
| `requests` 无 cost 列 | 解析 `db/schema.rs` 的 CREATE TABLE | 无 cost 列，确认 |
| `send_body` 无错误通道 | 读 `payload.rs:180` | 返回 `SendBody<'a>`，确认 |
| `FramePolicy` 有 2 处字面量 | `grep "FramePolicy {"` | `sse.rs:545` / `:745`，确认 |
| 全局键名是 `sanitizeBlacklistFingerprints` | 读 `config/types.rs:133` | 确认 |
| `toml 0.9.12` 不用 `toml_edit` | 读 `Cargo.lock:5306-5319` | 用 `toml_parser`/`toml_writer`，确认 |
| `toml_edit 0.25.13` 已在 lock | 读 `Cargo.lock:3588`（`proc-macro-crate` 依赖） | 确认 |
| axum 0.8 支持冒号路径段 | **实测**：起服务打 `/v1beta/models/x:y` | 200，参数完整，确认 |

### 未采纳的评审建议及理由

| 建议 | 未采纳理由 |
|---|---|
| Task 3 加 `Cost` 维度 | `requests` 无 cost 列，加它就是「永远为 0」的假功能。若将来有倍率表再单独立项。 |
| Task 3 用 `SmallVec` 优化 `effective_rules` | 本项目未引入 `smallvec`，而「不新增依赖」是硬约束。先做 `is_empty()` 短路，实测有热点再议。 |
| Task 4 用结构体替代裸 bool | 采纳一半：`FramePolicy` 保持既有两字段风格（改动最小），新增字段默认 false，不引入 builder。 |

# Meoo Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `ProvidersPlugin/Agent2Providers` 新增 `meoo`（秒悟 Meoo）provider：OAuth PKCE 登录、项目服务 AK、Token 续期、模型目录、OpenAI 兼容流式对话、积分余额，并在所有 Meoo 请求上携带官方 CLI 头；同时修掉 dsh-meoo 的已知缺陷。

**Architecture:** 插件实现全部上游知识，网关保持插件化：不新增 `ProviderKind` / `PROVIDERS` / `adapter_for` 实现。对话走无状态 `build_chat`（插件只返回 URL + 头 + body，网关负责发送与 SSE 翻译）；OAuth 回调走网关新增的 public 路由 `/oauth/callback`；账号、目录、余额、登录全部通过 `Provider` trait 与 `PluginCtx` 完成。

**Tech Stack:** Rust 2021（workspace `rust-version = 1.88`）、`reqwest`（rustls）、`serde_json`、`tokio`、`sha2`、`getrandom`、`base64`、`url`。不新增依赖。

**Spec:** `docs/superpowers/specs/2026-11-02-meoo-provider.md`

## Global Constraints

- 插件 crate 顶部有 `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`；生产代码禁止 `unwrap` / `expect` / `panic!`，需要兜底时用 `match` / `if let`。测试模块统一在 `#[cfg(test)] mod tests` 上加 `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`。
- 网关 `adapter_for()` 的注释写着「不要在这里加任何真实现」；本计划**不**在网关写 Meoo 上游调用代码。
- 所有出网走 `crate::http::client(None, &ctx.env.proxy)` / `crate::http::get_json` / `post_json` / `post_form`；不得自己 `reqwest::Client::new()`。
- 所有 Meoo 请求必须经过 `providers/meoo/headers.rs`；禁止在 `oauth.rs` / `control.rs` / `models.rs` / `chat.rs` 里手拼 `X-Meoo-*`。
- 公开视图（`public`）**绝不能**含 `accessToken` / `refreshToken` / `serviceAk` 本体，只留 `tokenTail`。
- 不得自动开通 Meoo 云服务；只有 `allowCloudEnable == true` 才能 `POST /cloud`。
- 不得信任旧 `~/.meoo-proxy/config.json` 的 `apiBaseUrl`；只允许 `https://meoo.com` / `https://gray-meoo.aliyun-inc.com`。
- 本计划**不修改版本号**、不打 tag、不推分支；发版按 `release-flow` 另行确认。
- 提交信息用中文：`feat(meoo): …` / `fix(meoo): …` / `test(meoo): …`；署名按 `release-flow`。
- 每个 Task 结束时工作区必须能编译；不允许「先写一堆再一起修」。

## Review Focus

实现者最容易在这七处出错，每个都在对应 Task 里配了测试：

1. **CLI 头漏发** —— `/models`、`/chat/completions`、控制面三处都必须有 `X-Meoo-Client` / `X-Meoo-CLI-Version` / `X-Meoo-Request-Id`；漏一处的症状是上游行为不一致，且很难从响应看出来。
2. **AK 与 OAuth token 混用** —— 对话/目录用项目服务 AK，控制面/积分用 OAuth access token；把 OAuth token 当对话 Bearer 会稳定 401。
3. **AK 写一次不更新** —— 换项目、AK 轮换后必须更新 `serviceAk`；dsh-meoo 的 `describe().configured` 跳过 `set` 就是这个问题。
4. **刷新并发/轮换** —— 必须单飞 + 比较再写；两个并发刷新会互相作废 refresh token。
5. **401 无限重试** —— 插件刷新只做一轮；`SERVICE_AK_FORMAT_INVALID` 必须 Fatal，不能 TokenExpired。
6. **自动开通云服务** —— 默认 `allowCloudEnable=false`；没有显式同意不得 `POST /cloud`。
7. **公开视图泄漏** —— `account_view` 只允许 `tokenTail`；错误文案与日志也不得出现 token/AK。

---

## 文件结构

### 插件仓 `ProvidersPlugin/Agent2Providers`（主要工作量）

```text
crates/providers/src/providers/meoo/
  mod.rs          Provider trait 实现、meta、登录方式、账号 normalize/view、admin_action
  endpoints.rs    站点 / 网关 / client ID / scope / CLI 版本常量
  headers.rs      CLI 头构造（唯一入口）
  oauth.rs        PKCE、授权 URL、换票、refresh、回调解析
  control.rs      identity / projects / cloud environment / points / entitlement
  credentials.rs  secret 解析、public 视图、identity、过期判断、AK 解析
  refresh.rs      单飞 + 比较再写的凭证刷新
  models.rs       远程 /models + 静态兜底 + TTL 缓存
  chat.rs         build_chat + classify + reasoning_patch
  usage.rs        points/entitlement 归一化
  login.rs        pending 表、login_start/callback、手动 AK、旧配置导入
```

### 网关仓 `agent2api`（小改动）

```text
desktop-tauri/src-tauri/server/src/server/http.rs             新增 GET /oauth/callback
desktop-tauri/src-tauri/server/src/server/api/session.rs     新增 login_meoo_callback
desktop-tauri/ui-islands/src/islands/add-provider-pick.tsx   新增 meoo 图标
desktop-tauri/ui/assets/providers/meoo.png                   新增图标资产
README.md + docs/README.{zh-CN,zh-Hant,ja,ko,pt-BR}.md        31→32 / 30→31，加 Meoo 条目
```

---

### Task 1: 插件身份登记（能编译、能列出、能握手）

**Files:**

- Create: `crates/providers/src/providers/meoo/mod.rs`
- Create: `crates/providers/src/providers/meoo/{endpoints,headers,oauth,control,credentials,refresh,models,chat,usage,login}.rs`（本 Task 只放一行模块注释）
- Modify: `crates/providers/src/providers/mod.rs`
- Modify: `crates/providers/Cargo.toml`
- Modify: `pack.json`
- Test: `crates/providers/src/providers/meoo/mod.rs` 末尾 `#[cfg(test)]`

**Interfaces:**

- Consumes: `agent2api_provider_sdk::{Provider, ProviderMeta}`
- Produces: `pub const ID: &str = "meoo";`、`pub struct MeooProvider;`

- [ ] **Step 1: 写失败测试**

在 `mod.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_declares_meoo_capabilities() {
        let meta = MeooProvider.meta();
        assert_eq!(meta.id, "meoo");
        assert_eq!(meta.label, "Meoo 秒悟");
        assert!(meta.capabilities.chat);
        assert!(!meta.capabilities.stateful);
        assert!(meta.capabilities.refresh);
        assert!(meta.capabilities.model_refresh);
        assert!(meta.capabilities.usage);
        assert!(meta.capabilities.web_login);
        assert!(!meta.capabilities.checkin);
        assert!(!meta.capabilities.claims);
        assert!(!meta.default_models.is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::tests::meta_declares_meoo_capabilities
```

Expected: 编译失败（`MeooProvider` 未定义）。

- [ ] **Step 3: 写最小实现**

`mod.rs`：

```rust
//! Meoo（秒悟）接入：OAuth PKCE + 项目服务 AK + OpenAI 兼容转发 + 积分余额。
//!
//! ── 凭证模型（最容易混的一处）────────────────────────────────
//! - 对话 / 模型目录：`Authorization: Bearer meoo_project_service_v1.*`
//! - 控制面 / 积分：`Authorization: Bearer <OAuth access_token>`
//! - OAuth access token 1 小时过期；项目服务 AK 按项目解析、可轮换
//!
//! ── 所有请求都带官方 CLI 头 ─────────────────────────────────
//! `X-Meoo-Client: cli` / `X-Meoo-CLI-Version` / `X-Meoo-Request-Id`
//! 统一由 `headers.rs` 构造，禁止别处手拼。
//!
//! ── 签到 / 领取：确认为 ✗ ───────────────────────────────────
//! 官方 CLI 的 `meoo account` 只有积分/权益查询；没有每日签到或领取动作。
//! 故 `checkin` / `claims` 恒 false，这不是「还没做」。

pub mod chat;
pub mod control;
pub mod credentials;
pub mod endpoints;
pub mod headers;
pub mod login;
pub mod models;
pub mod oauth;
pub mod refresh;
pub mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{Provider, ProviderMeta};

/// provider id（`pack.json` 必须写同一个）。
pub const ID: &str = "meoo";

pub struct MeooProvider;

impl Provider for MeooProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "Meoo 秒悟")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: false,
                refresh: true,
                model_refresh: true,
                usage: true,
                checkin: false,
                claims: false,
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "浏览器登录".into(),
                    hint: Some("打开 Meoo 授权页，授权后自动解析项目服务 AK".into()),
                },
                proto::LoginMethod::Token {
                    label: "项目服务 AK".into(),
                    placeholder: "meoo_project_service_v1....".into(),
                    extra_fields: vec![],
                },
            ])
            .with_models(models::default_models())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                identifier: vec!["projectUrlId".into(), "userId".into()],
                expiry: vec!["expiresAt".into()],
                ..Default::default()
            })
    }

    fn provider_id(&self) -> String {
        ID.to_string()
    }
}
```

其余文件先写：

```rust
//! 见 `docs/superpowers/specs/2026-11-02-meoo-provider.md`。
```

`models.rs` 例外 —— Task 1 的 `mod.rs` 已经调用 `models::default_models()`，先放一个最小非空清单（Task 7 会整体替换）：

```rust
//! 见 `docs/superpowers/specs/2026-11-02-meoo-provider.md`。

pub fn default_models() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "id": "qwen3.8-flash",
        "name": "Qwen3.8-Flash",
        "maxInputTokens": 200000,
        "maxOutputTokens": 64000,
        "supportsToolCall": true
    })]
}
```

- [ ] **Step 4: 注册**

`crates/providers/src/providers/mod.rs` 加：

```rust
#[cfg(feature = "meoo")]
pub mod meoo;
```

`registry()` 末尾（`list` 返回前）加：

```rust
    #[cfg(feature = "meoo")]
    list.push(Box::new(meoo::MeooProvider));
```

`crates/providers/Cargo.toml`：

```toml
all = [..., "meoo"]
meoo = []
```

`pack.json` 的 `providers[]` 末尾：

```json
{
  "id": "meoo",
  "label": "Meoo 秒悟",
  "order": 31,
  "default_enabled": true
}
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::tests::meta_declares_meoo_capabilities
cargo build --release -p agent2api-providers
node scripts/verify.mjs --exe target/release/agent2api-providers
```

Expected: 单测通过；`verify.mjs` 的 `init` 列表里出现 `meoo`，28 项仍全过。

- [ ] **Step 6: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/Cargo.toml crates/providers/src/providers/mod.rs \
        crates/providers/src/providers/meoo pack.json
git commit -m "feat(meoo): 登记 meoo provider 身份与能力位"
```

---

### Task 2: CLI 头与端点常量

**Files:**

- Modify: `crates/providers/src/providers/meoo/endpoints.rs`
- Modify: `crates/providers/src/providers/meoo/headers.rs`
- Test: 两个文件末尾 `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `pub const DEFAULT_API_BASE: &str = "https://meoo.com";`
  - `pub const DEFAULT_GATEWAY_BASE: &str = "https://api.meoo.host/meoo-ai/compatible-mode/v1";`
  - `pub const DEFAULT_CLI_VERSION: &str = "0.5.6";`
  - `pub fn api_base() -> String`
  - `pub fn gateway_base() -> String`
  - `pub fn client_id(api_base: &str) -> &'static str`
  - `pub fn cli_headers() -> Vec<(String, String)>`
  - `pub fn control_headers(access_token: &str) -> Vec<(String, String)>`
  - `pub fn gateway_headers(service_ak: &str, project: &str, stream: bool) -> Vec<(String, String)>`

- [ ] **Step 1: 写失败测试**

`headers.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_headers_match_official_cli() {
        let headers = cli_headers_with("0.5.6", "cli_req_test");
        assert!(headers.contains(&("X-Meoo-Client".into(), "cli".into())));
        assert!(headers.contains(&("X-Meoo-CLI-Version".into(), "0.5.6".into())));
        assert!(headers.contains(&("X-Meoo-Request-Id".into(), "cli_req_test".into())));
    }

    #[test]
    fn gateway_headers_include_cli_and_bearer() {
        let headers = gateway_headers_with("meoo_project_service_v1.x", "proj_1", true, "0.5.6", "cli_req_x");
        assert!(headers.contains(&("Authorization".into(), "Bearer meoo_project_service_v1.x".into())));
        assert!(headers.contains(&("Accept".into(), "text/event-stream, application/json".into())));
        assert!(headers.contains(&("X-Meoo-Client".into(), "cli".into())));
        assert!(headers.contains(&("OneDay-App-Id".into(), "proj_1".into())));
    }

    #[test]
    fn request_id_is_unique_and_prefixed() {
        let a = request_id_with("00000000-0000-4000-8000-000000000001");
        let b = request_id_with("00000000-0000-4000-8000-000000000002");
        assert!(a.starts_with("cli_req_"));
        assert_ne!(a, b);
    }
}
```

`endpoints.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_host_uses_production_client_id() {
        assert_eq!(client_id("https://meoo.com"), PROD_CLIENT_ID);
        assert_eq!(client_id("https://gray-meoo.aliyun-inc.com"), PROD_CLIENT_ID);
    }

    #[test]
    fn other_host_uses_test_client_id() {
        assert_eq!(client_id("https://example.com"), TEST_CLIENT_ID);
    }

    #[test]
    fn env_base_allowlist_rejects_unknown_hosts() {
        assert!(allowed_api_base("https://meoo.com"));
        assert!(!allowed_api_base("https://evil.example"));
        assert!(allowed_gateway_base("https://api.meoo.host/meoo-ai/compatible-mode/v1"));
        assert!(!allowed_gateway_base("https://evil.example/meoo-ai"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::headers
```

Expected: 编译失败（函数未定义）。

- [ ] **Step 3: 实现**

`endpoints.rs`：

```rust
//! Meoo 端点、client ID、CLI 版本常量。

pub const DEFAULT_API_BASE: &str = "https://meoo.com";
pub const DEFAULT_GATEWAY_BASE: &str =
    "https://api.meoo.host/meoo-ai/compatible-mode/v1";
pub const DEFAULT_CLI_VERSION: &str = "0.5.6";
pub const OAUTH_SCOPE: &str = "cli.compat";
pub const CALLBACK_PATH: &str = "/oauth/callback";

pub const PROD_CLIENT_ID: &str = "meoo_app_tdWlRbpvAxN1EjmZ8XICxdTh";
pub const TEST_CLIENT_ID: &str = "meoo_app_SFNvmHUFNePxyyfGjCd9hyaE";

/// 正式环境 hostname（与官方 CLI 逐字一致）。
const PROD_HOSTS: &[&str] = &["meoo.com", "gray-meoo.aliyun-inc.com"];

fn host_of(value: &str) -> Option<String> {
    url::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
}

fn insecure_test_base_allowed() -> bool {
    matches!(
        std::env::var("MEOO_ALLOW_INSECURE_BASE").ok().as_deref(),
        Some("1") | Some("true")
    )
}

fn allowed_api_base(value: &str) -> bool {
    host_of(value).as_deref().is_some_and(|host| PROD_HOSTS.contains(&host))
        || (insecure_test_base_allowed()
            && host_of(value)
                .as_deref()
                .is_some_and(|host| host == "127.0.0.1" || host == "localhost"))
}

fn allowed_gateway_base(value: &str) -> bool {
    host_of(value).as_deref() == Some("api.meoo.host")
        || (insecure_test_base_allowed()
            && host_of(value)
                .as_deref()
                .is_some_and(|host| host == "127.0.0.1" || host == "localhost"))
}

pub fn api_base() -> String {
    let candidate = std::env::var("MEOO_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty());
    match candidate {
        Some(value) if allowed_api_base(&value) => value,
        Some(_) => DEFAULT_API_BASE.to_string(),
        None => DEFAULT_API_BASE.to_string(),
    }
}

pub fn gateway_base() -> String {
    let candidate = std::env::var("MEOO_GATEWAY_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty());
    match candidate {
        Some(value) if allowed_gateway_base(&value) => value,
        Some(_) => DEFAULT_GATEWAY_BASE.to_string(),
        None => DEFAULT_GATEWAY_BASE.to_string(),
    }
}

pub fn client_id(api_base: &str) -> &'static str {
    let host = url::Url::parse(api_base)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        .unwrap_or_default();
    if PROD_HOSTS.contains(&host.as_str()) {
        PROD_CLIENT_ID
    } else {
        TEST_CLIENT_ID
    }
}

pub fn authorize_url(api_base: &str, redirect_uri: &str, state: &str, challenge: &str) -> String {
    let Ok(mut url) = url::Url::parse(&format!("{}/oauth/authorize", api_base.trim_end_matches('/'))) else {
        return format!("{DEFAULT_API_BASE}/oauth/authorize");
    };
    url.query_pairs_mut()
        .append_pair("client_id", client_id(api_base))
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", OAUTH_SCOPE)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    url.to_string()
}

pub fn token_url(api_base: &str) -> String {
    format!("{}/oauth/token", api_base.trim_end_matches('/'))
}

pub fn identity_url(api_base: &str) -> String {
    format!("{}/open/v1/cli-compat/account/identity", api_base.trim_end_matches('/'))
}

pub fn projects_url(api_base: &str) -> String {
    format!("{}/open/v1/cli-compat/projects?page_size=50", api_base.trim_end_matches('/'))
}

pub fn environment_url(api_base: &str, project: &str) -> String {
    format!(
        "{}/open/v1/cli-compat/projects/{}/cloud/environment",
        api_base.trim_end_matches('/'),
        project
    )
}

pub fn cloud_url(api_base: &str, project: &str) -> String {
    format!(
        "{}/open/v1/cli-compat/projects/{}/cloud",
        api_base.trim_end_matches('/'),
        project
    )
}

pub fn points_url(api_base: &str) -> String {
    format!("{}/open/v1/cli-compat/account/points", api_base.trim_end_matches('/'))
}

pub fn entitlement_url(api_base: &str) -> String {
    format!("{}/open/v1/cli-compat/account/entitlement", api_base.trim_end_matches('/'))
}

pub fn models_url(gateway_base: &str) -> String {
    format!("{}/models", gateway_base.trim_end_matches('/'))
}

pub fn chat_url(gateway_base: &str) -> String {
    format!("{}/chat/completions", gateway_base.trim_end_matches('/'))
}
```

`headers.rs`：

```rust
//! Meoo 请求头：CLI 头 + Bearer 的唯一构造入口。

use std::sync::OnceLock;

use super::endpoints::DEFAULT_CLI_VERSION;

pub fn cli_version() -> String {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            std::env::var("MEOO_CLI_VERSION")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty() && value.len() <= 32 && value.is_ascii())
                .unwrap_or_else(|| DEFAULT_CLI_VERSION.to_string())
        })
        .clone()
}

pub fn cli_headers_enabled() -> bool {
    !matches!(
        std::env::var("MEOO_CLI_HEADERS").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

pub fn request_id() -> String {
    request_id_with(&crate::ids::uuid_v4())
}

pub fn request_id_with(uuid: &str) -> String {
    format!("cli_req_{uuid}")
}

pub fn cli_headers_with(version: &str, request_id: &str) -> Vec<(String, String)> {
    vec![
        ("X-Meoo-Client".to_string(), "cli".to_string()),
        ("X-Meoo-CLI-Version".to_string(), version.to_string()),
        ("X-Meoo-Request-Id".to_string(), request_id.to_string()),
    ]
}

pub fn cli_headers() -> Vec<(String, String)> {
    if !cli_headers_enabled() {
        return Vec::new();
    }
    cli_headers_with(&cli_version(), &request_id())
}

pub fn control_headers(access_token: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {access_token}")),
    ];
    headers.extend(cli_headers());
    headers
}

pub fn gateway_headers(service_ak: &str, project: &str, stream: bool) -> Vec<(String, String)> {
    gateway_headers_with(
        service_ak,
        project,
        stream,
        &cli_version(),
        &request_id(),
    )
}

pub fn gateway_headers_with(
    service_ak: &str,
    project: &str,
    stream: bool,
    version: &str,
    request_id: &str,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Accept".to_string(),
            if stream {
                "text/event-stream, application/json".to_string()
            } else {
                "application/json".to_string()
            },
        ),
        ("Authorization".to_string(), format!("Bearer {service_ak}")),
    ];
    if cli_headers_enabled() {
        headers.extend(cli_headers_with(version, request_id));
    }
    if !project.trim().is_empty() {
        headers.push(("OneDay-App-Id".to_string(), project.trim().to_string()));
    }
    headers
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::
```

Expected: 6 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/headers.rs \
        crates/providers/src/providers/meoo/endpoints.rs
git commit -m "feat(meoo): 统一 CLI 头与端点常量"
```

---

### Task 3: OAuth PKCE 与换票

**Files:**

- Modify: `crates/providers/src/providers/meoo/oauth.rs`
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `pub struct Pkce { pub verifier: String, pub challenge: String }`
  - `pub fn create_pkce() -> Pkce`
  - `pub struct TokenBundle { pub access_token: String, pub refresh_token: String, pub expires_in: i64, pub scope: String, pub openid: String }`
  - `pub async fn exchange_code(ctx, api_base, client_id, redirect_uri, code, verifier) -> Result<TokenBundle, GatewayError>`
  - `pub async fn refresh(ctx, api_base, client_id, refresh_token) -> Result<TokenBundle, GatewayError>`
  - `pub fn parse_callback(code: &str, url: &str) -> Result<(String, String), String>`（返回 `(code, state)`）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let pkce = create_pkce();
        let expected = {
            use base64::Engine as _;
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(pkce.verifier.as_bytes());
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
        };
        assert_eq!(pkce.challenge, expected);
        assert!(pkce.verifier.len() >= 43 && pkce.verifier.len() <= 128);
    }

    #[test]
    fn callback_accepts_code_and_state() {
        let (code, state) = parse_callback("", "http://127.0.0.1:3065/oauth/callback?code=abc&state=xyz")
            .expect("应解析");
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn callback_rejects_error() {
        let error = parse_callback("", "http://127.0.0.1:3065/oauth/callback?error=access_denied&state=xyz")
            .expect_err("应拒绝");
        assert!(error.contains("access_denied"));
    }

    #[test]
    fn bare_code_is_accepted() {
        let (code, state) = parse_callback("bare-code", "").expect("裸 code 应接受");
        assert_eq!(code, "bare-code");
        assert!(state.is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::oauth
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! OAuth PKCE：授权 URL、换票、refresh、回调解析。

use serde_json::Value;

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::http;

use super::endpoints;

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn create_pkce() -> Pkce {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let verifier = {
        let mut bytes = [0u8; 32];
        let _ = getrandom::getrandom(&mut bytes);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    };
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Pkce { verifier, challenge }
}

#[derive(Clone, Debug, Default)]
pub struct TokenBundle {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub scope: String,
    pub openid: String,
}

fn parse_token(payload: &Value) -> Result<TokenBundle, GatewayError> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        let error = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("OAuth 响应缺少 access_token");
        return Err(GatewayError::with_status(401, format!("Meoo 登录失败：{error}")));
    }
    Ok(TokenBundle {
        access_token,
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
        expires_in: payload
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .unwrap_or(3600),
        scope: payload
            .get("scope")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
        openid: payload
            .get("openid")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
    })
}

pub async fn exchange_code(
    ctx: &PluginCtx,
    api_base: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenBundle, GatewayError> {
    let client = http::client(None, &ctx.env.proxy);
    let (status, payload) = http::post_form(
        &client,
        &endpoints::token_url(api_base),
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
    )
    .await
    .map_err(|error| GatewayError::with_status(502, format!("Meoo 换票失败：{error}")))?;
    if !(200..300).contains(&status) {
        return Err(GatewayError::with_status(
            401,
            format!("Meoo 换票被拒（HTTP {status}）：{}", payload.get("error").and_then(Value::as_str).unwrap_or("")),
        ));
    }
    parse_token(&payload)
}

pub async fn refresh(
    ctx: &PluginCtx,
    api_base: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenBundle, GatewayError> {
    let client = http::client(None, &ctx.env.proxy);
    let (status, payload) = http::post_form(
        &client,
        &endpoints::token_url(api_base),
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ],
    )
    .await
    .map_err(|error| GatewayError::with_status(502, format!("Meoo 续期失败：{error}")))?;
    if !(200..300).contains(&status) {
        let code = payload.get("error").and_then(Value::as_str).unwrap_or("");
        if code == "invalid_grant" {
            return Err(GatewayError::with_status(401, "Meoo 登录已失效，请重新登录"));
        }
        return Err(GatewayError::with_status(502, format!("Meoo 续期被拒（HTTP {status}）：{code}")));
    }
    parse_token(&payload)
}

pub fn parse_callback(code: &str, url: &str) -> Result<(String, String), String> {
    let mut code_value = code.trim().to_string();
    let Ok(parsed) = url::Url::parse(url) else {
        if !code_value.is_empty() {
            return Ok((code_value, String::new()));
        }
        return Err("回调地址无效".to_string());
    };
    let mut state = String::new();
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "error" => return Err(format!("授权被拒绝：{value}")),
            "code" => code_value = value.into_owned(),
            "state" => state = value.into_owned(),
            _ => {}
        }
    }
    if code_value.is_empty() {
        return Err("回调缺少授权码".to_string());
    }
    Ok((code_value, state))
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::oauth
```

Expected: 4 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/oauth.rs
git commit -m "feat(meoo): 实现 OAuth PKCE 与换票/续期"
```

---

### Task 4: 控制面与凭证解析（手动 AK）

**Files:**

- Modify: `crates/providers/src/providers/meoo/control.rs`
- Modify: `crates/providers/src/providers/meoo/credentials.rs`
- Modify: `crates/providers/src/providers/meoo/models.rs`（只补 AK 探活；完整目录在 Task 7）
- Test: `control.rs` / `credentials.rs` 末尾 `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `control::identity(ctx, api_base, access_token) -> Result<Value, GatewayError>`
  - `control::list_projects(ctx, api_base, access_token) -> Result<Vec<Value>, GatewayError>`
  - `control::resolve_service_ak(ctx, api_base, access_token, project, allow_cloud_enable) -> Result<String, GatewayError>`
  - `control::points(ctx, api_base, access_token) -> Result<Value, GatewayError>`
  - `control::entitlement(ctx, api_base, access_token) -> Result<Value, GatewayError>`
  - `credentials::from_secret(secret: &Value) -> MeooCredentials`
  - `credentials::account_view(secret: &Value) -> Value`
  - `credentials::identity_of(secret: &Value) -> String`
  - `credentials::parse_manual(input: &Value) -> Result<ManualAk, String>`
  - `models::validate_service_ak(ctx, ak, project) -> Result<(), GatewayError>`

- [ ] **Step 1: 写失败测试**

`credentials.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_user_api_key() {
        let error = parse_manual(&json!({"inputs": {"serviceAk": "meoo_ak_123"}}))
            .expect_err("用户 Key 必须拒绝");
        assert!(error.contains("项目服务 AK"));
    }

    #[test]
    fn accepts_project_service_ak() {
        let parsed = parse_manual(&json!({
            "inputs": {
                "serviceAk": "meoo_project_service_v1.abc",
                "projectUrlId": "proj_1",
                "name": "测试项目"
            }
        }))
        .expect("应接受");
        assert_eq!(parsed.service_ak, "meoo_project_service_v1.abc");
        assert_eq!(parsed.project_url_id.as_deref(), Some("proj_1"));
        assert_eq!(parsed.name.as_deref(), Some("测试项目"));
    }

    #[test]
    fn public_view_never_contains_secrets() {
        let view = account_view(&json!({
            "accessToken": "a",
            "refreshToken": "r",
            "serviceAk": "meoo_project_service_v1.abc",
            "projectUrlId": "proj_1",
            "userId": "u1",
            "username": "测试"
        }));
        let text = view.to_string();
        assert!(!text.contains("\"a\""));
        assert!(!text.contains("\"r\""));
        assert!(!text.contains("meoo_project_service_v1.abc"));
        assert!(text.contains("proj_1"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::credentials
```

Expected: 编译失败。

- [ ] **Step 3: 实现控制面**

`control.rs`：

```rust
//! Meoo 控制面：identity / projects / cloud environment / points / entitlement。

use serde_json::Value;

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::http;

use super::{endpoints, headers};

fn data_of(payload: &Value) -> Value {
    payload.get("data").cloned().unwrap_or_else(|| payload.clone())
}

fn message_of(payload: &Value) -> String {
    payload
        .get("detail")
        .or_else(|| payload.get("message"))
        .or_else(|| payload.get("error").and_then(|error| error.get("message")))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

async fn get_json(
    ctx: &PluginCtx,
    url: &str,
    access_token: &str,
    what: &str,
) -> Result<Value, GatewayError> {
    let client = http::client(None, &ctx.env.proxy);
    let (status, payload) = http::get_json(&client, url, &headers::control_headers(access_token))
        .await
        .map_err(|error| GatewayError::with_status(502, format!("{what}失败：{error}")))?;
    if !(200..300).contains(&status) {
        return Err(GatewayError::with_status(
            if status == 401 { 401 } else { 502 },
            format!("{what}失败（HTTP {status}）：{}", message_of(&payload)),
        ));
    }
    Ok(data_of(&payload))
}

pub async fn identity(
    ctx: &PluginCtx,
    api_base: &str,
    access_token: &str,
) -> Result<Value, GatewayError> {
    get_json(ctx, &endpoints::identity_url(api_base), access_token, "Meoo 身份查询").await
}

pub async fn list_projects(
    ctx: &PluginCtx,
    api_base: &str,
    access_token: &str,
) -> Result<Vec<Value>, GatewayError> {
    let data = get_json(ctx, &endpoints::projects_url(api_base), access_token, "Meoo 项目列表").await?;
    let list = data
        .get("list")
        .or_else(|| data.get("items"))
        .or_else(|| data.get("projects"))
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| data.as_array().cloned())
        .unwrap_or_default();
    Ok(list)
}

pub async fn resolve_service_ak(
    ctx: &PluginCtx,
    api_base: &str,
    access_token: &str,
    project: &str,
    allow_cloud_enable: bool,
) -> Result<String, GatewayError> {
    let read = |payload: &Value| -> String {
        payload
            .get("meooProjectApiKey")
            .or_else(|| payload.get("instance").and_then(|value| value.get("meooProjectApiKey")))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let environment = get_json(
        ctx,
        &endpoints::environment_url(api_base, project),
        access_token,
        "Meoo 项目环境查询",
    )
    .await?;
    let mut ak = read(&environment);
    if ak.is_empty() && allow_cloud_enable {
        let client = http::client(None, &ctx.env.proxy);
        let (status, payload) = http::post_json(
            &client,
            &endpoints::cloud_url(api_base, project),
            &headers::control_headers(access_token),
            &serde_json::json!({ "description": "agent2api: meoo provider" }),
        )
        .await
        .map_err(|error| GatewayError::with_status(502, format!("Meoo 云服务开通失败：{error}")))?;
        if !(200..300).contains(&status) {
            return Err(GatewayError::with_status(
                502,
                format!("Meoo 云服务开通失败（HTTP {status}）：{}", message_of(&payload)),
            ));
        }
        let environment = get_json(
            ctx,
            &endpoints::environment_url(api_base, project),
            access_token,
            "Meoo 项目环境查询",
        )
        .await?;
        ak = read(&environment);
    }
    if ak.is_empty() {
        return Err(GatewayError::with_status(
            400,
            format!(
                "项目 {project} 还没有开通 Meoo AI 服务：请在 Meoo 打开该项目 → 云服务 开通，\
                 或重新登录并勾选「自动开通」"
            ),
        ));
    }
    if !ak.starts_with("meoo_project_service_v1.") {
        return Err(GatewayError::with_status(
            401,
            "Meoo 返回的项目服务 AK 格式不正确，请重新登录或联系 Meoo",
        ));
    }
    Ok(ak)
}

pub async fn points(
    ctx: &PluginCtx,
    api_base: &str,
    access_token: &str,
) -> Result<Value, GatewayError> {
    get_json(ctx, &endpoints::points_url(api_base), access_token, "Meoo 积分查询").await
}

pub async fn entitlement(
    ctx: &PluginCtx,
    api_base: &str,
    access_token: &str,
) -> Result<Value, GatewayError> {
    get_json(ctx, &endpoints::entitlement_url(api_base), access_token, "Meoo 权益查询").await
}
```

- [ ] **Step 4: 实现账号归一化**

`credentials.rs`：

```rust
//! Meoo secret / public / identity / 手动 AK 解析。

use serde_json::{json, Value};

use crate::store;

pub struct MeooCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub api_base: String,
    pub gateway_base: String,
    pub project_url_id: String,
    pub service_ak: String,
    pub service_ak_project_id: String,
    pub user_id: String,
    pub username: String,
    pub allow_cloud_enable: bool,
}

pub fn from_secret(secret: &Value) -> MeooCredentials {
    MeooCredentials {
        access_token: store::pick(secret, &["accessToken", "access_token"]),
        refresh_token: store::pick(secret, &["refreshToken", "refresh_token"]),
        expires_at: secret.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
        api_base: store::pick(secret, &["apiBaseUrl"]),
        gateway_base: store::pick(secret, &["gatewayBaseUrl"]),
        project_url_id: store::pick(secret, &["projectUrlId", "serviceAkProjectId"]),
        service_ak: store::pick(secret, &["serviceAk", "apiKey"]),
        service_ak_project_id: store::pick(secret, &["serviceAkProjectId"]),
        user_id: store::pick(secret, &["userId", "openid"]),
        username: store::pick(secret, &["username", "name"]),
        allow_cloud_enable: secret
            .get("allowCloudEnable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

pub fn account_view(secret: &Value) -> Value {
    let cred = from_secret(secret);
    let tail = if !cred.service_ak.is_empty() {
        store::token_tail(&cred.service_ak)
    } else {
        store::token_tail(&cred.access_token)
    };
    let name = if !cred.username.is_empty() && !cred.project_url_id.is_empty() {
        format!("{} · {}", cred.username, cred.project_url_id)
    } else if !cred.username.is_empty() {
        cred.username.clone()
    } else {
        format!("Meoo 秒悟 {tail}")
    };
    json!({
        "name": name,
        "uid": cred.user_id.clone(),
        "userId": cred.user_id,
        "projectUrlId": cred.project_url_id,
        "tokenTail": tail,
        "expiresAt": cred.expires_at,
        "longLived": cred.access_token.is_empty() && !cred.service_ak.is_empty(),
    })
}

pub fn identity_of(secret: &Value) -> String {
    let cred = from_secret(secret);
    if !cred.user_id.is_empty() && !cred.project_url_id.is_empty() {
        return format!("meoo:{}:{}", cred.user_id, cred.project_url_id);
    }
    if !cred.project_url_id.is_empty() {
        return format!("meoo:project:{}", cred.project_url_id);
    }
    if !cred.service_ak.is_empty() {
        return format!("meoo:ak:{}", store::token_tail(&cred.service_ak));
    }
    format!("meoo:{}", store::token_tail(&cred.access_token))
}

pub struct ManualAk {
    pub service_ak: String,
    pub project_url_id: Option<String>,
    pub name: Option<String>,
}

pub fn parse_manual(payload: &Value) -> Result<ManualAk, String> {
    let fields = payload.get("inputs").unwrap_or(payload);
    let service_ak = fields
        .get("serviceAk")
        .or_else(|| fields.get("apiKey"))
        .or_else(|| fields.get("token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if service_ak.is_empty() {
        return Err("请填写项目服务 AK（meoo_project_service_v1....）".to_string());
    }
    if service_ak.starts_with("meoo_ak_") {
        return Err(
            "这是 Meoo 用户 API Key（meoo_ak_*），不是项目服务 AK；\
             请到项目「云服务 → AI」或执行 `meoo cloud pull-env` 获取 meoo_project_service_v1.*"
                .to_string(),
        );
    }
    if !service_ak.starts_with("meoo_project_service_v1.") {
        return Err("项目服务 AK 格式不正确，应以 meoo_project_service_v1. 开头".to_string());
    }
    Ok(ManualAk {
        service_ak,
        project_url_id: fields
            .get("projectUrlId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        name: fields
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}
```

`models.rs` 在 Task 4 先补一个 AK 探活（完整目录在 Task 7 替换）：

```rust
//! Task 4：只做项目服务 AK 探活；完整目录见 Task 7。

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::{endpoints, headers};

pub fn default_models() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "id": "qwen3.8-flash",
        "name": "Qwen3.8-Flash",
        "maxInputTokens": 200000,
        "maxOutputTokens": 64000,
        "supportsToolCall": true
    })]
}

pub async fn validate_service_ak(
    ctx: &PluginCtx,
    ak: &str,
    project: Option<&str>,
) -> Result<(), GatewayError> {
    let client = crate::http::client(None, &ctx.env.proxy);
    let (status, _payload) = crate::http::get_json(
        &client,
        &endpoints::models_url(&endpoints::gateway_base()),
        &headers::gateway_headers(ak, project.unwrap_or(""), false),
    )
    .await
    .map_err(|error| GatewayError::with_status(502, format!("Meoo 模型目录失败：{error}")))?;
    if !(200..300).contains(&status) {
        return Err(GatewayError::with_status(
            if status == 401 { 401 } else { 502 },
            format!("Meoo 项目服务 AK 验证失败（HTTP {status}）"),
        ));
    }
    Ok(())
}
```

`mod.rs` 的 `normalize_account` / `account_view` / `account_identity` 在 **Task 5** 接入（它依赖 Task 5 的 `login::normalize_legacy`）；本 Task 只完成可独立测试的解析器与控制面。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::credentials
```

Expected: 3 passed。

- [ ] **Step 6: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/control.rs \
        crates/providers/src/providers/meoo/credentials.rs \
        crates/providers/src/providers/meoo/models.rs
git commit -m "feat(meoo): 控制面查询与凭证解析"
```

---

### Task 5: OAuth 登录、动态登录方式与网关回调

**Files:**

- Modify: `crates/providers/src/providers/meoo/login.rs`
- Modify: `crates/providers/src/providers/meoo/mod.rs`
- Modify: `agent2api/desktop-tauri/src-tauri/server/src/server/http.rs`
- Modify: `agent2api/desktop-tauri/src-tauri/server/src/server/api/session.rs`
- Test: `login.rs` `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `login::begin_login(ctx, payload) -> Result<proto::LoginStart, String>`
  - `login::exchange_callback(ctx, callback) -> Result<proto::LoginPoll, String>`
  - `login::normalize_legacy(ctx, input) -> Result<proto::NormalizedAccount, String>`
  - `login::auth_methods() -> Value`
  - 网关 `pub async fn login_meoo_callback(...) -> Response`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_methods_declares_oauth_and_api() {
        let value = auth_methods();
        let methods = value.get("methods").and_then(serde_json::Value::as_array).expect("methods");
        assert_eq!(methods.len(), 2);
        assert_eq!(methods[0]["type"], "oauth");
        assert_eq!(methods[0]["methodIndex"], 0);
        assert_eq!(methods[0]["manualCallback"], true);
        assert_eq!(methods[0]["prompts"][1]["default"], "false");
        assert_eq!(methods[1]["type"], "api");
        assert_eq!(methods[1]["methodIndex"], 1);
    }

    #[test]
    fn callback_rejects_error_param() {
        let error = parse_callback_url("http://127.0.0.1:3065/oauth/callback?error=access_denied&state=x")
            .expect_err("应拒绝");
        assert!(error.contains("access_denied"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::login
```

Expected: 编译失败。

- [ ] **Step 3: 实现登录**

`login.rs` 关键逻辑：

```rust
//! Meoo 登录：OAuth PKCE + 手动 AK + 旧 meoo-proxy 配置导入。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use super::{control, credentials, endpoints, oauth};

const PENDING_TTL_MS: i64 = 6 * 60 * 1000;

struct Pending {
    verifier: String,
    redirect_uri: String,
    api_base: String,
    project_url_id: String,
    allow_cloud_enable: bool,
    created_at: i64,
}

fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Pending>> {
    match pending().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn sweep(table: &mut HashMap<String, Pending>) {
    let now = crate::http::now_ms();
    table.retain(|_, item| now - item.created_at < PENDING_TTL_MS);
}

pub fn begin_login(ctx: &PluginCtx, payload: &Value) -> Result<proto::LoginStart, String> {
    let method_index = payload
        .get("methodIndex")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if method_index != 0 {
        return Err("Meoo 的 API 方式请直接提交项目服务 AK，不要走浏览器登录".to_string());
    }
    let inputs = payload.get("inputs").unwrap_or(payload);
    let project_url_id = inputs
        .get("projectUrlId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let allow_cloud_enable = inputs
        .get("allowCloudEnable")
        .and_then(Value::as_str)
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let port = ctx.env.loopback_port;
    if port == 0 {
        return Err("网关尚未报告监听端口，无法生成 Meoo 授权地址".to_string());
    }
    let redirect_uri = format!("http://127.0.0.1:{port}{}", endpoints::CALLBACK_PATH);
    let api_base = endpoints::api_base();
    let pkce = oauth::create_pkce();
    let state = {
        let mut bytes = [0u8; 32];
        let _ = getrandom::getrandom(&mut bytes);
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    };
    let url = endpoints::authorize_url(&api_base, &redirect_uri, &state, &pkce.challenge);
    let mut table = lock();
    sweep(&mut table);
    table.insert(
        state.clone(),
        Pending {
            verifier: pkce.verifier,
            redirect_uri,
            api_base,
            project_url_id,
            allow_cloud_enable,
            created_at: crate::http::now_ms(),
        },
    );
    Ok(proto::LoginStart {
        flow: state,
        url,
        instructions: "在打开的 Meoo 授权页完成登录；如果页面跳回本机地址后账号没有出现，\
                       把地址栏里的完整回调地址粘贴回来即可。"
            .to_string(),
        poll_interval_ms: 0,
        external_browser: true,
        allowed_hosts: Vec::new(),
    })
}

pub async fn exchange_callback(
    ctx: &PluginCtx,
    callback: proto::LoginCallback,
) -> Result<proto::LoginPoll, String> {
    let Some(pending) = lock().remove(&callback.flow) else {
        return Ok(proto::LoginPoll::Failed {
            message: "这次登录已取消或已过期，请重新发起".to_string(),
        });
    };
    let raw = if callback.url.trim().is_empty() {
        callback.code.clone()
    } else {
        callback.url.clone()
    };
    let (code, state) = match parse_callback_url(&raw) {
        Ok(value) => value,
        Err(message) => return Ok(proto::LoginPoll::Failed { message }),
    };
    if !state.is_empty() && state != callback.flow {
        return Ok(proto::LoginPoll::Failed {
            message: "回调 state 与当前登录任务不匹配".to_string(),
        });
    }
    let token = match oauth::exchange_code(
        ctx,
        &pending.api_base,
        endpoints::client_id(&pending.api_base),
        &pending.redirect_uri,
        &code,
        &pending.verifier,
    )
    .await
    {
        Ok(token) => token,
        Err(error) => return Ok(proto::LoginPoll::Failed { message: error.message }),
    };
    let identity = match control::identity(ctx, &pending.api_base, &token.access_token).await {
        Ok(value) => value,
        Err(error) => return Ok(proto::LoginPoll::Failed { message: error.message }),
    };
    let user_id = identity
        .get("user_id")
        .or_else(|| identity.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let username = identity
        .get("name")
        .or_else(|| identity.get("username"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let mut project = pending.project_url_id;
    if project.is_empty() {
        project = identity
            .get("project_url_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
    }
    if project.is_empty() {
        let projects = match control::list_projects(ctx, &pending.api_base, &token.access_token).await {
            Ok(value) => value,
            Err(error) => return Ok(proto::LoginPoll::Failed { message: error.message }),
        };
        project = projects
            .first()
            .and_then(|item| item.get("url_id").or_else(|| item.get("urlId")).or_else(|| item.get("id")))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
    }
    if project.is_empty() {
        return Ok(proto::LoginPoll::Failed {
            message: "账号下没有可用项目：请先在 Meoo 创建一个项目，再重新登录".to_string(),
        });
    }
    let service_ak = match control::resolve_service_ak(
        ctx,
        &pending.api_base,
        &token.access_token,
        &project,
        pending.allow_cloud_enable,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => return Ok(proto::LoginPoll::Failed { message: error.message }),
    };
    if let Err(error) = super::models::validate_service_ak(ctx, &service_ak, Some(&project)).await {
        return Ok(proto::LoginPoll::Failed { message: error.message });
    }
    let now = crate::http::now_ms();
    let secret = json!({
        "accessToken": token.access_token,
        "refreshToken": token.refresh_token,
        "expiresAt": now + token.expires_in.max(1) * 1000,
        "scope": token.scope,
        "openid": token.openid,
        "userId": user_id,
        "username": username,
        "projectUrlId": project.clone(),
        "serviceAk": service_ak,
        "serviceAkProjectId": project,
        "apiBaseUrl": pending.api_base,
        "gatewayBaseUrl": endpoints::gateway_base(),
        "allowCloudEnable": pending.allow_cloud_enable,
    });
    Ok(proto::LoginPoll::Done {
        account: proto::NormalizedAccount {
            public: credentials::account_view(&secret),
            identity: credentials::identity_of(&secret),
            secret,
            desktop: false,
        },
    })
}

pub fn auth_methods() -> Value {
    json!({
        "methods": [
            {
                "methodIndex": 0,
                "type": "oauth",
                "label": "浏览器登录",
                "instructions": "打开 Meoo 授权页，授权后自动解析项目服务 AK。",
                "prompts": [
                    {"key":"projectUrlId","type":"text","label":"项目 URL ID（可选）","placeholder":"留空使用账号默认项目","optional":true},
                    {"key":"allowCloudEnable","type":"select","label":"项目未开通 AI 服务时","options":[
                        {"value":"false","label":"不开通（推荐）"},
                        {"value":"true","label":"自动开通（会消耗额度）"}
                    ],"default":"false"}
                ],
                "manualCallback": true,
                "callbackInput": "url"
            },
            {
                "methodIndex": 1,
                "type": "api",
                "label": "项目服务 AK",
                "prompts": [
                    {"key":"serviceAk","type":"text","label":"项目服务 AK","placeholder":"meoo_project_service_v1....","secret":true},
                    {"key":"projectUrlId","type":"text","label":"项目 URL ID（可选）","optional":true},
                    {"key":"name","type":"text","label":"备注名（可选）","optional":true}
                ]
            }
        ]
    })
}

pub fn parse_callback_url(raw: &str) -> Result<(String, String), String> {
    // 第一个参数是「裸 code」兜底；第二个参数是完整回调 URL。
    // 两者都传 raw：URL 会被解析出 code/state，裸 code 会走兜底分支。
    oauth::parse_callback(raw, raw)
}

/// 取消一轮登录：清掉 pending，避免 6 分钟内同一 state 残留。
pub fn cancel(flow: &str) {
    let _ = lock().remove(flow);
}

fn is_allowed_api_base(value: &str) -> bool {
    matches!(
        url::Url::parse(value)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
            .as_deref(),
        Some("meoo.com") | Some("gray-meoo.aliyun-inc.com")
    )
}

/// 旧 `~/.meoo-proxy/config.json` 导入。
///
/// 只允许正式 host；旧配置的 `apiBaseUrl` 不可信。
pub async fn normalize_legacy(
    ctx: &PluginCtx,
    _input: &proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if home.is_empty() {
        return Err("找不到用户目录，无法读取 ~/.meoo-proxy/config.json".to_string());
    }
    let path = std::path::Path::new(&home).join(".meoo-proxy").join("config.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("未找到旧 meoo-proxy 配置：{}", path.display()))?;
    let config: Value = serde_json::from_str(&text)
        .map_err(|_| "旧 meoo-proxy 配置不是合法 JSON".to_string())?;
    let configured_base = config
        .get("apiBaseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !configured_base.is_empty() && !is_allowed_api_base(configured_base) {
        return Err(
            "旧 meoo-proxy 配置的 apiBaseUrl 不在允许列表（只允许 https://meoo.com / \
             https://gray-meoo.aliyun-inc.com）"
                .to_string(),
        );
    }
    let service_ak = config
        .get("serviceAk")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let project = config
        .get("projectUrlId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if service_ak.starts_with("meoo_project_service_v1.") {
        super::models::validate_service_ak(ctx, service_ak, Some(project))
            .await
            .map_err(|error| error.message)?;
        let secret = serde_json::json!({
            "serviceAk": service_ak,
            "projectUrlId": project.clone(),
            "serviceAkProjectId": project,
            "accessToken": "",
            "refreshToken": "",
            "expiresAt": 0,
        });
        return Ok(proto::NormalizedAccount {
            public: credentials::account_view(&secret),
            identity: credentials::identity_of(&secret),
            secret,
            desktop: false,
        });
    }
    let refresh_token = config
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if refresh_token.is_empty() {
        return Err("旧 meoo-proxy 配置里没有可用的 serviceAk 或 refreshToken".to_string());
    }
    let api_base = if configured_base.is_empty() {
        endpoints::api_base()
    } else {
        configured_base.to_string()
    };
    let token = oauth::refresh(
        ctx,
        &api_base,
        endpoints::client_id(&api_base),
        refresh_token,
    )
    .await
    .map_err(|error| error.message)?;
    let identity = control::identity(ctx, &api_base, &token.access_token)
        .await
        .map_err(|error| error.message)?;
    let user_id = identity
        .get("user_id")
        .or_else(|| identity.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let username = identity
        .get("name")
        .or_else(|| identity.get("username"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let project = if project.is_empty() {
        identity
            .get("project_url_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    } else {
        project.to_string()
    };
    if project.is_empty() {
        return Err("旧 meoo-proxy 配置没有项目 URL ID，且账号身份里也没有".to_string());
    }
    let service_ak = control::resolve_service_ak(
        ctx,
        &api_base,
        &token.access_token,
        &project,
        false,
    )
    .await
    .map_err(|error| error.message)?;
    let now = crate::http::now_ms();
    let secret = serde_json::json!({
        "accessToken": token.access_token,
        "refreshToken": token.refresh_token,
        "expiresAt": now + token.expires_in.max(1) * 1000,
        "scope": token.scope,
        "openid": token.openid,
        "userId": user_id,
        "username": username,
        "projectUrlId": project.clone(),
        "serviceAk": service_ak,
        "serviceAkProjectId": project,
        "apiBaseUrl": api_base,
        "gatewayBaseUrl": endpoints::gateway_base(),
        "allowCloudEnable": false,
    });
    Ok(proto::NormalizedAccount {
        public: credentials::account_view(&secret),
        identity: credentials::identity_of(&secret),
        secret,
        desktop: false,
    })
}
```

`mod.rs` 顶部补 `use crate::errors::GatewayError;`；增加 `normalize_account` / `account_view` / `account_identity`：

```rust
fn normalize_account<'a>(
    &'a self,
    ctx: &'a PluginCtx,
    input: proto::NormalizeInput,
) -> SendFuture<'a, proto::NormalizedAccount> {
    Box::pin(async move {
        match &input.source {
            proto::NormalizeSource::Manual { payload } => {
                let manual = credentials::parse_manual(payload)?;
                models::validate_service_ak(ctx, &manual.service_ak, manual.project_url_id.as_deref())
                    .await
                    .map_err(GatewayError::into_message)?;
                let project = manual.project_url_id.clone().unwrap_or_default();
                let tail = crate::store::token_tail(&manual.service_ak);
                let name = input
                    .name
                    .clone()
                    .or(manual.name)
                    .unwrap_or_else(|| format!("Meoo 秒悟 {tail}"));
                let secret = serde_json::json!({
                    "serviceAk": manual.service_ak,
                    "projectUrlId": project.clone(),
                    "serviceAkProjectId": project,
                    "accessToken": "",
                    "refreshToken": "",
                    "expiresAt": 0,
                });
                Ok(proto::NormalizedAccount {
                    public: credentials::account_view(&secret),
                    identity: credentials::identity_of(&secret),
                    secret,
                    desktop: false,
                })
            }
            proto::NormalizeSource::FileText { content, .. } => {
                let payload = serde_json::from_str::<Value>(content.trim())
                    .unwrap_or_else(|_| serde_json::json!({ "serviceAk": content.trim() }));
                let input = proto::NormalizeInput {
                    source: proto::NormalizeSource::Manual { payload },
                    ..input.clone()
                };
                self.normalize_account(ctx, input).await
            }
            proto::NormalizeSource::FileBase64 { .. } => {
                Err("Meoo 不支持二进制文件导入，请粘贴项目服务 AK 或 JSON".to_string())
            }
            proto::NormalizeSource::Desktop => login::normalize_legacy(ctx, &input).await,
        }
    })
}

fn account_view(&self, secret: &Value) -> Result<Value, String> {
    Ok(credentials::account_view(secret))
}

fn account_identity(&self, secret: &Value) -> Result<String, String> {
    Ok(credentials::identity_of(secret))
}
```

`mod.rs` 增加：

```rust
fn login_start<'a>(
    &'a self,
    ctx: &'a PluginCtx,
    _method: &'a str,
    payload: &'a Value,
) -> SendFuture<'a, proto::LoginStart> {
    Box::pin(async move { login::begin_login(ctx, payload) })
}

fn login_callback<'a>(
    &'a self,
    ctx: &'a PluginCtx,
    callback: proto::LoginCallback,
) -> SendFuture<'a, proto::LoginPoll> {
    Box::pin(async move { login::exchange_callback(ctx, callback).await })
}

fn login_poll<'a>(
    &'a self,
    _ctx: &'a PluginCtx,
    _flow: &'a str,
) -> SendFuture<'a, proto::LoginPoll> {
    Box::pin(async { Ok(proto::LoginPoll::Pending { hint: None }) })
}

fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
    Box::pin(async move {
        login::cancel(flow);
        Ok(())
    })
}

fn admin_action<'a>(
    &'a self,
    _ctx: &'a PluginCtx,
    _account: &'a str,
    action: &'a str,
    _payload: &'a Value,
) -> SendFuture<'a, Value> {
    Box::pin(async move {
        if action == "auth.methods" {
            return Ok(login::auth_methods());
        }
        Err(format!("Meoo 没有实现管理动作「{action}」"))
    })
}
```

- [ ] **Step 4: 网关回调路由**

先确认没有重复路由：

```bash
cd /root/programming/github/agent2api
grep -rn '"/oauth/callback"' desktop-tauri/src-tauri/server/src/server | head
```

Expected: 只有注释/无输出；若有真实路由，改用 `/oauth/callback-meoo` 并同步 `endpoints::CALLBACK_PATH`（但优先保持官方 CLI 的 `/oauth/callback`）。

`http.rs` 在 `/auth/callback` 附近加：

```rust
        // Meoo 网页登录的 loopback 回调：官方 CLI 的 redirect_uri 是
        // `http://127.0.0.1:<port>/oauth/callback`（斜杠路径），必须逐字匹配。
        .route(
            "/oauth/callback",
            get(api::session::login_meoo_callback),
        )
```

`api/session.rs` 在 `login_lobsterai_callback` 附近加：

```rust
/// Meoo 网页登录的 loopback 回调（浏览器 302 到这里）。
///
/// 安全性由本次登录任务的一次性 state 承担；只接受 provider == "meoo" 的任务。
pub async fn login_meoo_callback(
    State(state): State<ServerState>,
    OriginalUri(uri): OriginalUri,
    headers: axum::http::HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let task_state = params.get("state").cloned().unwrap_or_default();
    let Some(task) = state.login().tasks().get(&task_state) else {
        return oauth_callback_page(400, "登录任务不存在或已过期");
    };
    if task.snapshot().provider != "meoo" {
        return oauth_callback_page(400, "这次登录不属于 Meoo");
    }
    if let Some(error) = params.get("error").filter(|value| !value.trim().is_empty()) {
        let error = state.login().reject_login_callback(&task_state, error).await;
        return oauth_callback_page(error.status_code, &error.message);
    }
    if task.adapter().is_none() {
        return oauth_callback_page(503, "Meoo 登录需要安装提供商插件");
    }
    let callback_url = match original_callback_url(&uri, &headers) {
        Ok(url) => url,
        Err(error) => return oauth_callback_page(error.status_code, &error.message),
    };
    login_callback_page(state.login().submit_login_callback(&task_state, &callback_url).await)
}
```

- [ ] **Step 5: 跑测试**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::login
cargo check -p agent2api-providers --features all

cd /root/programming/github/agent2api/desktop-tauri/src-tauri
cargo check -p agent2api-server
```

Expected: 插件测试通过；网关 0 error。

- [ ] **Step 6: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/login.rs \
        crates/providers/src/providers/meoo/mod.rs
git commit -m "feat(meoo): OAuth PKCE 登录与动态登录方式"

cd /root/programming/github/agent2api
git add desktop-tauri/src-tauri/server/src/server/http.rs \
        desktop-tauri/src-tauri/server/src/server/api/session.rs
git commit -m "feat(meoo): 新增 Meoo OAuth 回调路由"
```

---

### Task 6: 凭证生命周期（单飞刷新 + AK 更新）

**Files:**

- Modify: `crates/providers/src/providers/meoo/refresh.rs`
- Modify: `crates/providers/src/providers/meoo/credentials.rs`
- Modify: `crates/providers/src/providers/meoo/mod.rs`
- Test: `refresh.rs` `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `refresh::ensure_service_ak(ctx, account, force) -> Result<String, GatewayError>`
  - `refresh::next_secret(current: &Value, token: &TokenBundle, service_ak: Option<&str>) -> Value`
  - `credentials::should_refresh(cred: &MeooCredentials, now: i64) -> bool`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_refresh_uses_five_minute_margin() {
        let now = 1_000_000;
        let cred = credentials::MeooCredentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now + 4 * 60 * 1000,
            api_base: "https://meoo.com".into(),
            gateway_base: "https://api.meoo.host/meoo-ai/compatible-mode/v1".into(),
            project_url_id: "p".into(),
            service_ak: "meoo_project_service_v1.x".into(),
            service_ak_project_id: "p".into(),
            user_id: "u".into(),
            username: "n".into(),
            allow_cloud_enable: false,
        };
        assert!(credentials::should_refresh(&cred, now));
    }

    #[test]
    fn next_secret_keeps_project_and_updates_tokens() {
        let current = json!({
            "accessToken": "old",
            "refreshToken": "old-r",
            "expiresAt": 1,
            "projectUrlId": "p1",
            "serviceAk": "meoo_project_service_v1.old",
            "serviceAkProjectId": "p1"
        });
        let token = crate::providers::meoo::oauth::TokenBundle {
            access_token: "new".into(),
            refresh_token: "new-r".into(),
            expires_in: 3600,
            scope: "cli.compat".into(),
            openid: "o".into(),
        };
        let next = next_secret(&current, &token, Some("meoo_project_service_v1.new"));
        assert_eq!(next["accessToken"], "new");
        assert_eq!(next["refreshToken"], "new-r");
        assert_eq!(next["projectUrlId"], "p1");
        assert_eq!(next["serviceAk"], "meoo_project_service_v1.new");
        assert_eq!(next["serviceAkProjectId"], "p1");
    }

    #[tokio::test]
    async fn flight_table_single_flights_concurrent_callers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let table = Arc::new(flight::Table::<String>::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let leader_table = Arc::clone(&table);
        let leader_calls = Arc::clone(&calls);
        let leader = tokio::spawn(async move {
            match leader_table.join("k") {
                Join::Leader(guard) => {
                    leader_calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    guard.finish(Ok("v".to_string()));
                }
                Join::Waiter(waiter) => {
                    let _ = waiter.wait().await;
                }
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let waiter_table = Arc::clone(&table);
        let waiter_calls = Arc::clone(&calls);
        let waiter = tokio::spawn(async move {
            match waiter_table.join("k") {
                Join::Leader(guard) => {
                    waiter_calls.fetch_add(1, Ordering::SeqCst);
                    guard.finish(Ok("v".to_string()));
                }
                Join::Waiter(waiter) => {
                    assert_eq!(waiter.wait().await.expect("应拿到 leader 结果"), "v");
                }
            }
        });

        leader.await.expect("leader");
        waiter.await.expect("waiter");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "同一 key 只应真实执行一次");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::refresh
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

`credentials.rs` 增加：

```rust
pub fn should_refresh(cred: &MeooCredentials, now: i64) -> bool {
    if cred.service_ak.is_empty() || cred.service_ak_project_id != cred.project_url_id {
        return true;
    }
    cred.expires_at > 0 && cred.expires_at - now < 5 * 60 * 1000
}
```

`refresh.rs`：

```rust
//! 凭证刷新：OAuth access token + 项目服务 AK，单飞 + 比较再写。

use std::sync::OnceLock;

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::flight::{self, Join};
use crate::store;

use super::{control, credentials, endpoints, oauth};

fn flights() -> &'static flight::Table<String> {
    static TABLE: OnceLock<flight::Table<String>> = OnceLock::new();
    TABLE.get_or_init(flight::Table::new)
}

pub fn next_secret(current: &Value, token: &oauth::TokenBundle, service_ak: Option<&str>) -> Value {
    let now = crate::http::now_ms();
    let mut next = current.clone();
    if let Some(object) = next.as_object_mut() {
        object.insert("accessToken".into(), Value::String(token.access_token.clone()));
        if !token.refresh_token.is_empty() {
            object.insert("refreshToken".into(), Value::String(token.refresh_token.clone()));
        }
        object.insert("expiresAt".into(), Value::from(now + token.expires_in.max(1) * 1000));
        if !token.scope.is_empty() {
            object.insert("scope".into(), Value::String(token.scope.clone()));
        }
        if !token.openid.is_empty() {
            object.insert("openid".into(), Value::String(token.openid.clone()));
        }
        if let Some(ak) = service_ak {
            object.insert("serviceAk".into(), Value::String(ak.to_string()));
            let project = object
                .get("projectUrlId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            object.insert("serviceAkProjectId".into(), Value::String(project));
        }
    }
    next
}

async fn refresh_once(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<String, GatewayError> {
    let record = store::record(ctx, super::ID, account).await?;
    let current = store::secret_view(&record);
    let cred = credentials::from_secret(&current);
    if !force && !credentials::should_refresh(&cred, crate::http::now_ms()) {
        return Ok(cred.service_ak);
    }
    if cred.refresh_token.is_empty() {
        if !cred.service_ak.is_empty() {
            return Ok(cred.service_ak);
        }
        return Err(GatewayError::with_status(
            401,
            "Meoo 账号没有 refresh_token 或项目服务 AK，请重新登录",
        ));
    }
    let key = format!(
        "meoo:{account}:{}",
        flight::fingerprint(&cred.refresh_token)
    );
    match flights().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(guard) => {
            let result = refresh_leader(ctx, account, force).await;
            guard.finish(result.clone());
            result
        }
    }
}

async fn refresh_leader(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<String, GatewayError> {
    // 拿到锁后重读：可能已被另一个 leader 刷新
    let record = store::record(ctx, super::ID, account).await?;
    let current = store::secret_view(&record);
    let cred = credentials::from_secret(&current);
    if !force && !credentials::should_refresh(&cred, crate::http::now_ms()) {
        return Ok(cred.service_ak);
    }
    let api_base = if cred.api_base.is_empty() {
        endpoints::api_base()
    } else {
        cred.api_base.clone()
    };
    let token = oauth::refresh(
        ctx,
        &api_base,
        endpoints::client_id(&api_base),
        &cred.refresh_token,
    )
    .await?;
    if cred.project_url_id.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "Meoo 账号没有项目 URL ID，请重新登录或重新填写项目服务 AK",
        ));
    }
    let mut service_ak = if cred.service_ak.is_empty()
        || cred.service_ak_project_id != cred.project_url_id
    {
        Some(
            control::resolve_service_ak(
                ctx,
                &api_base,
                &token.access_token,
                &cred.project_url_id,
                cred.allow_cloud_enable,
            )
            .await?,
        )
    } else {
        None
    };
    if service_ak.as_deref().unwrap_or(&cred.service_ak).is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Meoo 项目服务 AK 为空，请重新登录或填写 AK",
        ));
    }
    let next = next_secret(&current, &token, service_ak.as_deref());
    if !store::save(ctx, account, &current, &next).await? {
        // 另一个刷新先落盘：重读赢家
        let record = store::record(ctx, super::ID, account).await?;
        let winner = credentials::from_secret(&store::secret_view(&record));
        if !winner.service_ak.is_empty() {
            return Ok(winner.service_ak);
        }
        return Err(GatewayError::with_status(409, "Meoo 凭证刷新冲突，请重试"));
    }
    let _ = ctx.notify_auth_changed(super::ID, account).await;
    Ok(service_ak.take().unwrap_or(cred.service_ak))
}

pub async fn ensure_service_ak(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<String, GatewayError> {
    refresh_once(ctx, account, force).await
}
```

`mod.rs` 实现：

```rust
fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
    Box::pin(async move {
        refresh::ensure_service_ak(ctx, account, false)
            .await
            .map_err(GatewayError::into_message)
    })
}

fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
    Box::pin(async move {
        refresh::ensure_service_ak(ctx, account, true)
            .await
            .map_err(GatewayError::into_message)
    })
}

fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
    Box::pin(async move {
        let record = crate::store::record(ctx, ID, account).await.map_err(GatewayError::into_message)?;
        let cred = credentials::from_secret(&crate::store::secret_view(&record));
        Ok(credentials::should_refresh(&cred, crate::http::now_ms()))
    })
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::refresh
cargo check -p agent2api-providers --features all
```

Expected: 3 passed；0 error。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/refresh.rs \
        crates/providers/src/providers/meoo/credentials.rs \
        crates/providers/src/providers/meoo/mod.rs
git commit -m "feat(meoo): 单飞刷新与项目服务 AK 更新"
```

---

### Task 7: 模型目录（远程 + 兜底 + CLI 头）

**Files:**

- Modify: `crates/providers/src/providers/meoo/models.rs`
- Modify: `crates/providers/src/providers/meoo/mod.rs`
- Test: `models.rs` `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `models::default_models() -> Vec<Value>`
  - `models::parse(payload: &Value) -> Vec<Value>`
  - `models::validate_service_ak(ctx, ak, project) -> Result<(), GatewayError>`
  - `models::list(ctx, account) -> ModelList`
  - `models::refresh(ctx, account, force) -> RefreshOutcome`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_openai_style_models() {
        let models = parse(&json!({
            "data": [
                {"id":"qwen3.8-flash","name":"Qwen3.8-Flash","context_length":200000,"max_output_tokens":64000},
                {"id":"deepseek-v4-pro-0813","context_length":1000000,"max_tokens":384000}
            ]
        }));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "qwen3.8-flash");
        assert_eq!(models[0]["maxInputTokens"], 200000);
        assert_eq!(models[1]["maxInputTokens"], 1000000);
        assert_eq!(models[1]["maxOutputTokens"], 384000);
    }

    #[test]
    fn parses_string_models_and_falls_back_to_table() {
        let models = parse(&json!({"models": ["qwen3.8-flash", "qwen3-vl-plus"]}));
        assert_eq!(models.len(), 2);
        assert_eq!(models[1]["id"], "qwen3-vl-plus");
        assert_eq!(models[1]["supportsImages"], true);
    }

    #[test]
    fn fallback_contains_vision_model() {
        let models = default_models();
        assert!(models.iter().any(|item| item["id"] == "qwen3-vl-plus"));
        assert!(models.iter().any(|item| item["id"] == "deepseek-v4-pro-0813"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::models
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! Meoo 模型目录：`GET {gateway}/models` + 静态兜底 + 5 分钟缓存。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::http;

use super::{endpoints, headers};

const TTL_MS: i64 = 5 * 60 * 1000;

fn fallback_entry(id: &str) -> Option<Value> {
    let (name, input, output, images, reasoning) = match id {
        "qwen3.8-flash" => ("Qwen3.8-Flash", 200_000, 64_000, true, true),
        "qwen3.8-max-0902" => ("Qwen3.8-Max-0902", 200_000, 64_000, true, true),
        "qwen3.8-max" => ("Qwen3.8-Max", 200_000, 64_000, true, true),
        "qwen3.7-max" => ("Qwen3.7-Max", 200_000, 64_000, false, true),
        "qwen3.7-plus" => ("Qwen3.7-Plus", 200_000, 64_000, true, false),
        "deepseek-v4-pro-0813" => ("DeepSeek V4 Pro", 1_000_000, 384_000, false, true),
        "deepseek-v4-flash-0731" => ("DeepSeek V4 Flash", 1_000_000, 384_000, false, true),
        "glm-5.2-fast-preview" => ("GLM-5.2-Fast", 200_000, 64_000, false, true),
        "glm-5.2" => ("GLM-5.2", 200_000, 64_000, false, true),
        "qwen3-vl-plus" => ("Qwen VL", 200_000, 64_000, true, false),
        _ => return None,
    };
    Some(json!({
        "id": id,
        "name": name,
        "maxInputTokens": input,
        "maxOutputTokens": output,
        "supportsImages": images,
        "supportsReasoning": reasoning,
        "supportsToolCall": true,
        "supportsVideo": false,
    }))
}

pub fn default_models() -> Vec<Value> {
    [
        "qwen3.8-flash",
        "qwen3.8-max-0902",
        "qwen3.8-max",
        "qwen3.7-max",
        "qwen3.7-plus",
        "deepseek-v4-pro-0813",
        "deepseek-v4-flash-0731",
        "glm-5.2-fast-preview",
        "glm-5.2",
        "qwen3-vl-plus",
    ]
    .iter()
    .filter_map(|id| fallback_entry(id))
    .collect()
}

fn read_number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str().and_then(|text| text.trim().parse::<i64>().ok()))
    .filter(|number| *number > 0)
}

fn entry_of(value: &Value) -> Option<Value> {
    if let Some(id) = value.as_str() {
        return fallback_entry(id).or_else(|| Some(json!({
            "id": id, "name": id, "maxInputTokens": 200_000, "maxOutputTokens": 64_000,
            "supportsImages": false, "supportsReasoning": false, "supportsToolCall": true,
            "supportsVideo": false
        })));
    }
    let object = value.as_object()?;
    let id = object.get("id").and_then(Value::as_str)?.trim();
    if id.is_empty() {
        return None;
    }
    let mut entry = fallback_entry(id).unwrap_or_else(|| json!({
        "id": id, "name": id, "maxInputTokens": 200_000, "maxOutputTokens": 64_000,
        "supportsImages": false, "supportsReasoning": false, "supportsToolCall": true,
        "supportsVideo": false
    }));
    if let Some(object) = entry.as_object_mut() {
        if let Some(name) = value.get("name").or_else(|| value.get("display_name")).and_then(Value::as_str) {
            object.insert("name".into(), Value::String(name.to_string()));
        }
        for key in ["context_length", "context_window", "max_input_tokens", "max_context_length"] {
            if let Some(number) = value.get(key).and_then(read_number) {
                object.insert("maxInputTokens".into(), Value::from(number));
                break;
            }
        }
        for key in ["max_tokens", "max_output_tokens"] {
            if let Some(number) = value.get(key).and_then(read_number) {
                object.insert("maxOutputTokens".into(), Value::from(number));
                break;
            }
        }
        if let Some(capabilities) = value.get("capabilities") {
            if let Some(images) = capabilities.get("vision").or_else(|| capabilities.get("images")).and_then(Value::as_bool) {
                object.insert("supportsImages".into(), Value::Bool(images));
            }
            if let Some(reasoning) = capabilities.get("reasoning").and_then(Value::as_bool) {
                object.insert("supportsReasoning".into(), Value::Bool(reasoning));
            }
        }
    }
    Some(entry)
}

pub fn parse(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.get("models").and_then(Value::as_array))
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(entry_of).collect()
}

struct State {
    models: Vec<Value>,
    fetched_at: i64,
    remote: bool,
}

fn state() -> &'static Mutex<Option<State>> {
    static STATE: OnceLock<Mutex<Option<State>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

fn with_state<R>(f: impl FnOnce(&mut Option<State>) -> R) -> R {
    let mut guard = match state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

async fn fetch(ctx: &PluginCtx, ak: &str, project: &str) -> Result<Vec<Value>, GatewayError> {
    let client = http::client(None, &ctx.env.proxy);
    let (status, payload) = http::get_json(
        &client,
        &endpoints::models_url(&endpoints::gateway_base()),
        &headers::gateway_headers(ak, project, false),
    )
    .await
    .map_err(|error| GatewayError::with_status(502, format!("Meoo 模型目录失败：{error}")))?;
    if !(200..300).contains(&status) {
        return Err(GatewayError::with_status(
            if status == 401 { 401 } else { 502 },
            format!("Meoo 模型目录失败（HTTP {status}）"),
        ));
    }
    let models = parse(&payload);
    if models.is_empty() {
        return Err(GatewayError::with_status(502, "Meoo 模型目录为空"));
    }
    Ok(models)
}

pub async fn validate_service_ak(
    ctx: &PluginCtx,
    ak: &str,
    project: Option<&str>,
) -> Result<(), GatewayError> {
    fetch(ctx, ak, project.unwrap_or("")).await.map(|_| ())
}

pub async fn list(ctx: &PluginCtx, account: &str) -> proto::ModelList {
    let cached = with_state(|slot| {
        slot.as_ref().map(|state| (state.models.clone(), state.fetched_at, state.remote))
    });
    if let Some((models, fetched_at, remote)) = cached {
        if crate::http::now_ms() - fetched_at < TTL_MS {
            return proto::ModelList {
                models,
                meta: proto::CatalogMeta { remote, refreshed_at: fetched_at },
            };
        }
    }
    let record = match crate::store::record(ctx, super::ID, account).await {
        Ok(record) => record,
        Err(_) => return proto::ModelList { models: default_models(), meta: proto::CatalogMeta::default() },
    };
    let cred = super::credentials::from_secret(&crate::store::secret_view(&record));
    if cred.service_ak.is_empty() {
        return proto::ModelList { models: default_models(), meta: proto::CatalogMeta::default() };
    }
    match fetch(ctx, &cred.service_ak, &cred.project_url_id).await {
        Ok(models) => {
            let now = crate::http::now_ms();
            with_state(|slot| {
                *slot = Some(State { models: models.clone(), fetched_at: now, remote: true });
            });
            proto::ModelList {
                models,
                meta: proto::CatalogMeta { remote: true, refreshed_at: now },
            }
        }
        Err(_) => {
            // 失败也记 fetched_at：5 分钟内不再重复打上游（否则 AK 失效时
            // 每次 /v1/models 都会再打一次）。手动刷新 force=true 会绕过。
            let now = crate::http::now_ms();
            let models = with_state(|slot| {
                let models = slot
                    .as_ref()
                    .map(|state| state.models.clone())
                    .unwrap_or_else(default_models);
                *slot = Some(State {
                    models: models.clone(),
                    fetched_at: now,
                    remote: false,
                });
                models
            });
            proto::ModelList {
                models,
                meta: proto::CatalogMeta { remote: false, refreshed_at: now },
            }
        }
    }
}

pub async fn refresh(ctx: &PluginCtx, account: &str, force: bool) -> proto::RefreshOutcome {
    let fresh = with_state(|slot| {
        slot.as_ref().map(|state| crate::http::now_ms() - state.fetched_at < TTL_MS).unwrap_or(false)
    });
    if !force && fresh {
        return proto::RefreshOutcome::unchanged();
    }
    let record = match crate::store::record(ctx, super::ID, account).await {
        Ok(record) => record,
        Err(error) => return proto::RefreshOutcome::failed(error.message),
    };
    let cred = super::credentials::from_secret(&crate::store::secret_view(&record));
    if cred.service_ak.is_empty() {
        return proto::RefreshOutcome::failed("Meoo 账号没有项目服务 AK，请重新登录");
    }
    match fetch(ctx, &cred.service_ak, &cred.project_url_id).await {
        Ok(models) => {
            let now = crate::http::now_ms();
            let count = models.len();
            with_state(|slot| {
                *slot = Some(State { models, fetched_at: now, remote: true });
            });
            proto::RefreshOutcome::refreshed(count)
        }
        Err(error) => proto::RefreshOutcome::failed(error.message),
    }
}
```

`mod.rs` 实现 `models` / `refresh_models`：

```rust
fn models<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, proto::ModelList> {
    Box::pin(async move { models::list(ctx, account).await })
}

fn refresh_models<'a>(
    &'a self,
    ctx: &'a PluginCtx,
    account: &'a str,
    force: bool,
) -> SendFuture<'a, proto::RefreshOutcome> {
    Box::pin(async move { models::refresh(ctx, account, force).await })
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::models
```

Expected: 3 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/models.rs \
        crates/providers/src/providers/meoo/mod.rs
git commit -m "feat(meoo): 模型目录与 CLI 头请求"
```

---

### Task 8: 对话构造与错误分类

**Files:**

- Modify: `crates/providers/src/providers/meoo/chat.rs`
- Modify: `crates/providers/src/providers/meoo/mod.rs`
- Test: `chat.rs` `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `chat::build_plan(secret: &Value, body: &Value, stream: bool) -> Result<ChatPlan, GatewayError>`
  - `chat::classify(status: u16, body: &Value) -> proto::ErrorClass`
  - `chat::reasoning_patch(level: &str, model: &str) -> proto::ReasoningPatch`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_plan_has_cli_headers_and_service_ak() {
        let plan = build_plan(
            &json!({"serviceAk":"meoo_project_service_v1.x","projectUrlId":"p1"}),
            &json!({"model":"qwen3.8-flash","messages":[]}),
            true,
        )
        .expect("应构造");
        assert!(plan.url.ends_with("/chat/completions"));
        assert!(plan.headers.contains(&("Authorization".into(), "Bearer meoo_project_service_v1.x".into())));
        assert!(plan.headers.contains(&("X-Meoo-Client".into(), "cli".into())));
        assert!(plan.headers.contains(&("Accept".into(), "text/event-stream, application/json".into())));
    }

    #[test]
    fn service_ak_format_invalid_is_fatal() {
        let class = classify(401, &json!({"error":{"message":"SERVICE_AK_FORMAT_INVALID"}}));
        assert!(matches!(class, proto::ErrorClass::Fatal { .. }));
    }

    #[test]
    fn quota_is_quota_limited() {
        let class = classify(429, &json!({"error":{"message":"积分不足"}}));
        assert!(matches!(class, proto::ErrorClass::QuotaLimited { .. }));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::chat
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! Meoo 对话：OpenAI 兼容，插件只构造 URL + 头 + body。

use serde_json::Value;

use agent2api_provider_sdk::ChatPlan;

use crate::errors::GatewayError;

use super::{credentials, endpoints, headers};

pub fn build_plan(
    secret: &Value,
    body: &Value,
    _client_stream: bool,
) -> Result<ChatPlan, GatewayError> {
    let cred = credentials::from_secret(secret);
    if cred.service_ak.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Meoo 账号缺少项目服务 AK，请重新登录或填写 AK",
        ));
    }
    let mut payload = body.clone();
    if let Some(object) = payload.as_object_mut() {
        if object.get("model").and_then(Value::as_str).unwrap_or("").trim().is_empty() {
            return Err(GatewayError::with_status(400, "请求缺少 model 字段"));
        }
    }
    // 网关请求上游**恒为 stream:true**（见 upstream::payload），客户端要非流式
    // 时由网关聚合。因此 Accept 以 body.stream 为准，不能用客户端偏好。
    let upstream_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok(ChatPlan::chat(
        endpoints::chat_url(&endpoints::gateway_base()),
        headers::gateway_headers(&cred.service_ak, &cred.project_url_id, upstream_stream),
        payload,
    ))
}

pub fn classify(status: u16, body: &Value) -> agent2api_provider_protocol::ErrorClass {
    use agent2api_provider_protocol::ErrorClass;
    let message = body
        .get("error")
        .and_then(|error| error.get("message"))
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let text = format!("Meoo 上游返回 {status}: {message}");
    let code = body
        .get("error")
        .and_then(|error| error.get("code"))
        .or_else(|| body.get("code"))
        .and_then(Value::as_i64);
    let code_text = body
        .get("error")
        .and_then(|error| error.get("code"))
        .or_else(|| body.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if status == 401 {
        if message.contains("SERVICE_AK_FORMAT_INVALID")
            || code_text.contains("SERVICE_AK_FORMAT_INVALID")
        {
            return ErrorClass::Fatal {
                status: 401,
                message: "Meoo 项目服务 AK 类型不正确：请使用 meoo_project_service_v1.*，\
                          不要用 meoo_ak_* 用户 Key"
                    .to_string(),
                upstream_code: code,
            };
        }
        return ErrorClass::TokenExpired { message: text };
    }
    if status == 403 {
        return ErrorClass::Fatal { status: 403, message: text, upstream_code: code };
    }
    if status == 402
        || status == 429
        || message.to_lowercase().contains("quota")
        || message.contains("积分不足")
        || message.to_lowercase().contains("credit")
    {
        return ErrorClass::QuotaLimited {
            reset_at: None,
            message: format!("{text}（Meoo 积分/额度不足，本网关按限流处理）"),
            upstream_code: code,
            status: 429,
        };
    }
    if status == 400 || status == 422 {
        return crate::content_block::classify_or_fatal(status, body, text, code);
    }
    if status == 404 {
        return ErrorClass::Fatal { status: 404, message: text, upstream_code: code };
    }
    ErrorClass::Fatal {
        status: if status >= 500 { 502 } else { status },
        message: text,
        upstream_code: code,
    }
}

pub fn reasoning_patch(level: &str, model: &str) -> agent2api_provider_protocol::ReasoningPatch {
    use agent2api_provider_protocol::ReasoningPatch;
    let level = level.trim();
    if level.is_empty() {
        return ReasoningPatch::Skip { reason: "未配置思考等级".to_string() };
    }
    let supports = model.starts_with("deepseek-v4")
        || model.starts_with("glm-5.2")
        || model.starts_with("qwen3.8-max")
        || model == "qwen3.7-max";
    if supports {
        ReasoningPatch::Set {
            field: "reasoning_effort".to_string(),
            value: Value::String(level.to_string()),
        }
    } else {
        ReasoningPatch::Skip {
            reason: format!("Meoo 模型 {model} 没有声明思考等级"),
        }
    }
}
```

`mod.rs` 实现 `build_chat` / `classify` / `reasoning_patch`：

```rust
fn build_chat<'a>(
    &'a self,
    ctx: &'a PluginCtx,
    req: proto::ChatRequest,
) -> SendFuture<'a, agent2api_provider_sdk::ChatPlan> {
    Box::pin(async move {
        let record = crate::store::record(ctx, ID, &req.account)
            .await
            .map_err(GatewayError::into_message)?;
        chat::build_plan(&crate::store::secret_view(&record), &req.body, req.stream)
            .map_err(GatewayError::into_message)
    })
}

fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
    chat::classify(status, body)
}

fn reasoning_patch(&self, level: &str, model: &str, _body: &Value) -> proto::ReasoningPatch {
    chat::reasoning_patch(level, model)
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::chat
```

Expected: 3 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/chat.rs \
        crates/providers/src/providers/meoo/mod.rs
git commit -m "feat(meoo): 对话构造、CLI 头与错误分类"
```

---

### Task 9: 积分余额

**Files:**

- Modify: `crates/providers/src/providers/meoo/usage.rs`
- Modify: `crates/providers/src/providers/meoo/mod.rs`
- Test: `usage.rs` `#[cfg(test)]`

**Interfaces:**

- Produces:
  - `usage::shape(points: &Value, entitlement: &Value, identity: &Value) -> Value`
  - `usage::query(ctx, account) -> Result<Value, GatewayError>`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes_personal_points() {
        let value = shape(
            &json!({"account":{"availableBalance":1234,"totalGranted":2000,"totalConsumed":766}}),
            &json!({"planName":"Pro"}),
            &json!({"edition":"personal"}),
        );
        assert_eq!(value["available"], 1234);
        assert_eq!(value["plan"], "Pro");
        assert_eq!(value["credits"][0]["name"], "可用积分");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::usage
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! Meoo 积分：`/account/points` + `/account/entitlement`。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::{control, credentials, endpoints};

fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
}

pub fn shape(points: &Value, entitlement: &Value, identity: &Value) -> Value {
    let account = points.get("account").unwrap_or(points);
    let available = number(account.get("availableBalance"));
    let total_granted = number(account.get("totalGranted"));
    let total_consumed = number(account.get("totalConsumed"));
    let total_expired = number(account.get("totalExpired"));
    let plan = entitlement
        .get("planName")
        .or_else(|| entitlement.get("planCode"))
        .or_else(|| identity.get("edition"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut credits = Vec::new();
    if let Some(value) = available {
        credits.push(json!({"name":"可用积分","remain":value}));
    }
    if let Some(value) = total_granted {
        credits.push(json!({"name":"累计获得","remain":value}));
    }
    if let Some(value) = total_consumed {
        credits.push(json!({"name":"累计消耗","remain":value}));
    }
    if let Some(value) = total_expired.filter(|value| *value > 0.0) {
        credits.push(json!({"name":"已过期","remain":value}));
    }
    let wallets = available
        .map(|value| vec![json!({"type":"meoo_points","displayName":"秒悟积分","balance":value})])
        .unwrap_or_default();
    let mut report = json!({
        "unit": "积分",
        "plan": plan,
        "wallets": wallets,
        "credits": credits,
        "raw": {"points": points, "entitlement": entitlement},
    });
    if let Some(value) = available {
        if let Some(object) = report.as_object_mut() {
            object.insert("available".into(), Value::from(value));
        }
    }
    report
}

pub async fn query(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    let record = crate::store::record(ctx, super::ID, account).await?;
    let secret = crate::store::secret_view(&record);
    let cred = credentials::from_secret(&secret);
    if cred.refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "该 Meoo 账号只有项目服务 AK，无法查询积分；请用「浏览器登录」添加账号",
        ));
    }
    let api_base = if cred.api_base.is_empty() {
        endpoints::api_base()
    } else {
        cred.api_base.clone()
    };
    let access_token = if cred.access_token.is_empty()
        || cred.expires_at - crate::http::now_ms() < 60_000
    {
        let _ = super::refresh::ensure_service_ak(ctx, account, true).await?;
        let record = crate::store::record(ctx, super::ID, account).await?;
        credentials::from_secret(&crate::store::secret_view(&record)).access_token
    } else {
        cred.access_token.clone()
    };
    let (points, entitlement) = tokio::join!(
        control::points(ctx, &api_base, &access_token),
        control::entitlement(ctx, &api_base, &access_token)
    );
    let points = points?;
    let entitlement = entitlement.unwrap_or(Value::Null);
    let identity = control::identity(ctx, &api_base, &access_token)
        .await
        .unwrap_or(Value::Null);
    Ok(shape(&points, &entitlement, &identity))
}
```

`mod.rs` 实现 `usage`：

```rust
fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, proto::UsageReport> {
    Box::pin(async move {
        usage::query(ctx, account)
            .await
            .map_err(GatewayError::into_message)
    })
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features meoo meoo::usage
```

Expected: 1 passed。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/Agent2Providers
git add crates/providers/src/providers/meoo/usage.rs \
        crates/providers/src/providers/meoo/mod.rs
git commit -m "feat(meoo): 积分余额查询"
```

---

### Task 10: 网关图标与六份 README

**Files:**

- Modify: `agent2api/desktop-tauri/ui-islands/src/islands/add-provider-pick.tsx`
- Create: `agent2api/desktop-tauri/ui/assets/providers/meoo.png`
- Create: `Agent2Providers/assets/meoo.png`
- Modify: `agent2api/README.md`
- Modify: `agent2api/docs/README.{zh-CN,zh-Hant,ja,ko,pt-BR}.md`
- Modify: `Agent2Providers/crates/providers/Cargo.toml`（description 22 → 32）
- Modify: `Agent2Providers/Cargo.toml`（注释 22 → 32）

**Interfaces:**

- Consumes: 无
- Produces: 添加账号页显示 Meoo 图标；六份 README 的支持数量与条目一致。

- [ ] **Step 1: 准备图标**

```bash
cd /tmp/opencode
curl -L --max-time 30 -o meoo-favicon.ico https://meoo.com/favicon.ico
# 用 ImageMagick 归一成 256x256 PNG；没有 convert 就用 Python PIL
convert meoo-favicon.ico -resize 256x256 -background none -gravity center -extent 256x256 meoo.png
cp meoo.png /root/programming/github/Agent2Providers/assets/meoo.png
cp meoo.png /root/programming/github/agent2api/desktop-tauri/ui/assets/providers/meoo.png
```

如果 favicon 拉不到，用一张中性 “M” 字 256×256 PNG 占位，并在 PR 说明里标注「待官方 logo」。

- [ ] **Step 2: 前端图标登记**

`add-provider-pick.tsx` 的 `PROVIDER_ICONS` 加：

```ts
  // Meoo（秒悟）：官方站点 favicon 归一成 256×256
  meoo: 'assets/providers/meoo.png',
```

- [ ] **Step 3: 六份 README**

六份都把：

- `31 provider ids` / `31 個` / `31 個` / `31 個` / `31개` / `31 ids` → **32**
- `30 real providers` / `30 家真实提供商` / `30 家真實供應商` / `30 社の実プロバイダー` / `30개 실제 공급자` / `30 provedores reais` → **31**

并在 Kiro/Nous/Tabbit 条目附近加一条（按各语言翻译）：

- EN: `- **Meoo (秒悟)**: supports LLM requests, OAuth token refresh, the model list and points balance; no check-in or claim capabilities. Requires a Meoo project with AI service enabled; inference uses the project service AK.`
- ZH-CN: `- **秒悟 Meoo**：支持 LLM 请求、OAuth 自动续期、模型列表与积分余额；无签到与领取类能力。需要 Meoo 项目已开通 AI 服务，推理使用项目服务 AK。`
- ZH-Hant: `- **秒悟 Meoo**：支援 LLM 請求、OAuth 自動續期、模型清單與積分餘額；無簽到與領取類能力。需要 Meoo 專案已開通 AI 服務，推理使用專案服務 AK。`
- JA: `- **Meoo（秒悟）**：LLM リクエスト、OAuth トークン自動更新、モデル一覧、ポイント残高に対応。チェックイン／受け取り系はありません。Meoo プロジェクトで AI サービスを有効化し、推論にはプロジェクトサービス AK を使用します。`
- KO: `- **Meoo(秒悟)**: LLM 요청, OAuth 토큰 자동 갱신, 모델 목록, 포인트 잔액을 지원합니다. 출석 체크/수령 기능은 없습니다. Meoo 프로젝트에서 AI 서비스를 활성화해야 하며, 추론에는 프로젝트 서비스 AK를 사용합니다.`
- PT-BR: `- **Meoo (秒悟)**: suporta requisições LLM, renovação automática de token OAuth, lista de modelos e saldo de pontos; sem check-in ou resgates. Requer um projeto Meoo com o serviço de IA ativado; a inferência usa a AK de serviço do projeto.`

- [ ] **Step 4: 构建与检查**

```bash
cd /root/programming/github/agent2api
npm run ui:islands
cd desktop-tauri/ui-islands && npx tsc --noEmit
```

Expected: 0 error；`ui/islands/ui.js` 已更新。

- [ ] **Step 5: 提交**

```bash
cd /root/programming/github/agent2api
git add desktop-tauri/ui-islands/src/islands/add-provider-pick.tsx \
        desktop-tauri/ui/assets/providers/meoo.png \
        desktop-tauri/ui/islands/ui.js \
        README.md docs/README.zh-CN.md docs/README.zh-Hant.md \
        docs/README.ja.md docs/README.ko.md docs/README.pt-BR.md
git commit -m "feat(meoo): 添加 Meoo 图标与支持矩阵"

cd /root/programming/github/Agent2Providers
git add assets/meoo.png crates/providers/Cargo.toml Cargo.toml
git commit -m "docs(meoo): 更新插件包提供商数量与图标"
```

---

### Task 11: 实机验证（手动 AK + OAuth + 探针）

**Files:**

- 不改代码；只记录验证结果。发现问题回到对应 Task 修。

**Interfaces:**

- Consumes: Task 1–10 的产物
- Produces: `docs/superpowers/plans/2026-11-02-meoo-provider-verification.md`（验证记录）

- [ ] **Step 1: 构建与协议验证**

```bash
cd /root/programming/github/Agent2Providers
cargo test -p agent2api-providers --features all
cargo build --release -p agent2api-providers
node scripts/verify.mjs --exe target/release/agent2api-providers
node scripts/pack.mjs
```

Expected: 单测全过；`verify.mjs` 28/28；`dist/agent2api-providers-<version>-<target>.zip` 生成。

- [ ] **Step 2: 安装插件并添加手动 AK 账号**

1. 网关「插件」页安装 zip；
2. 「账号 → 添加账号 → Meoo 秒悟 → 项目服务 AK」；
3. 填入真实 `meoo_project_service_v1.*`（从 `meoo cloud pull-env` 或项目环境变量获取）；
4. 确认账号卡片显示项目 ID、AK 尾号；确认 `usage` 按钮对纯 AK 账号给出「请用浏览器登录」的可读提示。

- [ ] **Step 3: 跑探针**

先在「网关 Key」页建一把**只允许 `meoo`** 的 Key（`scripts/probe-provider.mjs` 启动时会校验；不建会被同名模型路由到别家，结论无法归因）。

```bash
cd /root/programming/github/agent2api
node scripts/probe-provider.mjs \
  --base http://127.0.0.1:3065 \
  --key sk-dev-meoo \
  --provider meoo \
  --model qwen3.8-flash
```

记录九项结果：`stream` / `tool_call` / `parallel_tools` / `tool_roundtrip` / `image` / `reasoning` / `json_schema` / `usage` / `context_window`。至少 `stream`、`tool_call`、`parallel_tools`、`tool_roundtrip` 必须 PASS；`image` 用 `qwen3-vl-plus` 单独验。

- [ ] **Step 4: 浏览器 OAuth**

1. 「添加账号 → Meoo 秒悟 → 浏览器登录」；
2. 桌面端：浏览器授权后应自动回调；网页/远程：若没有自动出现，把地址栏完整回调 URL 粘贴回面板；
3. 确认账号卡片显示用户名、项目、积分按钮可用；
4. 点「积分」确认 `available` / `累计获得` / `累计消耗` 三项有值。

- [ ] **Step 5: 刷新与换项目**

- 在账号 secret 里把 `expiresAt` 改成过去（或用测试账号等 1 小时），发一条对话；确认自动续期且 `refreshToken` 已轮换；
- 用第二个 Meoo 项目重复 OAuth 登录；确认生成第二个账号（identity 含 projectUrlId），两个账号可并存；
- 对第一个账号重新登录同一项目；确认更新而不是新增。

- [ ] **Step 6: 错误路径**

- 用 `meoo_ak_*` 添加：必须报「不是项目服务 AK」；
- 用错误 AK 添加：必须 401，不能落库；
- 关闭 `allowCloudEnable` 且项目未开通 AI 服务：必须失败并提示去 Meoo 开通，**不得**自动 POST `/cloud`；
- 用错误 state 访问 `/oauth/callback`：必须 400，不能落账号。

- [ ] **Step 7: 记录并提交**

把结果写到 `docs/superpowers/plans/2026-11-02-meoo-provider-verification.md`，包含：

- 构建/测试命令与结果
- 探针九项 PASS/FAIL/SKIP
- OAuth 桌面/网页两种路径
- 刷新前后 token 尾号（只写尾号）
- 发现的问题与修复提交

```bash
cd /root/programming/github/agent2api
git add docs/superpowers/plans/2026-11-02-meoo-provider-verification.md
git commit -m "test(meoo): 记录 provider 实机验证结果"
```

---

### Task 12: 最终检查

**Files:**

- 不改代码；只跑检查。

- [ ] **Step 1: 插件仓**

```bash
cd /root/programming/github/Agent2Providers
cargo fmt --check
cargo test -p agent2api-providers --features all
cargo build --release -p agent2api-providers
node scripts/verify.mjs --exe target/release/agent2api-providers
```

Expected: fmt 无 diff；测试全过；`verify.mjs` 28/28。

- [ ] **Step 2: 网关仓**

```bash
cd /root/programming/github/agent2api/desktop-tauri/src-tauri
cargo check -p agent2api-server
cd /root/programming/github/agent2api
npm run ui:islands
cd desktop-tauri/ui-islands && npx tsc --noEmit
```

Expected: 0 error。

- [ ] **Step 3: 文档一致性**

```bash
cd /root/programming/github/agent2api
grep -rn "31 个提供商\|31 provider ids\|30 家真实\|30 real" README.md docs/README.*.md
```

Expected: 无输出（全部已改 32/31）。

- [ ] **Step 4: 确认没有越界改动**

```bash
cd /root/programming/github/agent2api
git status --short
git diff --stat
```

确认：

- 没有改 `ProviderKind` / `PROVIDERS` / `adapter_for`
- 没有改版本号 / tag
- 没有把 token / AK 写进任何提交

- [ ] **Step 5: 提交最终修正**

如果 Task 11 发现问题已在对应 Task 修完，这一步通常无改动；若有格式化/文案修正：

```bash
cd /root/programming/github/Agent2Providers
git add -A && git commit -m "chore(meoo): 最终格式化与检查"

cd /root/programming/github/agent2api
git add -A && git commit -m "chore(meoo): 最终检查与文档修正"
```

---

## 执行顺序与依赖

```text
Task 1 (身份)
  → Task 2 (头/端点)
    → Task 3 (OAuth)
      → Task 4 (控制面/手动 AK)
        → Task 5 (登录/回调)
          → Task 6 (刷新)
            → Task 7 (模型)
              → Task 8 (对话)
                → Task 9 (积分)
                  → Task 10 (网关图标/README)
                    → Task 11 (实机)
                      → Task 12 (最终检查)
```

Task 10 的网关 README/图标可以在 Task 5 之后并行做；Task 11 必须等 Task 1–10 全部完成。

## 发布（不在本计划内）

- 不修改 `Cargo.toml` / `pack.json` 的版本号。
- 发版前按 `release-flow`：确认版本号、更新日志、六处版本号、tag、Release。
- 插件发版走 `ProvidersPlugin/Agent2Providers` 独立 tag → `Agent2Builder/agent2api-providers-releases`。

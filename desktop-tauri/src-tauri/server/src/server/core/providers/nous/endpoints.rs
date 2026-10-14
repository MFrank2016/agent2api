//! Nous Research Portal（`portal.nousresearch.com`）的地址、客户端常量与头集合。
//!
//! ── 上游长什么样（规格：本任务给的流程 A/B/C）────────────────────
//! 一条身份链（Portal OAuth 设备码）＋ 两个推理面（付费 / 免费）：
//! ```text
//!   POST {portal}/api/oauth/device/code   设备码（form）→ user_code / verification_uri
//!   POST {portal}/api/oauth/token         轮询 / 续期（form）
//!   POST {portal}/api/oauth/agent-key     用 access_token 铸推理用的 agent key（Bearer）
//!   POST {portal}/api/anonymous/create    匿名身份（**UA 必须是 hermes-agent 形态**）
//!   POST {portal}/api/anonymous/token     匿名身份 → 免费档 JWT（~900s）
//!   POST {inference_base}/chat/completions  付费面（Bearer agent key）
//!   POST {welcome_base}/chat/completions    免费面（Bearer 匿名 JWT，**只认 nous/welcome**）
//!   GET  {inference_base}/models          模型目录（Bearer）
//! ```
//!
//! ── 两个推理面（别混）────────────────────────────────────────
//!   - **付费面** `https://inference-api.nousresearch.com/v1`：账号（agent key）用；
//!     匿名 JWT 打这里会回 400「Anonymous accounts must use …welcome-api…」；
//!   - **免费面** `https://welcome-api.nousresearch.com/v1`：匿名 JWT 专用，
//!     只认 `nous/welcome`（其它模型 429 `model_not_free`）。
//! 账号的 `inference_base_url` 若上游给了（token / agent-key 响应里的同名字段），
//! 以它为准（见 `DEFAULT_INFERENCE_BASE_URL` 的说明）。
//!
//! ── 匿名创建为什么必须伪装成官方 CLI（硬事实，别省）──────────
//! `POST /api/anonymous/create` 按 **User-Agent** 做门禁：不是
//! `hermes-agent/<ver> (cli; <os>; <arch>)` 形态一律 403。参考实现（官方
//! `hermes-agent`）逐字就是这个形态，因此这里照拼。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

/// Portal 基址（身份链：设备码 / 令牌 / agent key）
pub const PORTAL_BASE_URL: &str = "https://portal.nousresearch.com";

/// 付费推理面默认基址（账号用；上游给了 `inference_base_url` 时以它为准）
pub const DEFAULT_INFERENCE_BASE_URL: &str = "https://inference-api.nousresearch.com/v1";

/// 免费推理面基址（匿名专用，硬编码；匿名令牌响应里的 `inference_base_url` 恒为它）
pub const WELCOME_BASE_URL: &str = "https://welcome-api.nousresearch.com/v1";

/// 免费档唯一可用的模型名
pub const WELCOME_MODEL: &str = "nous/welcome";

/// OAuth 客户端 id（官方 hermes-cli 用的那个）
pub const CLIENT_ID: &str = "hermes-cli";

/// 设备码授权申请的 scope
pub const SCOPE: &str = "inference:mint_agent_key";

/// 设备码授权类型（RFC 8628）
pub const GRANT_TYPE_DEVICE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// 续期授权类型
pub const GRANT_TYPE_REFRESH: &str = "refresh_token";

/// 铸 agent key 时要求的最小 TTL（秒）——上游给 ≥ 这个值的 key
pub const AGENT_KEY_MIN_TTL_SECONDS: i64 = 1800;

// ─── 路径 ────────────────────────────────────────────────────

/// 设备码申请
pub const DEVICE_CODE_PATH: &str = "/api/oauth/device/code";
/// 令牌端点（轮询与续期共用）
pub const TOKEN_PATH: &str = "/api/oauth/token";
/// agent key 铸造
pub const AGENT_KEY_PATH: &str = "/api/oauth/agent-key";
/// 匿名身份创建
pub const ANONYMOUS_CREATE_PATH: &str = "/api/anonymous/create";
/// 匿名令牌换取
pub const ANONYMOUS_TOKEN_PATH: &str = "/api/anonymous/token";

/// chat 端点后缀（拼在推理基址后）
pub const CHAT_PATH: &str = "/chat/completions";
/// 模型目录后缀
pub const MODELS_PATH: &str = "/models";

// ─── 官方 CLI 伪装常量 ───────────────────────────────────────

/// 官方 CLI 名（匿名创建的 UA 门禁认它）
pub const HERMES_AGENT_NAME: &str = "hermes-agent";
/// 官方 CLI 版本
pub const HERMES_AGENT_VERSION: &str = "1.0.0";
/// 运行面（匿名创建 UA 的第三段）
pub const HERMES_AGENT_SURFACE: &str = "cli";
/// 匿名令牌请求体里 `client.name`
pub const ANONYMOUS_CLIENT_NAME: &str = "hermes-agent";
/// 匿名令牌请求体里 `client.version`
pub const ANONYMOUS_CLIENT_VERSION: &str = "1.0.0";
/// 匿名令牌请求体里 `client.surface`
pub const ANONYMOUS_CLIENT_SURFACE: &str = "cli";
/// 匿名令牌请求体里 `client.capabilities`
pub const ANONYMOUS_CAPABILITIES: &[&str] = &["browser-v1"];

/// 当前运行平台（`linux` / `macos` / `windows` …）—— UA 与请求体共用
pub fn platform() -> &'static str {
    std::env::consts::OS
}

/// 当前 CPU 架构（`x86_64` / `aarch64` …）—— UA 用
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// 匿名创建的 User-Agent：`hermes-agent/<ver> (cli; <os>; <arch>)`（逐字形态，见模块头）
pub fn hermes_user_agent() -> String {
    format!(
        "{HERMES_AGENT_NAME}/{HERMES_AGENT_VERSION} ({HERMES_AGENT_SURFACE}; {}; {})",
        platform(),
        arch()
    )
}

/// Portal 基址（可由 `NOUS_PORTAL_BASE` 覆盖，便于自建 / 测试）
pub fn portal_base() -> String {
    env_override("NOUS_PORTAL_BASE").unwrap_or_else(|| PORTAL_BASE_URL.to_string())
}

/// 付费推理面默认基址（可由 `NOUS_INFERENCE_BASE` 覆盖）
pub fn default_inference_base() -> String {
    env_override("NOUS_INFERENCE_BASE").unwrap_or_else(|| DEFAULT_INFERENCE_BASE_URL.to_string())
}

/// 免费推理面基址（可由 `NOUS_WELCOME_BASE` 覆盖）
pub fn welcome_base() -> String {
    env_override("NOUS_WELCOME_BASE").unwrap_or_else(|| WELCOME_BASE_URL.to_string())
}

/// 读环境变量覆盖（空值视为未设置，去掉尾部斜杠）
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// Portal 全 URL（基址 + 路径）
pub fn portal_url(path: &str) -> String {
    format!("{}{path}", portal_base())
}

/// 推理基址 + 路径（基址可能自带 `/v1`，这里只去尾部斜杠再拼）
pub fn inference_url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim().trim_end_matches('/'))
}

/// 鉴权头（`Authorization: Bearer <token>`）
pub fn auth_headers(token: &str) -> Vec<(String, String)> {
    vec![(
        "Authorization".to_string(),
        format!("Bearer {}", token.trim()),
    )]
}

/// 匿名相关请求的公共头（UA 门禁 + JSON）
pub fn anonymous_headers() -> Vec<(String, String)> {
    vec![
        ("User-Agent".to_string(), hermes_user_agent()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ]
}

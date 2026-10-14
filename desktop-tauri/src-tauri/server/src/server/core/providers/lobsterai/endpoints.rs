//! LobsterAI（有道龙虾，NetEase Youdao）上游地址、协议版本与逐字请求头。
//!
//! ── 上游长什么样（逆向契约）────────────────────────────────────
//! ```text
//!   POST {base}/api/auth/exchange                授权码换令牌（无 Bearer）
//!   POST {base}/api/auth/refresh                 令牌续期
//!   POST {base}/api/proxy/v1/chat/completions    聊天（SSE-only，恒 stream=true）
//!   GET  {base}/api/models/available             模型目录（Bearer + 能力头）
//!   GET  {base}/api/client-activities/slot       签到槽位
//!   GET  {base}/api/client-activities/{code}/context   签到上下文
//!   POST {base}/api/client-activities/{code}/actions/check_in  签到领取
//!   GET  {base}/api/user/profile-summary         积分余额
//! ```
//! 基址默认 `https://lobsterai-server.youdao.com`，可经 `LOBSTERAI_API_BASE` 覆盖。
//!
//! ── 两个「版本」不是一回事（改之前先读这段）────────────────────
//!   · `PROTOCOL_VERSION`（`"0.1.0"`）：keyfrom 信封里的协议版本，恒定不变，
//!     进 exchange / refresh / models 三处的 `version` 字段；
//!   · **真实客户端版本**（`real_version()`）：上游按「像不像官方客户端」做风控，
//!     slot 接口的 `clientVersion` 用**过时的** `"0.1.0"` 会返回 `slotState=empty`
//!     （活动被隐藏）。真值从有道 Overmind 的更新接口拿（`REAL_VERSION_URL` 的
//!     `data.value.version`），缓存 1 小时，拿不到回退 [`APP_VERSION_FALLBACK`]。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。
//!
//! ── 与别家不同的一处：真实版本是**异步**拿的、但 chat 头是**同步**拼的 ──
//! `ProviderAdapter::build_chat_request` 是同步无参的，发不了网络请求；因此这里
//! 用一个进程级缓存（[`real_version`]）承载「最近一次拿到的真实版本」，由异步的
//! [`refresh_real_version`] 在转发前的凭证准备（`ensure_access_token`）与
//! 签到 / 余额查询里顺带刷新。缓存为空时一律回退 [`APP_VERSION_FALLBACK`]。

use std::sync::{Mutex, OnceLock};

use serde_json::Value;

/// 默认 API 基址（`LOBSTERAI_API_BASE` 可覆盖）
pub const DEFAULT_BASE_URL: &str = "https://lobsterai-server.youdao.com";

/// 默认登录门户基址（`LOBSTERAI_PORTAL_BASE` 可覆盖）。
///
/// ⚠️ 不确定性：门户域名来自契约里的 `{portal}/portal#/login`，上游未给出权威
/// 取值；这里取产品域名作缺省，实测对不上时用环境变量覆盖即可（授权地址由本
/// 模块本地拼，不写死在别处）。
pub const DEFAULT_PORTAL_BASE: &str = "https://lobsterai.youdao.com";

/// keyfrom 信封里的协议版本（恒定，见模块头）
pub const PROTOCOL_VERSION: &str = "0.1.0";

/// 真实客户端版本拉不到时的回退值（契约给定）
pub const APP_VERSION_FALLBACK: &str = "2026.9.4";

/// 真实客户端版本的更新接口（有道 Overmind）
pub const REAL_VERSION_URL: &str =
    "https://api-overmind.youdao.com/openapi/get/luna/hardware/lobsterai/prod/update";

/// 真实客户端版本的缓存有效期（契约：约 1 小时）
pub const VERSION_TTL_MS: i64 = 60 * 60 * 1000;

/// 客户端能力头（逐字，两个 capability 用逗号分隔）
pub const CLIENT_CAPABILITIES: &str = "kimi-k3-agentic-v1,thinking-level-control-v1";

/// 聊天 / 目录 / 签到请求的 `Accept`（SSE 优先）
pub const SSE_ACCEPT: &str = "text/event-stream, application/json";

// ─── 路径 ────────────────────────────────────────────────────
pub const EXCHANGE_PATH: &str = "/api/auth/exchange";
pub const REFRESH_PATH: &str = "/api/auth/refresh";
pub const CHAT_PATH: &str = "/api/proxy/v1/chat/completions";
pub const MODELS_PATH: &str = "/api/models/available";
pub const ACTIVITY_SLOT_PATH: &str = "/api/client-activities/slot";
pub const PROFILE_SUMMARY_PATH: &str = "/api/user/profile-summary";

/// 网页登录回调路径（落在本网关自己的 loopback 端口上）
pub const CALLBACK_PATH: &str = "/auth/callback";

/// 登录发起时上报的客户端标记（契约固定 `electron`）
pub const LOGIN_SOURCE: &str = "electron";

/// 读环境变量覆盖（空值视为未设置，去掉尾部斜杠）
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// API 基址
pub fn base_url() -> String {
    env_override("LOBSTERAI_API_BASE").unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// 登录门户基址
pub fn portal_base() -> String {
    env_override("LOBSTERAI_PORTAL_BASE").unwrap_or_else(|| DEFAULT_PORTAL_BASE.to_string())
}

/// 全 URL（含路径）
pub fn url(path: &str) -> String {
    format!("{}{path}", base_url())
}

/// 构造授权地址：`{portal}/portal#/login?source=electron&redirect_uri=..&state=..`。
///
/// 用 `form_urlencoded` 拼查询串（`redirect_uri` 里带 `:` / `/`、`state` 是 uuid，
/// 都必须正确百分号编码）。`#` 之后是 SPA 路由的查询串，所以整条是字符串拼接。
pub fn build_authorize_url(state: &str, redirect_uri: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("source", LOGIN_SOURCE)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .finish();
    format!("{}/portal#/login?{query}", portal_base())
}

// ─── loopback 回调端口（进程级常量，bootstrap 写一次）──────────

/// 回调端口。`ProviderAdapter::build_login_url()` 是同步无参的（trait 契约），
/// 拿不到 `ServerState`；端口在进程生命周期内不变，因此开机写一次、之后只读 ——
/// 与 accio / codearts / antigravity 的 `set_loopback_port` 同一手法。
static LOOPBACK_PORT: OnceLock<u16> = OnceLock::new();

/// 装入回调端口（`ServerState::bootstrap` 调用一次）。
pub fn set_loopback_port(port: u16) {
    let _ = LOOPBACK_PORT.set(port);
}

/// 本次登录的 `redirect_uri`（`http://127.0.0.1:{port}/auth/callback`）。
///
/// 端口未知（bootstrap 之前）时返回 None —— `build_login_url` 据此回 None，
/// 上层给「未能生成网页登录授权地址，请重试」的通用文案。
pub fn login_redirect_uri() -> Option<String> {
    LOOPBACK_PORT
        .get()
        .map(|port| format!("http://127.0.0.1:{port}{CALLBACK_PATH}"))
}

// ─── 逐字请求头 ──────────────────────────────────────────────

/// 鉴权头（`Authorization: Bearer <access_token>`）。
pub fn auth_headers(access_token: &str) -> Vec<(String, String)> {
    vec![(
        "Authorization".to_string(),
        format!("Bearer {}", access_token.trim()),
    )]
}

/// 官方客户端伪装头（聊天 / 目录 / 签到共用）。
///
/// `User-Agent` 与 `X-LobsterAI-Client-Version` 用**真实版本**（见模块头）——
/// 上游按它做风控，`"0.1.0"` 会被判成过时客户端。
pub fn capability_headers() -> Vec<(String, String)> {
    let version = real_version();
    vec![
        ("User-Agent".to_string(), format!("LobsterAI/{version}")),
        (
            "X-LobsterAI-Client-Capabilities".to_string(),
            CLIENT_CAPABILITIES.to_string(),
        ),
        ("X-LobsterAI-Client-Version".to_string(), version),
    ]
}

// ─── 真实客户端版本（进程级缓存 + best-effort 刷新）───────────

/// 版本缓存：`(version, fetched_at_ms)`。`None` = 从未拿到过。
static VERSION: OnceLock<Mutex<Option<(String, i64)>>> = OnceLock::new();

fn version_slot() -> &'static Mutex<Option<(String, i64)>> {
    VERSION.get_or_init(|| Mutex::new(None))
}

/// 当前已知的真实客户端版本（同步、永不失败）。缓存为空时回退 [`APP_VERSION_FALLBACK`]。
pub fn real_version() -> String {
    let guard = version_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .map(|(version, _)| version.clone())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| APP_VERSION_FALLBACK.to_string())
}

/// 缓存是否仍然新鲜（1 小时 TTL）。
fn version_fresh() -> bool {
    let guard = version_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.as_ref() {
        Some((_, fetched_at)) => {
            *fetched_at > 0 && crate::server::logging::now_ms() - *fetched_at < VERSION_TTL_MS
        }
        None => false,
    }
}

/// best-effort 刷新真实客户端版本（TTL 内早退）。
///
/// 失败只打一条 verbose 日志，绝不影响调用方 —— 版本拿不到时回退值仍然可用
/// （只是 slot 可能被判成过时而返回空活动）。
pub async fn refresh_real_version() {
    if version_fresh() {
        return;
    }
    let response = match crate::server::core::auth_http::send_raw(
        "GET",
        REAL_VERSION_URL,
        None,
        &[],
        None,
        Some(10_000),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            crate::server::logging::verbose(
                "[LobsterAI]",
                &format!("真实客户端版本拉取失败（沿用回退值）：{error}"),
            );
            return;
        }
    };
    let version = response
        .payload
        .as_ref()
        .and_then(|payload| payload.pointer("/data/value/version"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(version) = version else {
        crate::server::logging::verbose(
            "[LobsterAI]",
            "真实客户端版本响应里没有 data.value.version（沿用回退值）",
        );
        return;
    };
    let mut guard = version_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some((version.to_string(), crate::server::logging::now_ms()));
    crate::server::logging::verbose(
        "[LobsterAI]",
        &format!("真实客户端版本已刷新：{version}"),
    );
}

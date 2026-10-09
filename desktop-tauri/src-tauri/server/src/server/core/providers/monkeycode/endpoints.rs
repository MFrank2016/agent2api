//! MonkeyCode 上游端点与请求常量（cookie 名 / 路径 / 请求头）。
//!
//! ── 上游长什么样（逆向来源 `Acankao/MonkeyCodeReverseEngineer`）──────
//! 单体 Web 应用，网页与 API 同域（`https://monkeycode-ai.com`，
//! `mvp/config.py` 的 `BASE_URL`）；国际站在 `monkeycode-ai.net`（见
//! `region.rs` 的模块头：`.net` 站点存在，但参考未覆盖其端点的实测）。
//!
//! ```text
//!   认证    GET  /api/v1/users/status      → {"code":0,"data":{"user":{...}}}
//!   目录    GET  /api/v1/users/models      → {"code":0,"data":{"models":[...]}}
//!   任务    POST /api/v1/users/tasks       （下一步 WS 转发用）
//!   任务流  WS   /api/v1/users/tasks/stream?id=&mode=（下一步用）
//! ```
//!
//! ── cookie 名（参考 `mvp/config.py` + `docs/02-auth/03-login-methods.md`）──
//! 普通用户 `monkeycode_ai_session`，团队管理员 `monkeycode_ai_team_session`；
//! TTL 30 天、HttpOnly + Secure + SameSite=Lax。本网关只接**普通用户**这一路
//! （团队登录是另一套端点与另一套账号体系，不在本期范围）。
//!
//! ── 鉴权形态：粘贴式（**无验证码 / 无短信 / 无 OAuth 窗口**）──────────
//! 本家的密码登录要 go-cap 验证码、OAuth 要百智云 SCaptcha + 短信 —— 两条都
//! 不适合网关代跑。参考 `docs/02-auth/03-login-methods.md` 的结论：从浏览器
//! 复制 session cookie 是最省事、最可靠的一条路，因此本网关的登录入口就是
//! 「粘贴 session」+ 一次 `GET /users/status` 校验（见 `login.rs`）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use super::region::Region;

/// 普通用户的 session cookie 名（`mvp/config.py::SESSION_COOKIE_NAME`）
pub const SESSION_COOKIE_NAME: &str = "monkeycode_ai_session";

/// 团队管理员的 session cookie 名（参考里同源；本网关本期不接，仅登记以免
/// 将来有人把团队 cookie 当普通 cookie 用）
pub const TEAM_SESSION_COOKIE_NAME: &str = "monkeycode_ai_team_session";

/// 登录状态校验路径（`mvp/auth.py::check_status`，也是 `docs/05-api` 认证端点表里的那条）
pub const USER_STATUS_PATH: &str = "/api/v1/users/status";

/// 模型目录路径（`mvp/models.py::list_models`，`GET {base}/api/v1/users/models`）
pub const MODELS_PATH: &str = "/api/v1/users/models";

/// 任务列表路径（`discoverImageId` 用：从已有任务里取 `image.id`）
pub const TASKS_PATH: &str = "/api/v1/users/tasks";

/// 任务列表的查询串：`discoverImageId` 取最近 5 条任务（见 `login.rs`）
pub const TASKS_DISCOVER_QUERY: &str = "?page=1&size=5";

/// 任务创建路径（**下一步** WS 转发用；本步只登记常量）
pub const TASK_CREATE_PATH: &str = "/api/v1/users/tasks";

/// 任务流 WebSocket 路径（**下一步**用：`?id=<taskId>&mode=new|attach`）
pub const TASK_STREAM_PATH: &str = "/api/v1/users/tasks/stream";

/// session 有效期（`docs/02-auth/03-login-methods.md`：30 天硬限制，**不可续期**）
pub const SESSION_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

/// 出站 User-Agent（照抄客户端/参考实现的形态；上游对陌生 UA 会加风控）
pub const USER_AGENT: &str = "monkeycode-local-proxy";

/// `Cookie: monkeycode_ai_session=…` 请求头值
pub fn cookie_header(session: &str) -> String {
    format!("{SESSION_COOKIE_NAME}={session}")
}

/// 站内 JSON 请求的公共头：`Origin` / `Referer` 伪装成站点自身的前端请求
/// （参考 `mkHeaders`），加一个可辨识的 UA。
pub fn json_headers(region: Region) -> Vec<(String, String)> {
    let origin = region.site_origin();
    vec![
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("Origin".to_string(), origin.to_string()),
        ("Referer".to_string(), format!("{origin}/")),
        ("Accept".to_string(), "application/json".to_string()),
    ]
}

/// 在公共头后追加鉴权 cookie（调用方还会传 body 时由 `send_raw` 自动补
/// `Content-Type`，这里不重复加）。
pub fn authed_headers(region: Region, session: &str) -> Vec<(String, String)> {
    let mut headers = json_headers(region);
    headers.push(("Cookie".to_string(), cookie_header(session)));
    headers
}

/// 状态校验的完整 URL
pub fn status_url(region: Region) -> String {
    format!("{}{USER_STATUS_PATH}", region.base_url())
}

/// 模型目录的完整 URL
pub fn models_url(region: Region) -> String {
    format!("{}{MODELS_PATH}", region.base_url())
}

/// 任务列表（用于发现 image_id）的完整 URL
pub fn tasks_discover_url(region: Region) -> String {
    format!("{}{TASKS_PATH}{TASKS_DISCOVER_QUERY}", region.base_url())
}

/// 接口类型 → 上游 CLI / Agent 名（`proxy/src/task-runner.ts` 的映射，逐字核实）。
///
/// 三种 `interface_type` 决定容器里装哪个 coding agent（`docs/03-llm/02-interface-types.md`）：
///   - `openai_chat`      → `opencode`（通用 OpenAI 兼容）
///   - `openai_responses` → `codex`
///   - `anthropic`        → `claude`
///
/// 未知 / 缺失的接口类型回落 `opencode`（与参考 `task-runner.ts` 的兜底一致）
/// —— 那是「最通用」的一档，不是随意的默认值。
pub fn cli_name_for(interface_type: &str) -> &'static str {
    match interface_type.trim().to_ascii_lowercase().as_str() {
        "openai_responses" => "codex",
        "anthropic" => "claude",
        _ => "opencode",
    }
}

//! LobsterAI 浏览器 OAuth 登录任务：与通用 `start / wait / cancel` 状态表的衔接。
//!
//! ── 这条链长什么样 ──────────────────────────────────────────
//! 与 Antigravity / Accio 同款：**授权地址由适配器本地拼**（`build_login_url`，
//! 见 `providers::lobsterai::endpoints::build_authorize_url`），用户完成登录后浏览器
//! 302 回**本网关自己的 loopback 端口** `/auth/callback`（public 路由，见 `http.rs`），
//! 由 `api::session::login_lobsterai_callback` 把 `code` / `state` 转交给
//! `LoginService::submit_login_callback` → 适配器 `exchange_login_code` 换凭证落账号。
//!
//! 因此这里**没有后台轮询任务**：authUrl 与 state 都在发起时本地就有（不像
//! workbuddy / ZCode 那样要问上游），任务句柄登记进表后等回调来收尾即可 —— 与
//! 小浣熊的 `start_web_login` 完全同款，直接复用它。
//!
//! ── 为什么单独一个入口（而不是让通用分支兜底）────────────────
//! `login_start` 的通用分支（`start_web_login`）本就能服务本家，但显式给一个
//! 命名入口让「LobsterAI 走的是浏览器 OAuth」在调用点一眼可见，也便于将来本家
//! 若需要额外的本地准备（如设备指纹）时有一个现成的落点。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use crate::server::core::providers::ProviderKind;

use super::{LoginService, LoginTaskHandle};

impl LoginService {
    /// 发起一次 LobsterAI 浏览器 OAuth 登录。
    ///
    /// 复用通用的网页登录机制：`build_login_url()` 现拼授权地址与 state，任务句柄
    /// 登记进表后等 `submit_login_callback` 收尾（或等 `cancel`）。
    pub fn start_lobsterai_login(&self) -> Result<LoginTaskHandle, String> {
        self.start_web_login(ProviderKind::LobsterAi)
    }
}

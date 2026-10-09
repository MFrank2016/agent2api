//! MonkeyCode 适配器（两个地区共用一套实现，按 `Region` 参数化）。
//!
//! ── 为什么走会话式转发（`is_stateful`）────────────────────────
//! MonkeyCode 的上游不是「一次 HTTP 请求 = 一次回答」的 OpenAI 协议：对话要
//! **先建任务**（`POST /api/v1/users/tasks`，带 `image_id` / `model_id` /
//! `cli_name`），再挂 **WebSocket** 任务流
//! （`GET /api/v1/users/tasks/stream?id=&mode=`）读 ACP 事件
//! （见 `Acankao/.../docs/04-websocket/*`）。单请求构造容纳不了这条链，
//! 因此 `is_stateful()` 为 true —— 与 Kuku / CatPaw / Qoder 同一处境
//! （「一次发送要适配器自己完成」）。
//!
//! ── 本步（骨架）的边界 ──────────────────────────────────────
//! 账号管理（粘贴 session 登录 + 公开形态）、模型目录（`GET /users/models`）
//! 已接通；**会话转发留空** —— [`Self::forward_conversation`] 返回一个明确的
//! `GatewayError`，下一步在 `chat.rs` 里接上「建任务 → WS 流 → ACP 翻译」。
//! 本步不引入任何 WebSocket 依赖（`Cargo.toml` 不动，那是下一步的事）。
//!
//! ── 本家没有的东西（如实声明，别照抄别家）──────────────────────
//!   - **没有续期**：session 30 天硬限制，上游无 refresh 接口 →
//!     `supports_refresh = false`（与 Loomy 同一处境）；
//!   - **没有网页登录**：登录要么带验证码、要么要 OAuth 窗口，本网关只接
//!     「粘贴 session」这一条 → `supports_web_login` 保持默认 false；
//!   - **本步不做签到与余额**：`supports_usage` 保持默认 false，
//!     `query_usage` 走 trait 默认实现。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::endpoints::cli_name_for;
use super::region::Region;
use super::{credentials, models};

/// MonkeyCode 适配器：**按地区参数化**（与 Qoder / AutoClaw 同款）。
///
/// 拆家后两个地区是两家 provider（`monkeycode` 国内版 / `monkeycode-intl`
/// 国际版），两个静态实例由 `adapter_for` 按 kind 给出；地区 → 身份的互查在
/// `region::Region`。适配器内部链路（凭证、目录、下一步的转发）一律以本字段
/// 的 `region` 为准。
pub struct MonkeyCodeAdapter {
    region: Region,
}

/// 国内版静态实例
pub static MONKEYCODE_ADAPTER: MonkeyCodeAdapter =
    MonkeyCodeAdapter { region: Region::Cn };
/// 国际版静态实例
pub static MONKEYCODE_INTL_ADAPTER: MonkeyCodeAdapter =
    MonkeyCodeAdapter { region: Region::Intl };

impl ProviderAdapter for MonkeyCodeAdapter {
    fn kind(&self) -> ProviderKind {
        self.region.kind()
    }

    /// 上游是「建任务 → WebSocket 任务流」的多步会话（见模块头）。
    fn is_stateful(&self) -> bool {
        true
    }

    /// 模型清单来自远程目录（`models.rs` 的进程缓存 + 落盘缓存，按地区分格）
    fn list_models(&self) -> Vec<Value> {
        models::list(self.region)
    }

    /// **防御性报错**：本家走会话式转发，不走单请求路径。
    ///
    /// 与 Kuku 同一处置：编排层已按 `is_stateful` 分流，走到这里说明是内部
    /// 契约错误，报错比「构造出一个半成品请求发出去」安全。
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            503,
            format!(
                "MonkeyCode {} 走会话式转发，不走单请求路径（内部错误：编排层未按 is_stateful 分流）",
                self.region.label()
            ),
        ))
    }

    /// 上游错误分类：
    ///   - HTTP 401 / 403 → `TokenExpired`（登录态失效，重新粘贴 session）；
    ///   - HTTP 429 → `QuotaLimited`（上游不给结构化恢复时间）；
    ///   - 其余 → `Fatal`（本家的错误在转发链路里已归一到 `GatewayError`，
    ///     正常路径走不到这里 —— 与 Kuku 同一核对结论）。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let message = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| format!("上游返回 {status}: {text}"))
            .unwrap_or_else(|| format!("上游返回 HTTP {status}"));
        if status == 401 || status == 403 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: error_body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        UpstreamErrorClass::Fatal {
            status,
            message,
            upstream_code: error_body.get("code").and_then(Value::as_i64),
        }
    }

    /// 取可用 session。本家**没有续期**：只做「记录里有没有 session」的检查，
    /// 临期与否只影响账号页的提示（`credentials_expiring`），不影响转发 ——
    /// 上游拒了就是 401，编排层把它归成 `TokenExpired` 并如实报给客户端。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let record = store.monkeycode_account_record(self.region, account_id);
            if !account_id.is_empty() && record.is_none() {
                return Err(GatewayError::with_status(
                    401,
                    format!(
                        "MonkeyCode {} 账号 {account_id} 不存在（请重新添加）",
                        self.region.label()
                    ),
                ));
            }
            let credentials = credentials::from_record(record.as_ref())?;
            if credentials.expiring() {
                logging::verbose(
                    "[MonkeyCode]",
                    "账号凭证已过期或临期（会话 30 天、无续期接口），如遇 401 请重新粘贴 session",
                );
            }
            Ok(credentials.session)
        })
    }

    /// 没有续期手段（上游没有 refresh 接口）——维护任务不问本家
    fn supports_refresh(&self) -> bool {
        false
    }

    /// 临期判定：取记录 → 凭证的 `expiring()`（登录时间 + 30 天，提前 1 天）
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.is_empty() {
            return false;
        }
        match store.monkeycode_account_record(self.region, account_id) {
            Some(record) => credentials::from_record(Some(&record))
                .map(|credentials| credentials.expiring())
                .unwrap_or(false),
            None => false,
        }
    }

    /// 有远程目录（`GET /api/v1/users/models`，见 `models.rs`）
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷模型目录：用指定账号（空串 = 本地区组内第一个可用账号）的 session。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`（与 Loomy / AutoClaw 同一处置：脚本 / CI 不该看到一个
    /// 红色失败）。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let record = store.monkeycode_account_record(self.region, account_id);
            if record.is_none() {
                if account_id.is_empty() {
                    logging::verbose(
                        "[Models]",
                        &format!("MonkeyCode {} 模型目录刷新跳过：尚未添加账号", self.region.label()),
                    );
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的账号不存在或不可用，请重新选择");
            }
            let credentials = match credentials::from_record(record.as_ref()) {
                Ok(credentials) => credentials,
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            models::refresh(self.region, &credentials.session, force).await
        })
    }

    /// **会话式转发入口（本步留空）**。
    ///
    /// 下一步在这里接上：`credentials::from_record` → 校验 `image_id`（缺了报
    /// 可读错误）→ `POST /api/v1/users/tasks` 建任务 → 连
    /// `wss://…/api/v1/users/tasks/stream` → 把 ACP 事件翻成 OpenAI chunk，
    /// 产出与无状态路径同形的 `ForwardOutcome`。WebSocket 依赖（`tokio-tungstenite`
    /// 一类的选择）也在下一步加进 `Cargo.toml`。
    fn forward_conversation<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _body: &'a Value,
        _client_headers: &'a HeaderMap,
        _proxy: Option<crate::server::core::proxies::ResolvedProxy>,
        _stream: bool,
        _telemetry: &'a std::sync::Arc<crate::server::core::upstream::usage::RequestTelemetry>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::server::core::upstream::ForwardOutcome, GatewayError>,
                > + Send
                + 'a,
        >,
    > {
        let label = self.region.label();
        Box::pin(async move {
            Err(GatewayError::with_status(
                501,
                format!("MonkeyCode {label} 的会话转发尚未接通"),
            ))
        })
    }
}

/// 接口类型 → CLI 名的转发口（供下一步与排障复用；判据唯一写在 `endpoints`）
pub fn cli_name(interface_type: &str) -> &'static str {
    cli_name_for(interface_type)
}

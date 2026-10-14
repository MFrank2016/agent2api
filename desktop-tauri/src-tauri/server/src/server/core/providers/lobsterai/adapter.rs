//! LobsterAI 适配器：**无状态** SSE 转发 + 令牌续期 + 目录 + 签到 + 余额 + 网页登录。
//!
//! ── 为什么是无状态 ──────────────────────────────────────────
//! 上游就是 OpenAI 兼容的 `POST /api/proxy/v1/chat/completions`（SSE），一次请求
//! 一次回答，没有会话闸门或多步协议。所以 `is_stateful` 走默认 false，转发链只需
//! `build_chat_request`；账号轮换、限额冷却、退避重试、usage 与取消全部留在编排层。
//!
//! ── 三处与别家不同、值得先知道的事实 ────────────────────────
//!   1. **上游 SSE-only**：`stream:false` 会被上游 500。因此 `build_chat_request`
//!      **强制** `stream=true`（非流式客户端由编排层聚合，与 OfficeAce 同款）；
//!   2. **`tool_choice` 的空档要删掉**：`"" | "none" | null` 会让上游报错，构造时
//!      一并删除（`none` 与缺省同义）；
//!   3. **真实版本伪装**：`User-Agent` / `X-LobsterAI-Client-Version` 用从有道
//!      Overmind 拿的真实版本（见 `endpoints` 模块头），过时的 `"0.1.0"` 会被风控。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use crate::server::core::providers::{content_block, ProviderKind};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{balance, credentials, endpoints, login, models, refresh};

/// LobsterAI 适配器（无状态单例；凭证在账号存储、目录在 `models.rs` 的进程级句柄）
pub struct LobsterAiAdapter;

/// 静态单例（`adapter_for(ProviderKind::LobsterAi)` 返回这一个）
pub static LOBSTERAI_ADAPTER: LobsterAiAdapter = LobsterAiAdapter;

/// 上游业务码：积分不足（HTTP 200 的流内错误也会带它）
const CODE_INSUFFICIENT_CREDITS: i64 = 40201;
/// 上游业务码：未知模型
const CODE_UNKNOWN_MODEL: i64 = 40300;

/// 从会话里取转发用的 access token（公开形态里只有尾号，拿它拼不出 Authorization）
fn session_access_token(account: &Value) -> String {
    account
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .or_else(|| account.get("accessToken").and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// `tool_choice` 是否需要删除（`"" | "none" | null` 上游不认，见模块头）
fn tool_choice_is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => {
            let trimmed = text.trim();
            trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none")
        }
        _ => false,
    }
}

impl ProviderAdapter for LobsterAiAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::LobsterAi
    }

    /// 模型清单来自远程目录（`models.rs` 的进程缓存 + 落盘缓存）
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {base}/api/proxy/v1/chat/completions`。
    ///
    /// 上游恒流式（SSE-only）：强制 `stream=true`，并删掉上游不认的空 `tool_choice`
    /// （见模块头）。`Authorization` 从会话的 `auth.accessToken` 取。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let access_token = session_access_token(account);
        if access_token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "LobsterAI 账号缺少 access token，无法转发（请重新登录或重新粘贴令牌）",
            ));
        }
        let mut headers: Vec<(String, String)> = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        headers.extend(endpoints::capability_headers());
        headers.push(("Accept".to_string(), endpoints::SSE_ACCEPT.to_string()));

        let mut out = body.clone();
        if let Some(object) = out.as_object_mut() {
            object.insert("stream".to_string(), Value::Bool(true));
            if object.get("tool_choice").is_some_and(tool_choice_is_empty) {
                object.remove("tool_choice");
            }
        }
        Ok(ChatRequestPlan::chat(
            endpoints::url(endpoints::CHAT_PATH),
            headers,
            out,
        ))
    }

    /// 上游错误分类：
    ///   - 业务码 `40201`（积分不足）→ `QuotaLimited`（下游按 429，换账号更有用）；
    ///   - 业务码 `40300`（未知模型）→ `Fatal(404)`（不换家、不冷却）；
    ///   - 400 / 422 → 内容拦截判定（`content_block`，命中则换中性提示词补救）；
    ///   - 401 / 403 → `TokenExpired`（刷新一次再重试）；
    ///   - 402 / 429 → `QuotaLimited`；404 → `Fatal(404)`；503 → `Fatal(503)`；
    ///   - 其余 → `Fatal(502)`。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = error_body.get("code").and_then(Value::as_i64);
        // 业务码优先：HTTP 200 的体内容错误也走这里（见 `is_error_response`）
        match code {
            Some(CODE_INSUFFICIENT_CREDITS) => {
                return UpstreamErrorClass::QuotaLimited {
                    reset_at: None,
                    message: format!(
                        "{message}（LobsterAI 积分不足：请充值或换一个账号，本网关按限流处理）"
                    ),
                    upstream_code: code,
                    status: 429,
                }
            }
            Some(CODE_UNKNOWN_MODEL) => {
                return UpstreamErrorClass::Fatal {
                    status: 404,
                    message: format!("{message}（上游不认这个模型）"),
                    upstream_code: code,
                }
            }
            _ => {}
        }
        match status {
            400 | 422 => content_block::classify_or_fatal(400, error_body, message, code),
            401 | 403 => UpstreamErrorClass::TokenExpired { message },
            402 => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("{message}（额度 / 套餐不足，本网关按限流处理）"),
                upstream_code: code,
                status: 429,
            },
            429 => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            },
            404 => UpstreamErrorClass::Fatal {
                status: 404,
                message,
                upstream_code: code,
            },
            503 => UpstreamErrorClass::Fatal {
                status: 503,
                message,
                upstream_code: code,
            },
            _ => UpstreamErrorClass::Fatal {
                status: 502,
                message,
                upstream_code: code,
            },
        }
    }

    /// 上游可能用 **HTTP 200 + `application/json`** 返回业务错误（如积分不足 /
    /// 未知模型）—— 与 ZCode 同款判据：非 2xx，或 200 且 content-type 是 JSON
    /// （不是 SSE）时，送入既有错误读取、分类与账号轮换流程。
    fn is_error_response(&self, status: u16, headers: &HeaderMap) -> bool {
        if !(200..300).contains(&status) {
            return true;
        }
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    }

    /// 取可用 access token（临期或缺失时自动续期并回写）。
    ///
    /// 顺带 best-effort 刷新真实客户端版本（chat 头是同步拼的，读的是进程级缓存，
    /// 见 `endpoints` 模块头）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            endpoints::refresh_real_version().await;
            let cred = refresh::ensure_fresh(store, account_id, false).await?;
            if cred.access_token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "LobsterAI 账号没有可用的 access token，请重新登录",
                ));
            }
            Ok(cred.access_token)
        })
    }

    /// 401 后的**强制**刷新（不看临期窗口）。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let cred = refresh::ensure_fresh(store, account_id, true).await?;
            Ok(cred.access_token)
        })
    }

    /// 有续期手段（refresh token + `POST /api/auth/refresh`）
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 该账号的凭证是否临期（没有 refresh token 的账号返回 false）
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        let Some(record) = store.lobsterai_account_record(account_id) else {
            return false;
        };
        match credentials::from_record(Some(&record)) {
            Ok(cred) if cred.can_refresh() => cred.expiring(),
            _ => false,
        }
    }

    /// 有远程目录（`GET /api/models/available`）
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷模型目录：用指定账号（空串 = 队首可用账号）的凭证打上游。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let record = store.lobsterai_account_record(account_id);
            if record.is_none() {
                if account_id.trim().is_empty() {
                    logging::verbose("[Models]", "LobsterAI 模型目录刷新跳过：尚未添加账号");
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的账号不存在或不可用，请重新选择");
            }
            let cred = match credentials::from_record(record.as_ref()) {
                Ok(cred) => cred,
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            models::refresh(&cred, force).await
        })
    }

    /// 有余额查询能力（`GET /api/user/profile-summary`）
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查积分余额（`balance::query_usage`）。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        Box::pin(balance::query_usage(store, account_id))
    }

    /// 支持网页登录（浏览器 OAuth 授权码 + loopback 回调，见 `login.rs`）
    fn supports_web_login(&self) -> bool {
        true
    }

    /// 授权地址 + state（`login::build_authorize_url`）。
    ///
    /// 返回 None = 回调端口还没写进进程级常量（bootstrap 之前）。
    fn build_login_url(&self) -> Option<(String, String)> {
        let state = login::new_login_state();
        let redirect_uri = endpoints::login_redirect_uri()?;
        let url = endpoints::build_authorize_url(&state, &redirect_uri);
        Some((url, state))
    }

    /// 用回调里的一次性 `code` 换令牌并落账号（`login::exchange`）。
    ///
    /// `state` 已由调用方逐字比对过；实现里再做一次非空校验作为深度防御。
    fn exchange_login_code<'a>(
        &'a self,
        store: &'a AccountStore,
        code: &'a str,
        state: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { login::exchange(store, code, state).await })
    }
}


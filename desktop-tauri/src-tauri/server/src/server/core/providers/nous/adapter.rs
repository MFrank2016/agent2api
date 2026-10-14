//! Nous 适配器：**无状态** OpenAI 兼容转发 + Portal 设备码登录 + 匿名免费档。
//!
//! ── 转发的落点 ──────────────────────────────────────────────
//! `build_chat_request` 一条路：`POST {base}/chat/completions`，`Bearer` 凭证。
//! `base` 由凭证决定（账号 = 付费面 `inference-api`；匿名 = 免费面 `welcome-api`，
//! 见 `credentials::from_record`），响应就是标准 chat SSE —— 不需要翻译层，
//! 账号轮换 / 冷却 / 重试 / usage / 取消全部留在编排层。
//!
//! ── 匿名免费档怎么接（`allows_anonymous_default_session`）──────
//! 本家覆写 [`ProviderAdapter::allows_anonymous_default_session`] 为 true：
//! 「一个账号都没有」时，编排层会调 [`Self::ensure_access_token`] 拿一份默认
//! 凭证（本家给的是**匿名 JWT**），现造一个最小会话
//! （`{auth:{accessToken:<JWT>}}`）继续转发。适配器在 `build_chat_request` 里
//! 按会话形态分辨账号 / 匿名（见 `credentials::from_record`），匿名自动改打
//! 免费面。这与官方 `hermes-agent` 的「无账号即免费档」行为一致。
//!
//! ── 环境变量旁路（`env_credentials_present`）──────────────────
//! 本家**没有**环境变量凭证；但「匿名免费档恒可用」意味着这家**永远有一条可
//! 用的默认登录态**，因此 `env_credentials_present` 返回 true —— 聚合目录据此
//! 把本家算作「现在可用」（`catalog::provider_available`），`nous/welcome`
//! 因此无需添加账号就会出现在 `/v1/models` 里。这是刻意的：匿名免费档的存在
//! 让「有没有登录态」对这个问题的答案是「有」。
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

use super::{anonymous, credentials, endpoints, models, oauth};

/// Nous 适配器（无状态单例；凭证在账号存储、目录在 `models.rs` 的进程级句柄）
pub struct NousAdapter;

/// 静态单例（`adapter_for(ProviderKind::Nous)` 返回这一个）
pub static NOUS_ADAPTER: NousAdapter = NousAdapter;

impl ProviderAdapter for NousAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Nous
    }

    /// 模型清单来自远程目录（`models.rs` 的进程缓存 + 落盘缓存 + 内置兜底）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {base}/chat/completions`。
    ///
    /// `base` 与 Bearer 凭证都取自会话：账号形态是 agent key + 付费面；
    /// 匿名形态是匿名 JWT + 免费面（见 `credentials::from_record`）。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credentials::from_record(Some(account))?;
        let url = endpoints::inference_url(&credential.base_url, endpoints::CHAT_PATH);
        let mut headers = endpoints::auth_headers(&credential.token);
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        headers.push(("Accept".to_string(), "text/event-stream".to_string()));
        // 发送体原样透传（OpenAI 兼容）；model 已由编排层按目录改写
        Ok(ChatRequestPlan::chat(url, headers, body.clone()))
    }

    /// 上游错误分类（四档口径）：
    ///   - 400 → 内容拦截判定（`content_block`，命中则换中性提示词补救）；
    ///   - 401 / 403 / 404 → `TokenExpired`（刷新一次再重试：账号换新 agent key，
    ///     匿名重换 JWT；404 也归这一档，因为匿名身份失效时免费面回 404）；
    ///   - 402 → `QuotaLimited`（下游按 429：额度不足换账号更有用）；
    ///   - 429 → `QuotaLimited`（含免费面对非 `nous/welcome` 模型的 `model_not_free`）；
    ///   - 503 → `Fatal(503)`；其余 → `Fatal(502)`。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = error_body.get("code").and_then(Value::as_i64);
        match status {
            400 => content_block::classify_or_fatal(400, error_body, message, code),
            401 | 403 | 404 => UpstreamErrorClass::TokenExpired { message },
            402 => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("{message}（Nous 额度 / 套餐不足，本网关按限流处理）"),
                upstream_code: code,
                status: 429,
            },
            429 => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
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

    /// 取可用凭证：
    ///   - 空 `account_id`（无账号）→ 匿名免费档 JWT；
    ///   - 非空 → 账号的 agent key（临期或缺失时自动续期 / 重铸并回写）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            if account_id.trim().is_empty() {
                return anonymous::ensure_token(false).await;
            }
            let credentials = oauth::ensure_fresh(store, account_id, false).await?;
            if credentials.token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "Nous 账号没有可用的推理凭证（agent key），请重新登录",
                ));
            }
            Ok(credentials.token)
        })
    }

    /// 401 后的**强制**刷新：账号重铸 agent key，匿名重换 JWT。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            if account_id.trim().is_empty() {
                return anonymous::ensure_token(true).await;
            }
            let credentials = oauth::ensure_fresh(store, account_id, true).await?;
            Ok(credentials.token)
        })
    }

    /// 有续期手段（Portal refresh token + agent key 重铸）。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 该账号的凭证是否临期（Portal access token 或 agent key 任一临期）。
    ///
    /// 没有 refresh token 的账号返回 false：它连续期手段都没有，交给维护只会
    /// 变成每轮一条稳定失败的记录（与 Antigravity / Trae 同一处置）。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        let Some(record) = store.nous_account_record(account_id) else {
            return false;
        };
        match credentials::from_record(Some(&record)) {
            Ok(credentials) if credentials.can_refresh() => credentials.expiring(),
            _ => false,
        }
    }

    /// 有远程目录（`GET {base}/models`）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷模型目录：用指定账号（空串 = 队首可用账号）的凭证打上游。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，返回
    /// `unchanged()`（与 Command Code / Loomy 同一处置）。匿名档只有
    /// `nous/welcome`，也不远程刷（`models::refresh_from_record` 里已早退）。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let record = store.nous_account_record(account_id);
            if record.is_none() {
                if account_id.trim().is_empty() {
                    logging::verbose("[Models]", "Nous 模型目录刷新跳过：尚未添加账号");
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的账号不存在或不可用，请重新选择");
            }
            models::refresh_from_record(record.as_ref(), force).await
        })
    }

    /// **允许匿名默认会话**：没有账号时用匿名免费档转发（见模块头）。
    fn allows_anonymous_default_session(&self) -> bool {
        true
    }

    /// 匿名免费档恒可用 ⇒ 本家「现在算有可用登录态」（见模块头）。
    fn env_credentials_present(&self) -> bool {
        true
    }
}

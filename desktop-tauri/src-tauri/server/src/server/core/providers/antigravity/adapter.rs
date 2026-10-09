//! Antigravity 的 `ProviderAdapter` 实现（**本步只接账号 / 刷新 / 模型目录**）。
//!
//! ── 本步的边界（读这段再动这个文件）────────────────────────────
//! 本步交付的是**骨架**：账号可添加、token 可刷新、模型目录可刷、注册接线全通；
//! **聊天转发未接通** —— [`AntigravityAdapter::build_chat_request`] 返回一条明确的
//! 501（而不是构造一个半成品请求发出去）。
//!
//! ── 为什么 `is_stateful` 保持默认 false（下一步的路线在此定）────
//! 上游是**无状态 HTTP**（一次 `POST …:streamGenerateContent?alt=sse` = 一次生成），
//! 只是「响应帧不是 OpenAI 方言」：SSE 每帧是 v1internal 的信封
//! （`data: {"response":{…gemini 响应…}}`，规格 §4.2），且字段路径、思考位、
//! 工具调用全按 Gemini 的形状给。本仓对这类上游的既定解法是
//! [`UpstreamResponse`](crate::server::core::providers::adapter::UpstreamResponse)
//! **加一个变体 + 翻译层**（Command Code 的 NDJSON、ZCode 的 Anthropic 都是这条
//! 路线），于是账号轮换、限额冷却、退避重试、usage 记账与取消处理全部留在编排层。
//! 改成 `is_stateful = true` + `forward_conversation` 会把那五样在适配器里重写
//! 一遍，而本家并没有多步会话协议 —— 没有理由付那份代价。
//!
//! ── 本家没有的东西（如实声明，别照抄别家）──────────────────────
//!   - **没有网页登录**（`supports_web_login` 保持默认 false）：本步只做粘贴式
//!     （评估见 `mod.rs` 的模块头）；
//!   - **没有签到**（`core::auto_checkin` 的清单不含本家：Antigravity 没有可自动
//!     领取的奖励活动）；
//!   - **没有余额查询**（`supports_usage` 保持默认 false）：额度是「每模型剩余
//!     比例」（`fetchAvailableModels` 的 `quotaInfo.remainingFraction`，随模型
//!     条目一起进目录缓存），不是账号级余额 —— 界面上要展示时走模型条目里的
//!     `quota` 键，不需要一条独立的余额链路；
//!   - **不发 `x-goog-api-client`**、不采集设备指纹（见 `endpoints.rs` 的模块头）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, RetryAdvice, UpstreamErrorClass,
};
use crate::server::core::providers::{content_block, ProviderKind};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{credentials, models, oauth, project};

/// Antigravity 适配器（无状态单例；身份全在账号记录里）
pub struct AntigravityAdapter;

/// 静态单例（`adapter_for(ProviderKind::Antigravity)` 返回这一个）
pub static ANTIGRAVITY_ADAPTER: AntigravityAdapter = AntigravityAdapter;

impl ProviderAdapter for AntigravityAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Antigravity
    }

    /// 模型清单来自远程目录（`models.rs`：进程缓存 + 落盘缓存 + 内置兜底）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// **本步未接通**：聊天转发留待下一步（信封 + `UpstreamResponse` 新变体 +
    /// Gemini SSE 翻译层，见模块头）。
    ///
    /// 返回 501（Not Implemented）而不是 503：503 在本仓的语义是「上游暂时不可用、
    /// 可以换账号重试」，501 才是「这个能力还没实现」—— 用户与排障者一眼能分清
    /// 「本家坏了」与「本家还没接」。
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            501,
            "Antigravity 的会话转发尚未接通（本步只接入账号、token 刷新与模型目录）：\
             请先用其它提供商转发，或等待下一步接入",
        ))
    }

    /// **本步如实声明「还没有转发能力」**（`false`）——下一步接通转发后改回 true。
    ///
    /// ── 为什么这一个是 false（而 `is_stateful` 那一个是「保持默认」）──
    /// `is_stateful=false` 说的是「上游是无状态 HTTP」（协议事实，下一步也不变）；
    /// 本方法说的是「这条账号现在**能不能承接请求**」（能力事实，本步就是不能）。
    /// trait 的文档把这一位留给的正是本家现在这种过渡态：「先上账号管理、后接
    /// 转发的 provider 仍需要它」（Qoder 接推理协议之前就是 false）。
    ///
    /// 两个消费方，两个后果，都是我们想要的：
    ///   1. `account_store::pick_current`（全局队首）：排进队首会让顶栏把这条账号
    ///      显示成「当前登录态」，而「退出登录」按队首**删除账号** —— 一条还不能
    ///      转发的账号被当成登录态删掉，是本步最不该发生的事；
    ///   2. 公开形态的 `chatSupported`：界面据此如实说明「这家还没接通转发」，
    ///      而不是让用户以为加了账号就能用。
    ///
    /// **不影响的**：账号添加、token 刷新、模型目录刷新与 `/v1/models` 的广告
    /// （那几处的判据是「有没有可用凭证 / 清单是否非空」，与这一位无关）——
    /// 于是本步交付的四件事照常成立，而「点名一个 Antigravity 模型」会在转发层
    /// 得到 `build_chat_request` 那条明确的 501。
    ///
    /// **下一步的 TODO**：`build_chat_request` 接上真身的同时，把这一位改回
    /// `true`（两件事必须同一步改，否则界面会继续说「未接通」或反过来）。
    fn supports_chat(&self) -> bool {
        false
    }

    /// 上游错误分类（规格 §7 的映射表 + 本仓的四档口径）。
    ///
    /// ── 判据为什么先看文案再看状态码 ────────────────────────────
    /// Google 把业务语义放在 `error.status` 里（`PERMISSION_DENIED` /
    /// `RESOURCE_EXHAUSTED` / `UNAVAILABLE`），HTTP 状态只是它的投影：
    ///   - `RESOURCE_EXHAUSTED` → 限额（**即使状态码不是 429** —— 规格坑 #11 记
    ///     载了「官方客户端在 agent 路径撞无细节 429」这一现象，文案才是可靠信号）；
    ///   - `PERMISSION_DENIED` → 403，本家的含义是「这个账号没有资格 / 未开通」
    ///     （规格 §6：受限地区可能 403）—— 归 `TokenExpired`：编排层会先刷一次
    ///     凭证、再按队列换下一个账号，那正是这种账号该走的路；
    ///   - 其余 401 / 403 → `TokenExpired`（凭证被拒）；
    ///   - 429 → `QuotaLimited`；400 档交给共用的内容拦截判定；
    ///   - 408 / 5xx → `Fatal` + [`Self::retry_advice`] 的原地退避（可重试）；
    ///   - 其余 → `Fatal`（原样透出）。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = error_body.get("code").and_then(Value::as_i64);
        let lowered = raw.to_ascii_uppercase();
        if lowered.contains("RESOURCE_EXHAUSTED") {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                // 429 是下游看到的语义（额度用尽 / 限流），即使上游回的是别的码
                status: 429,
            };
        }
        if status == 401 || status == 403 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status,
            };
        }
        if status == 400 {
            return content_block::classify_or_fatal(status, error_body, message, code);
        }
        UpstreamErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }

    /// 「这个错误要不要原地退避重试」（模块头扩展 1）。
    ///
    /// ── 与编排层两条全局兜底的关系（先说清，免得误会成必需）──────
    /// 编排层自己已经有两档统一兜底：`transient_retry_advice`（408 / 5xx 按状态码）
    /// 与 `fallback_retry_advice`（一切 `Fatal` 在换账号前先原地重发一次）。
    /// 因此**「5xx 可重试」这件事不靠本方法也成立** —— 本方法的增量只有两点：
    ///   1. 给出一条本家措辞的原因（`RetryAdvice::reason` 会进请求日志的重试链）；
    ///   2. 覆盖「状态码不显眼、文案才是信号」的情形：Google 把瞬时故障写成
    ///      `error.status = UNAVAILABLE / INTERNAL / DEADLINE_EXCEEDED`，
    ///      message 里可能出现 `overloaded` / `try again` 这类措辞
    ///      （规格 §7.14 的首包空 / 流提前结束也归这一档）。
    ///
    /// ── 判据为什么只能看文案（如实说明）────────────────────────
    /// 契约只把**错误体**传进来，而 Google 的错误体把语义放在 `error.status` 里
    /// （归一化层读的是顶层 `code` 与 `error.message`），所以这里按 message 的
    /// 关键词判瞬时故障。429 不走这里 —— 它是 `QuotaLimited`，换账号比原地重试有用。
    fn retry_advice(&self, error_body: &Value, attempt: usize, budget: usize) -> Option<RetryAdvice> {
        if attempt >= budget {
            return None;
        }
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let lowered = raw.to_ascii_lowercase();
        let transient = ["unavailable", "internal", "deadline_exceeded", "overloaded", "try again", "timeout"]
            .iter()
            .any(|pattern| lowered.contains(pattern));
        if !transient {
            return None;
        }
        let retry = crate::server::config::retry_settings();
        Some(RetryAdvice {
            delay_ms: retry.delay_ms(),
            reason: format!("上游暂时不可用（{raw}），稍后重试"),
        })
    }

    /// 取可用 access token：临期（提前 15 分钟）或没有 token 时刷新并回写。
    ///
    /// 刷新链（单飞 + 比较再写）在 `oauth::ensure_fresh` —— 本方法只做转调，
    /// 这样「401 后的强制刷新」与「维护任务的临期刷新」共用同一段实现。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(store, account_id, false).await?;
            if credentials.access_token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "Antigravity 账号没有可用的 access token（刷新未返回令牌），请重新粘贴 refresh token",
                ));
            }
            Ok(credentials.access_token)
        })
    }

    /// 401 后的**强制**刷新：无视临期判定直接刷一次。
    ///
    /// 上游被拒时不会告诉我们令牌还剩多久（Google 的 401 也可能是服务端提前
    /// 失效），走临期门会原样返回刚被拒的串，让编排层的「刷新后重试一次」
    /// 退化成「拿同一个坏 token 再打一次」。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(store, account_id, true).await?;
            Ok(credentials.access_token)
        })
    }

    /// 本家有续期手段（`refresh_token` + Google token 端点）——维护任务要问本家。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 后台凭证维护问「这条账号是不是快到期了」（判据与刷新用的是同一把尺）。
    ///
    /// 没有 refresh_token 的账号返回 false：它连续期手段都没有，交给维护只会
    /// 变成每轮一条稳定失败的记录（与 Trae 的同一条处置）。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        let Some(record) = store.antigravity_account_record(account_id) else {
            return false;
        };
        match credentials::from_record(Some(&record)) {
            Ok(credentials) if credentials.can_refresh() => credentials.expiring(),
            _ => false,
        }
    }

    /// 有远程目录（`POST {base}:fetchAvailableModels`，见 `models.rs`）
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷模型目录：用指定账号（空串 = 队首可用账号）的令牌打上游。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`（与 Loomy / MonkeyCode / Command Code 同一处置：
    /// 脚本 / CI 不该看到一个红色失败）。点名的账号取不到则如实失败。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let record = store.antigravity_account_record(account_id);
            let Some(record) = record else {
                if account_id.trim().is_empty() {
                    logging::verbose("[Models]", "Antigravity 模型目录刷新跳过：尚未添加账号");
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的账号不存在或不可用，请重新选择");
            };
            let credentials = match credentials::from_record(Some(&record)) {
                Ok(credentials) => credentials,
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            let proxy = match oauth::account_proxy(&record) {
                Ok(proxy) => proxy,
                // 代理配置坏了要如实失败（不静默直连）：直连会拿到一张「从本机
                // 看得到」的表，而用户以为刷的是那条账号的出口。
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            // 令牌：临期就刷一次（复用适配器那条链，成功会回写记录）
            let credentials = if credentials.expiring() {
                match oauth::ensure_fresh(store, account_id, false).await {
                    Ok(fresh) => fresh,
                    Err(error) => return ModelRefreshOutcome::failed(error.message),
                }
            } else {
                credentials
            };
            // project：只在缺失时发现一次（best-effort，失败不影响目录）
            let mut project_id = credentials.project_id.clone();
            if project_id.trim().is_empty() {
                match project::discover_project(&credentials.access_token, proxy.as_ref()).await {
                    Ok(found) => {
                        if store.set_antigravity_project(&credentials.id, &found).is_err() {
                            logging::verbose(
                                "[Antigravity]",
                                "project 发现结果未能写回账号记录（记录可能已被删除）",
                            );
                        }
                        project_id = found;
                    }
                    Err(error) => logging::verbose(
                        "[Antigravity]",
                        &format!("project 未发现（目录刷新继续）：{}", error.message),
                    ),
                }
            }
            models::refresh(&credentials.access_token, &project_id, proxy.as_ref(), force).await
        })
    }
}

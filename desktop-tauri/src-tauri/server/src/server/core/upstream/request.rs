//! 对话转发的**传输层**：请求发送、上游错误解析、追踪 id 生成。
//!
//! ── 本文件为什么只做传输（Agent2API 改造 W2b-T3）────────────
//! 改造前这里还有头集合、URL 拼接、system 注入与 11128/6004 的判定 ——
//! 那些全是 **workbuddy 专属知识**（X-IDE-* 头、`/v2/chat/completions`、
//! 首条消息必须是 system、11128 敏感词拦截码），已整体搬进
//! `core::providers::workbuddy`。改造后 provider 差异全部收在适配器里，
//! 本文件对「上游是哪一家」一无所知：它只认 `TransportRequest` 这个
//! **协议无关**的形态（URL + 头 + 已序列化的 body + 出口）。
//!
//! 只保留的三件事：
//!   1. `send_chat_request`：按出口取 wreq Client 发一次请求，
//!      不读 body（SSE 需要 `bytes_stream()`）；
//!   2. `read_upstream_error`：把上游错误响应归一化成 `{code, message}`
//!      —— 「怎么读一个 HTTP 错误体」是协议层的事，与哪一家无关；
//!   3. `new_request_id`：一轮对话的追踪 id（适配器拼追踪头时用）。
//!
//! ── 出网 ────────────────────────────────────────────────────
//! 只走 `core::egress::client_for`：同一出口共用一个连接池，
//! 客户端上的 connect/read 超时也一并复用（见 egress 头部的旋钮映射）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::{AccountEgress, OnError, RotationPlan};
use crate::server::errors::GatewayError;

/// 单次对话请求的总超时（除退避重试外的等待时间）。
///
/// 与 Node 版的差别：Node 的 proxyFetch 不设总超时（bodyTimeout: 0），
/// 完全靠客户端断开与上游主动结束。Rust 侧的 wreq client 用
/// `read_timeout(600s)` 限制「两次数据之间」的间隔（见 core::egress 的说明），
/// 这里不再叠加总超时 —— 那会掐断长回答的 SSE 流。
pub const NO_TOTAL_TIMEOUT: Option<u64> = None;

/// 单次尝试**等待上游响应头**的上限（毫秒）。
///
/// 与 OmniProxy 的 `headers_timeout`（默认 300 秒）同义：连接建立之后、
/// 响应头到达之前的静默等待必须有个上限 —— `read_timeout` 管的是「两次数据
/// 之间」，虽然首包之前的等待也计入它，但分钟级的等待 × 重试链（同账号重发 ×
/// 换号）会把一条请求拖到几十分钟，而客户端早就等不及了。
///
/// 现在是**配置项**（设置页「请求超时 → 等待响应超时」，默认 300 秒，
/// 1–3600）：每次发送时从内存快照取一次，改完设置下一个请求就生效。
///
/// 为什么默认 300 秒是安全的：网关请求上游**恒带 `stream: true`**（见
/// `UpstreamService::forward` 的说明），响应头在 SSE 建立时就到达，
/// 与「模型思考多久」无关；非流式的长回答也走这条路径（本地聚合）。
/// 超时按传输层失败处理（502 + 既有退避重试），与连接失败同一档。
fn headers_timeout() -> Duration {
    Duration::from_millis(crate::server::config::timeout_settings().headers_ms())
}

/// 「连接中超时」的设定值（秒）：连接超时文案标注实际生效的秒数用，
/// 与设置页「请求超时 → 连接中超时」同一份配置。
fn connect_timeout_seconds() -> u64 {
    crate::server::config::timeout_settings().connect_ms() / 1000
}

/// 一次上游请求的全部素材（协议无关形态；provider 差异在构造阶段已消解）
#[derive(Clone)]
pub struct TransportRequest {
    /// 上游完整 URL（由适配器给出）
    pub url: String,
    /// 请求头（由适配器给出；不含出网代理相关）
    pub headers: Vec<(String, String)>,
    /// 已序列化的请求体（由编排层从适配器的 `plan.body` 序列化而来）
    ///
    /// 开启出站脱敏时，这里是**脱敏后**的字节（构造阶段已换好，见
    /// [`crate::server::core::masking::mask_outbound`]）。
    pub payload: String,
    /// 出网代理（账号级；与 provider 无关，由编排层解析后带上）
    pub egress: crate::server::core::proxies::AccountEgress,
    /// 账号未配代理时本家是否**跟随系统代理**出网（编排层按适配器的能力位填，
    /// 默认 false = 直连）。为 true 的家见 `ProviderAdapter::system_proxy_when_unset`。
    pub system_proxy_when_unset: bool,
    /// 本次请求的脱敏会话（`None` = 未脱敏）。
    ///
    /// 映射（占位符 → 原文）必须与 `payload` 同源：构造阶段调用
    /// [`crate::server::core::masking::mask_outbound`]，`Some((masked, session))`
    /// 时把 `masked` 放进 `payload`、会话放进这里，回复侧再取出来还原
    /// （Task 5.3）。**不要在这里放与 `payload` 不匹配的会话** —— 那会让
    /// 回复还原出错误的原文。
    pub mask_session: Option<crate::server::core::masking::MaskSession>,
}

impl TransportRequest {
    /// 构造辅助：对序列化后的请求体跑一次出站脱敏门禁。
    ///
    /// 返回 `(payload, mask_session, report)`：关闭脱敏时原样返回（会话为
    /// `None`、报告为空，字节不变）；开启时返回脱敏后的字节、会话与命中
    /// 元数据。前两者必须一起放进 [`TransportRequest`]（见 `mask_session`
    /// 的说明），报告交给调用方记 `mask_hits`。
    ///
    /// 开启却脱敏失败 → `Err(原因)`：调用方映射成 503，并**不发送任何请求**
    /// （fail-closed）。错误类型用 `String` 而不是 `GatewayError`，避免在
    /// `Result` 里搬一个体积很大的错误结构（调用点各自映射状态码）。
    pub fn mask_payload(
        payload: String,
    ) -> Result<
        (
            String,
            Option<crate::server::core::masking::MaskSession>,
            crate::server::core::masking::MaskReport,
        ),
        String,
    > {
        match crate::server::core::masking::mask_outbound(&payload) {
            Ok(None) => Ok((
                payload,
                None,
                crate::server::core::masking::MaskReport::default(),
            )),
            Ok(Some((masked, session, report))) => Ok((masked, Some(session), report)),
            Err(message) => Err(message),
        }
    }
}

/// 归一化后的上游错误：`{code, message}`
///
/// 非调试请求的 `message` 兜底取响应前 500 个字符，调试请求保留完整文本。
/// 适配器的 `classify_error` 收的就是本结构的 JSON 形态（`to_value`）——
/// 于是「上游错误怎么读」与「这条错误属于哪一档」彻底分开：
/// 前者是本文件（协议层），后者是适配器（provider 层）。
pub struct UpstreamErrorDetail {
    pub code: Option<i64>,
    pub message: String,
    /// 原始错误报文（能解析成 JSON 时保留）：原生协议直通要把上游错误体
    /// 原样交给客户端，而不是只回一句摘要。
    pub payload: Option<Value>,
}

impl UpstreamErrorDetail {
    /// 适配器入参形态：`{code, message}`（键名与上游 JSON 一致，
    /// 于是适配器可以直接用 `error_body.get("code")` 读，不需要中间结构体）
    pub fn to_value(&self) -> Value {
        json!({
            "code": self.code.map(Value::from).unwrap_or(Value::Null),
            "message": self.message,
        })
    }
}

/// 上游错误响应 → `{code, message}`（对照 Node 的 readUpstreamError）。
///
/// `capture`：调试模式的采集器（None = 未开启）。**必须在这里采** —— 本函数
/// 用 `response.text()` 把响应体整个吃掉，调用方拿不到第二份；不在这里顺手
/// 旁路，错误响应体就永远进不了调试报文（而失败现场恰恰是最需要看的那一半）。
pub async fn read_upstream_error(
    response: wreq::Response,
    capture: Option<&crate::server::core::debug_traffic::TrafficCapture>,
) -> UpstreamErrorDetail {
    if let Some(capture) = capture {
        capture.attach_response(response.status().as_u16(), response.headers());
    }
    let text = response.text().await.unwrap_or_default();
    if let Some(capture) = capture {
        capture.push(text.as_bytes());
    }
    let mut code = None;
    let mut message = if capture.is_some() {
        text.clone()
    } else {
        text.chars().take(500).collect()
    };
    let mut parsed = None;
    if let Ok(payload) = serde_json::from_str::<Value>(&text) {
        code = payload.get("code").and_then(Value::as_i64);
        // Node: `payload?.message || payload?.msg || payload?.error?.message || message`
        let candidate = payload
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
            .or_else(|| {
                payload
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            });
        if let Some(candidate) = candidate {
            message = candidate.to_string();
        }
        parsed = Some(payload);
    }
    UpstreamErrorDetail { code, message, payload: parsed }
}

/// 轮询计划一次请求会尝试的出口数（spec §9 的 N）：`onError=next` 试全部成员，
/// `onError=none` 只试首选；至少 1（成员为空理论上不可达 —— `resolve_account_egress`
/// 对空成员直接回 `Failed`，这里防御性兜底，免得文案出现「0 个出口」）。
fn rotation_attempts(plan: &RotationPlan) -> usize {
    let count = if plan.on_error == OnError::Next {
        plan.members.len()
    } else {
        1
    };
    count.max(1)
}

/// 发一次上游请求（不读 body，保留原始响应给流式转发与错误解析）。
///
/// 与 `core::auth_http::send_raw` 的分工：那个把响应读成文本，用于管理接口；
/// 这个把 `wreq::Response` 原样交给调用方，SSE 需要 `bytes_stream()`。
pub async fn send_chat_request(
    plan: &TransportRequest,
) -> Result<wreq::Response, UpstreamRequestError> {
    // ── 出站脱敏门禁（fail-closed）────────────────────────────────
    // 正常情况下构造阶段已经脱敏并把会话挂在 plan 上（`payload` 已是脱敏后
    // 的字节，回复侧用 `plan.mask_session` 还原）—— 这里不再二次脱敏。
    // 只有「未挂会话」的请求才在发送前补一次门禁：关闭时原样放行（字节不变），
    // 开启却脱敏失败时**一个字节都不发**（503，且不重试）。
    //
    // 脱敏成功却**没有把会话带回来**的构造方（未来新增、忘了挂会话）：同样
    // fail-closed。发出去的是脱敏后的字节，回复侧却没有映射可还原 —— 客户端
    // 会收到占位符而不是原文，而且这条请求的明文已经被替换掉、无法重放。宁可
    // 503，也不发一条注定还原不了的请求。当前 4 个构造点都会挂会话，这条分支
    // 只是防御未来的回归。
    let payload = match &plan.mask_session {
        Some(_) => plan.payload.clone(),
        None => match crate::server::core::masking::mask_outbound(&plan.payload) {
            Ok(None) => plan.payload.clone(),
            Ok(Some(_)) => {
                return Err(UpstreamRequestError::masking_failed(
                    "请求体已脱敏但未挂会话，拒绝发送",
                ))
            }
            Err(message) => return Err(UpstreamRequestError::masking_failed(message)),
        },
    };
    let url = plan.url.clone();
    let headers = plan.headers.clone();
    let response = crate::server::core::egress::dispatch(
        &plan.egress,
        plan.system_proxy_when_unset,
        |client| {
            let mut builder = client.post(&url).body(payload.clone());
            for (key, value) in &headers {
                builder = builder.header(key, value);
            }
            if let Some(timeout) = NO_TOTAL_TIMEOUT {
                builder = builder.timeout(Duration::from_millis(timeout));
            }
            builder
        },
    );
    // 出口说明（失败文案用）：按**出口类型**描述，而不是 `primary()` 指到的
    // 第一个成员 —— 轮询组可能在别的成员上尝试 / 成功，点名首个成员会误导。
    let via = match &plan.egress {
        AccountEgress::Direct if plan.system_proxy_when_unset => "跟随系统代理".to_string(),
        AccountEgress::Direct => "直连".to_string(),
        AccountEgress::Single(proxy) if !proxy.label.is_empty() => format!("经代理 {}", proxy.label),
        AccountEgress::Single(proxy) => format!("经代理 {}", proxy.host),
        AccountEgress::Rotate(plan) => format!("轮询组（{} 个出口）", rotation_attempts(plan)),
    };
    // 等待响应头有上限（见 headers_timeout 的说明）：超时后 future 被丢弃，
    // 上游连接随之关闭（与客户端断开时的取消是同一机制）
    let headers_budget = headers_timeout();
    let response = tokio::time::timeout(headers_budget, response).await;
    match response {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => {
            // `send()` 阶段的超时只可能来自连接（等待响应头由外层计时器管，
            // 它的预算 ≤ 客户端 read_timeout，见 egress 的说明）：此时错误链
            // 对用户没有信息量（就是「没连上」），文案直接给设置页的旋钮名
            // 与实际生效的秒数；其余（ECONNREFUSED、代理鉴权失败、DNS…）
            // 保留根因与出口说明，便于判断是不是代理配错了
            let reason = if error.is_timeout() {
                format!("连接中超时({}秒)", connect_timeout_seconds())
            } else {
                "上游连接失败".to_string()
            };
            let message = if error.is_timeout() {
                format!("连接中超时({}秒，出口 {via})", connect_timeout_seconds())
            } else {
                let detail = egress::describe_error_detail(&error);
                // 轮询组 onError=next：到这里说明**所有成员都连不上**，按 spec §9
                // 补上「已尝试 N 个出口」。via 里已是「轮询组（N 个出口）」，这里换
                // 一种写法避免再套一层括号（可读性）。
                match &plan.egress {
                    AccountEgress::Rotate(plan) if plan.on_error == OnError::Next => {
                        format!(
                            "上游请求失败（轮询组，已尝试 {} 个出口）: {detail}",
                            rotation_attempts(plan)
                        )
                    }
                    _ => format!("上游请求失败（{via}）: {detail}"),
                }
            };
            let safe_to_retry = error.is_connect() || error.is_builder();
            Err(UpstreamRequestError {
                message,
                reason,
                safe_to_retry,
                status_code: 502,
                retryable: true,
                network_failure: true,
            })
        }
        Err(_elapsed) => {
            let seconds = headers_budget.as_secs();
            Err(UpstreamRequestError {
                reason: format!("等待响应超时({seconds}秒)"),
                message: format!("等待响应超时({seconds}秒，出口 {via})"),
                safe_to_retry: false,
                status_code: 502,
                retryable: true,
                network_failure: true,
            })
        }
    }
}

/// 传输层失败（默认收敛成 502，与 Node 的 fetchViaProxy 一致）
#[derive(Clone, Debug)]
pub struct UpstreamRequestError {
    pub message: String,
    /// 简短原因（重试链里「这次为什么重试」的展示文案）：
    /// 「连接中超时(N秒)」/「等待响应超时(N秒)」/「上游连接失败」。
    /// `message` 是最终失败的详细文案（带出口说明），这里是它的简短形态。
    pub reason: String,
    /// 只有请求尚未写出（建连或构造失败）才允许非幂等的生图自动重试。
    pub safe_to_retry: bool,
    /// 下发给客户端的状态码（传输层失败默认 502；出站脱敏失败是 503）。
    pub status_code: i32,
    /// 是否允许按设置退避重发。脱敏门禁失败**不可重试**：重发只会得到同
    /// 一份错误，且没有任何字节到过上游。
    pub retryable: bool,
    /// 是否是「链路层」失败（证书类错误除外）：备用入口 / 候选地址只对这类生效。
    /// 出站脱敏门禁失败不算链路故障。
    pub network_failure: bool,
}

impl UpstreamRequestError {
    /// 出站脱敏 fail-closed：请求体没有发出去，按 503 收敛。
    ///
    /// `message` 由 [`crate::server::core::masking::mask_outbound`] 给出，
    /// 只描述「为什么没发」，**不含请求体内容**（不记录、不回显明文）。
    pub fn masking_failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            reason: "出站脱敏失败".to_string(),
            safe_to_retry: false,
            status_code: 503,
            retryable: false,
            network_failure: false,
        }
    }

    pub fn to_gateway_error(&self) -> GatewayError {
        GatewayError::with_status(self.status_code, self.message.clone())
    }
}

/// 用「已解析的候选地址」客户端发一次（备用入口 / 候选 IP 路径专用）。
///
/// `plan.payload` 在构造阶段已脱敏（并挂了 `mask_session`），这里不再二次
/// 脱敏 —— 调用方（`network_fallback::Fallback`）只在 `send_chat_request`
/// 之外补发同一份已备好的请求。multipart 信封由调用方按需构造。
pub(super) async fn send_chat_with_client(
    plan: &TransportRequest,
    client: &wreq::Client,
    form: Option<wreq::multipart::Form>,
) -> Result<wreq::Response, UpstreamRequestError> {
    let url = plan.url.clone();
    let headers = plan.headers.clone();
    let payload = plan.payload.clone();
    let mut builder = client.post(&url);
    if let Some(form) = form {
        builder = builder.multipart(form);
    } else {
        builder = builder.body(payload);
    }
    for (key, value) in &headers {
        // multipart 由 wreq 自己写 Content-Type（含 boundary），跳过原始头
        if form_present(&plan.headers) && key.eq_ignore_ascii_case("content-type") {
            continue;
        }
        builder = builder.header(key, value);
    }
    if let Some(timeout) = NO_TOTAL_TIMEOUT {
        builder = builder.timeout(Duration::from_millis(timeout));
    }
    builder.send().await.map_err(|error| UpstreamRequestError {
        message: format!("上游请求失败（备用入口）: {}", egress::describe_error_detail(&error)),
        reason: "上游连接失败".to_string(),
        safe_to_retry: error.is_connect() || error.is_builder(),
        status_code: 502,
        retryable: true,
        network_failure: !error.is_builder() && (error.is_connect() || error.is_timeout()),
    })
}

fn form_present(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("content-type") && value.eq_ignore_ascii_case("multipart/form-data")
    })
}

/// 生成一轮对话的追踪 id（对应 Node 的 `randomUUID()`）。
///
/// 用 RFC 4122 v4 形态：上游会把 X-Request-ID 记进服务端日志，
/// 保持标准 UUID 形态便于与官方客户端的行为对齐（也便于下游按 UUID 解析）。
/// 随机源取自 `RandomState`（由 OS 随机种子初始化）+ 进程内计数器 + 纳秒时钟，
/// 三者拼出的 id 在单机排障场景足够唯一 —— 它不是安全凭证，不用引 rand 依赖。
pub fn new_request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut bytes = [0u8; 16];
    let mix = |label: &[u8], salt: u64| -> u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write(label);
        hasher.write_u64(salt);
        hasher.finish()
    };
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let first = mix(b"wb-request-id-a", nanos ^ pid);
    let second = mix(b"wb-request-id-b", counter.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    bytes[..8].copy_from_slice(&first.to_be_bytes());
    bytes[8..].copy_from_slice(&second.to_be_bytes());
    // 版本位 4 + 变体位 10xx（RFC 4122）
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

// ─── UUIDv7 家族（WorkBuddy 对话链路的会话/消息/链路 id）──────
//
// 2026-10-16 对国际版桌面客户端 5.8.4 的抓包结论（参考实现
// workbuddy2api-hub #381，本仓与之对齐）：官方客户端的请求 / 消息 id 是
// 32 位无连字符 hex、会话 id 是带连字符的 UUID，都是 UUIDv7 家族
// （48 位毫秒时间戳打头，时间有序）。上游不校验版本位 —— 抓包里主对话的
// X-Conversation-ID 实际是 v4、辅助请求是 v7，两种都被正常接受 —— 所以
// 形态只求贴近，不承担语义。上游也不拿这些 id 存对话历史（同一 id 连发两轮、
// 第二轮只带 system+user，模型仍答不出上一轮），因此**每次请求现生成**，
// 不按会话维护映射；缺了整个头族才会触发渠道校验（400 / code 11-128，
// `conversation id missing`）。

/// 生成 16 字节的 UUIDv7（RFC 9562）：毫秒时间戳 + 版本位 7 + 变体位 10。
///
/// 随机源与 [`new_request_id`] 同款（`RandomState` 种子 + 进程内计数器），
/// 不为此引入新的随机依赖。
fn uuid7_bytes() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mix = |label: &[u8], salt: u64| -> u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write(label);
        hasher.write_u64(salt);
        hasher.finish()
    };
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
        & ((1 << 48) - 1);
    let rand_a = mix(b"wb-uuid7-a", counter) & 0x0fff;
    let rand_b = mix(b"wb-uuid7-b", counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)) & ((1 << 62) - 1);
    let int = ((millis as u128) << 80)
        | (0x7u128 << 76)
        | ((rand_a as u128) << 64)
        | (0b10u128 << 62)
        | (rand_b as u128);
    int.to_be_bytes()
}

/// 请求 / 消息 id（`X-Request-ID` / `X-Conversation-Message-ID` /
/// `X-Conversation-Request-ID` / `X-Root-Request-ID`）：32 位无连字符 hex。
pub fn new_message_id() -> String {
    let bytes = uuid7_bytes();
    let mut id = String::with_capacity(32);
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// 会话 id（`X-Conversation-ID`）：带连字符的 UUID 形态。
pub fn new_conversation_id() -> String {
    let bytes = uuid7_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// 链路追踪的 span id（`traceparent` / b3 / `X-B3-SpanId`）：16 位 hex。
pub fn new_span_id() -> String {
    let bytes = uuid7_bytes();
    let mut id = String::with_capacity(16);
    for byte in &bytes[..8] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    /// 开启脱敏但脱敏器不可用：门禁必须 fail-closed —— 返回 503，且
    /// **一个请求都不能发到上游**（用本地计数服务验证）。
    #[tokio::test]
    async fn masking_enabled_without_masker_sends_nothing() {
        let _guard = crate::server::core::masking::config::testing::enabled_without_masker();

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/chat",
            axum::routing::post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/chat", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let plan = TransportRequest {
            url,
            headers: Vec::new(),
            payload: r#"{"messages":[]}"#.to_string(),
            egress: crate::server::core::proxies::AccountEgress::Direct,
            system_proxy_when_unset: false,
            // 构造阶段没走门禁（模拟旧调用点 / 未挂会话的请求）
            mask_session: None,
        };
        let result = send_chat_request(&plan).await;
        match result {
            Err(error) => {
                assert_eq!(error.status_code, 503);
                assert!(!error.retryable);
                assert_eq!(error.to_gateway_error().status_code, 503);
                assert!(
                    error.message.contains("未发送任何请求"),
                    "{}",
                    error.message
                );
            }
            Ok(_) => panic!("脱敏器不可用时不应发出请求"),
        }
        // 门禁在 egress::dispatch 之前返回：上游一次都没被打到。
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }

    /// 脱敏成功但构造方忘了挂会话：必须 fail-closed（503、不发、不重试）。
    ///
    /// 发出去的是脱敏后的字节，而回复侧没有映射可还原 —— 客户端会看到占位符，
    /// 且明文已被替换、无法重放。这条分支是防御未来构造点的回归。
    #[tokio::test]
    async fn masking_without_a_session_is_refused_instead_of_sent() {
        let masker = std::sync::Arc::new(crate::server::core::masking::RulesMasker::default());
        let _guard = crate::server::core::masking::config::testing::enabled_with(masker);

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/chat",
            axum::routing::post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/chat", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let plan = TransportRequest {
            url,
            headers: Vec::new(),
            payload: r#"{"messages":[{"role":"user","content":"key sk-abc123"}]}"#.to_string(),
            egress: crate::server::core::proxies::AccountEgress::Direct,
            system_proxy_when_unset: false,
            // 构造方忘了把 mask_outbound 的会话带回来
            mask_session: None,
        };
        let error = send_chat_request(&plan)
            .await
            .expect_err("脱敏成功却未挂会话时必须拒绝发送");
        assert_eq!(error.status_code, 503);
        assert!(!error.retryable);
        assert!(
            error.message.contains("未挂会话"),
            "{}",
            error.message
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0, "上游一次都不该被打到");
        server.abort();
    }
}

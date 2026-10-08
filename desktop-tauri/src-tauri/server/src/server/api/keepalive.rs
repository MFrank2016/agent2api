//! 流式转发的「首字节等待」保活（SSE keep-alive）。
//!
//! ── 它解决什么 ──────────────────────────────────────────────
//! 流式请求（客户端要 `stream:true`）在响应头发出之前要先等
//! `upstream().forward()` 返回：那一步包含选路 + 建连 + 等上游响应头，传输
//! 失败还会退避重试（连接超时 30s + 5s 退避，最多 3 次）。上游慢或抖时这段
//! 能到 30~60s+，其间客户端**一个字节都收不到** —— 按 ~60s TTFB 超时收尾的
//! 客户端会先断开，网关侧只能记一条 408「客户端在响应完成前断开连接」
//! （见 `api::disconnect_guard`；实测某模型上出现过 318 条）。
//!
//! 本模块让流式请求**先**把响应头发出去（200 + `text/event-stream`），在等待
//! 上游期间周期性下发 SSE 注释帧 `: keep-alive` —— 客户端因此持续有字节可读、
//! TTFB 计时被不断重置，不会因为「迟迟等不到首字节」而断开。
//!
//! ── 快慢两分支（[`race`]）────────────────────────────────────
//! 把转发 future 钉住（`Pin<Box<dyn Future + Send>>`，`'static`）后与宽限期
//! [`GRACE`] 赛跑：
//!   - **快分支**（宽限期内就绪，绝大多数请求）：原样交出结果，调用方走**原有**
//!     的响应路径 —— 状态码与形态逐字不变，本模块完全不介入；
//!   - **慢分支**（超过宽限期仍未就绪）：把仍未就绪的 future 交给
//!     [`KeepAliveStream`]，由它继续等上游、期间周期下发保活帧。
//!
//! ── 三段式状态机（[`KeepAliveStream`]）────────────────────────
//!   1. **等待**：持有尚未就绪的转发 future；每隔 [`KEEP_ALIVE_INTERVAL`] 下发
//!      一帧 `: keep-alive\n\n`；
//!   2. **接管**：future 一就绪，用调用方给的 [`Materialize`] 把它变成下游字节流
//!      （chat 侧是 `RecordingStream` 包住上游帧 / 合成的收尾帧，见
//!      `api::chat::materialize_chat`）；
//!   3. **透传**：之后原样转发下游流的每一项，直到它结束。
//!
//! ── 保活帧为什么不进记账 / 不干扰收尾判定 ────────────────────
//! 保活帧只在下游流**建出来之前**下发（阶段 1），而 `RecordingStream` 是在
//! 阶段 2 才被 [`Materialize`] 建出来的 —— 也就是说保活帧根本不经过记账流：
//! 既不会被当成正文累积（`RawCapture`）、也不会触发首响采集
//! （`note_first_frame`），更不会命中 `pipeline::terminal::CHAT` 的收尾帧特征。
//! 这是把保活放在记账层**外面**的自然结果，不是额外的特判。
//!
//! ── 等待阶段被丢弃（客户端断开）─────────────────────────────
//! 与 `RecordingStream` 的 Drop 兜底同一职责：若本流在阶段 1 就被丢弃（客户端
//! 在保活期间断开 / 服务退出），`Drop` 就地补一条 `STREAM_ABORTED` 终态并注销
//! 取消令牌 —— 否则这条请求会永远停在「进行中」（`DisconnectGuard` 已
//! `handoff`，不会再兜底）。进入阶段 3 后上下文已交给 `RecordingStream`，
//! 本流的 Drop 不再做任何事（收尾由那边的 Drop 完成）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块零 unwrap/expect/panic。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::server::core::upstream::{cancellation, ForwardOutcome};
use crate::server::errors::GatewayError;

use super::pipeline::{self, RecordContext, STREAM_ABORTED};

/// 保活注释帧（SSE comment）：以 `:` 开头的行，客户端解析器一律忽略其内容。
///
/// 字节形状是硬约定（`: keep-alive\n\n`）：客户端只要收到**任何**字节就会重置
/// TTFB 计时，但保持标准的注释帧形状能确保它绝不进入内容解析。
pub const KEEP_ALIVE_FRAME: &[u8] = b": keep-alive\n\n";

/// 快慢分支的分界：`forward()` 在这个宽限期内没就绪就走慢分支。
///
/// 8s 是刻意的折中：短到让绝大多数正常请求仍走原有的「拿到响应头再回」路径
/// （快分支，行为逐字不变），又长到不必为一次正常的上游首字节等待
/// （常见 1~3s）白开一条保活流。
pub const GRACE: Duration = Duration::from_secs(8);

/// 慢分支里两帧保活的间隔。
///
/// 取 12s：落在「~10–15s」区间中段，远小于客户端常见的 ~60s TTFB 超时 ——
/// 即便上游连续几次仍无字节，客户端也不会因为计时耗尽而断开。
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(12);

/// 尚未就绪的转发 future（`'static`：调用方把所有权移进来，见 `api::chat`）。
pub type ForwardFuture =
    Pin<Box<dyn Future<Output = Result<ForwardOutcome, GatewayError>> + Send>>;

/// 下游字节流（与 `pipeline::sse_response` 的入参同型）。
pub type InnerStream = Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>;

/// 把转发结果变成下游流的构造器（协议相关，由调用方提供）。
///
/// 收 `RecordContext` 的所有权：三条入口（chat / responses / anthropic）各自把
/// 结果包进 `RecordingStream`（chat 的实现见 `api::chat::materialize_chat`）。
pub type Materialize =
    Box<dyn FnOnce(Result<ForwardOutcome, GatewayError>, RecordContext) -> InnerStream + Send>;

/// 与宽限期赛跑的结果（见 [`race`]）。
pub enum ForwardStart {
    /// 宽限期内就绪 → 快分支：调用方走**原有**的响应路径（状态码 / 形态不变）。
    Ready(Result<ForwardOutcome, GatewayError>),
    /// 超过宽限期仍未就绪 → 慢分支：把 future 交给 [`KeepAliveStream`] 继续等。
    Slow(ForwardFuture),
}

/// 让转发 future 与 [`GRACE`] 赛跑：先就绪走 [`ForwardStart::Ready`]，否则把
/// 仍未就绪的 future 原样交回（[`ForwardStart::Slow`]）。
///
/// ── 为什么是「快分支 return、慢分支落空」──────────────────────
/// `tokio::select!` 的 `&mut forward` 借用在整个 select 作用域内都活着，臂内
/// 直接移动 `forward` 会与这个借用冲突。改成快分支 `return`、慢分支落空到
/// select 之后，那时借用已释放，再移动 `forward` 合法。
pub async fn race(mut forward: ForwardFuture, grace: Duration) -> ForwardStart {
    tokio::select! {
        result = &mut forward => return ForwardStart::Ready(result),
        _ = tokio::time::sleep(grace) => {}
    }
    ForwardStart::Slow(forward)
}

/// 等待首字节期间的保活流（三段式状态机见模块头）。
pub struct KeepAliveStream {
    /// 尚未就绪的转发 future（阶段 1 持有；接管后置 None）
    forward: Option<ForwardFuture>,
    /// 保活计时器（到点下发一帧注释，随后重置）
    keepalive: Pin<Box<tokio::time::Sleep>>,
    /// 两帧保活的间隔（重置计时器用）
    interval: Duration,
    /// 接管后的下游流（阶段 3 持有；None = 还没接管）
    inner: Option<InnerStream>,
    /// 收尾上下文（阶段 1 持有；接管时移交给 [`Materialize`]）
    context: Option<RecordContext>,
    /// 结果 → 下游流的构造器（接管时取走，保证只调用一次）
    materialize: Option<Materialize>,
    /// 已收尾（下游流结束）—— 之后的 poll 一律返回 None
    done: bool,
}

impl KeepAliveStream {
    /// 用仍未就绪的转发 future 建流（只在 [`race`] 给出 `Slow` 后调用）。
    pub fn new(
        forward: ForwardFuture,
        interval: Duration,
        context: RecordContext,
        materialize: Materialize,
    ) -> Self {
        Self {
            forward: Some(forward),
            keepalive: Box::pin(tokio::time::sleep(interval)),
            interval,
            inner: None,
            context: Some(context),
            materialize: Some(materialize),
            done: false,
        }
    }
}

impl Stream for KeepAliveStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 字段全为 Unpin（Pin<Box<…>> 与 Box<…> 都 Unpin），get_mut 安全
        let this = self.get_mut();
        loop {
            if this.done {
                return Poll::Ready(None);
            }
            // 阶段 3：已接管 → 原样透传下游流
            if let Some(inner) = this.inner.as_mut() {
                match inner.poll_next_unpin(cx) {
                    Poll::Ready(None) => {
                        this.done = true;
                        return Poll::Ready(None);
                    }
                    other => return other,
                }
            }
            // 阶段 1：看转发 future 是否就绪。先算出 poll 结果、再动其它字段，
            // 避免 `&mut this.forward` 的借用横跨赋值。
            let polled = match this.forward.as_mut() {
                Some(forward) => forward.as_mut().poll(cx),
                // 理论不可达（forward 与 inner 必有一个 Some）；保守收尾
                None => {
                    this.done = true;
                    return Poll::Ready(None);
                }
            };
            match polled {
                Poll::Ready(result) => {
                    // 阶段 2：接管。取走 future 与上下文，用构造器建下游流
                    this.forward = None;
                    let context = this.context.take();
                    let materialize = this.materialize.take();
                    match (context, materialize) {
                        (Some(context), Some(materialize)) => {
                            this.inner = Some(materialize(result, context));
                        }
                        _ => {
                            this.done = true;
                            return Poll::Ready(None);
                        }
                    }
                    // 回到循环顶部去 poll 刚建好的下游流
                }
                Poll::Pending => {
                    // 到点就发一帧保活；先重置计时器再返回
                    // （与 `upstream::stall::IdleGuard` 同一手法）
                    match this.keepalive.as_mut().poll(cx) {
                        Poll::Ready(()) => {
                            let deadline = tokio::time::Instant::now() + this.interval;
                            this.keepalive.as_mut().reset(deadline);
                            return Poll::Ready(Some(Ok(Bytes::from_static(KEEP_ALIVE_FRAME))));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    }
}

impl Drop for KeepAliveStream {
    fn drop(&mut self) {
        // 只有「还在阶段 1」才需要在这里收尾：context 尚未移交（移交后为 None）。
        // 这一段对应「客户端在保活期间断开 / 服务退出」——`DisconnectGuard` 已
        // handoff，不再兜底，所以补一条中断终态并注销取消令牌。
        if let Some(context) = self.context.take() {
            pipeline::record_entry(&context, Some(STREAM_ABORTED.to_string()));
            cancellation::unregister(&context.telemetry.id());
        }
    }
}

#[cfg(test)]
mod tests {
    //! 保活流的字节形状用例（对应任务书的三条：保活→上游帧、快分支不受影响）。
    //! 错误帧 → `[DONE]` 那条测的是 chat 的 `materialize_chat`，放在
    //! `api::chat` 的用例里（本模块不认识任何协议）。
    use super::*;
    use std::sync::Arc;

    use futures::StreamExt;
    use serde_json::json;

    use crate::server::core::upstream::usage::RequestTelemetry;
    use crate::server::request_stats::{RequestStats, Retention};

    /// 一个「记账降级为空操作」的收尾上下文：本模块的用例只关心字节形状，
    /// 不关心是否真的落库（库句柄给 None，`record_entry` 静默跳过写入）。
    fn noop_context() -> RecordContext {
        RecordContext {
            stats: Arc::new(RequestStats::with_db(None, || Retention::default())),
            telemetry: Arc::new(RequestTelemetry::new()),
            started_at: 0,
            model: "test-model".to_string(),
            client_model: String::new(),
            client_reasoning: String::new(),
            status: 200,
            raw_request: None,
            raw_response: None,
            is_test: false,
        }
    }

    /// 原样透传上游帧的构造器（模拟 chat 的 Stream 分支，去掉记账包装）。
    fn passthrough() -> Materialize {
        Box::new(|outcome, _context| match outcome {
            Ok(ForwardOutcome::Stream { stream, .. }) => stream,
            _ => Box::new(futures::stream::iter(
                Vec::<Result<Bytes, std::io::Error>>::new(),
            )),
        })
    }

    fn one_frame(data: &'static [u8]) -> InnerStream {
        Box::new(futures::stream::iter(vec![Ok(Bytes::from_static(data))]))
    }

    /// 保活间隔与转发延迟取「小但差距大」的实数值：区间足够宽，任何调度
    /// 抖动都不会把「保活先于上游帧」的顺序颠倒（不引 tokio 的 test-util
    /// 时间控制 —— Cargo.toml 的依赖面刻意收窄，不为测试加 feature）。
    const TEST_INTERVAL: Duration = Duration::from_millis(15);
    const TEST_FORWARD_DELAY: Duration = Duration::from_millis(150);
    const TEST_GRACE: Duration = Duration::from_millis(30);

    #[tokio::test]
    async fn emits_keep_alive_while_pending_then_upstream_frames() {
        // 转发 150ms 后才就绪，保活间隔 15ms → 期间应下发若干帧保活，随后
        // 是上游帧。断言只依赖顺序（保活全部在上游帧之前、至少一帧），不
        // 依赖具体帧数，避开调度抖动。
        let forward: ForwardFuture = Box::pin(async {
            tokio::time::sleep(TEST_FORWARD_DELAY).await;
            Ok(ForwardOutcome::Stream {
                status: 200,
                stream: Box::new(futures::stream::iter(vec![
                    Ok(Bytes::from_static(b"data: {\"a\":1}\n\n")),
                    Ok(Bytes::from_static(b"data: [DONE]\n\n")),
                ])),
            })
        });
        let mut stream =
            KeepAliveStream::new(forward, TEST_INTERVAL, noop_context(), passthrough());

        let mut frames: Vec<Bytes> = Vec::new();
        while let Some(item) = stream.next().await {
            frames.push(item.expect("保活流不应产出错误"));
        }
        assert!(frames.len() >= 3, "至少一帧保活 + 两帧上游，实得 {} 帧", frames.len());
        // 尾部两帧必须是上游帧，其余全是保活帧
        let upstream = &frames[frames.len() - 2..];
        assert_eq!(upstream[0], Bytes::from_static(b"data: {\"a\":1}\n\n"));
        assert_eq!(upstream[1], Bytes::from_static(b"data: [DONE]\n\n"));
        assert!(
            frames[..frames.len() - 2]
                .iter()
                .all(|frame| frame == &Bytes::from_static(KEEP_ALIVE_FRAME)),
            "上游帧之前只应有保活帧"
        );
    }

    #[tokio::test]
    async fn ready_future_emits_no_keep_alive() {
        // future 立即就绪：接管时保活计时器还没到点，一帧保活都不该出现
        // （「快路径不受影响」在流层面的体现）。
        let forward: ForwardFuture = Box::pin(async {
            Ok(ForwardOutcome::Stream { status: 200, stream: one_frame(b"data: [DONE]\n\n") })
        });
        let mut stream =
            KeepAliveStream::new(forward, TEST_INTERVAL, noop_context(), passthrough());
        let first = stream.next().await.expect("应有第一帧").expect("不应出错");
        assert_eq!(first, Bytes::from_static(b"data: [DONE]\n\n"));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn race_returns_ready_when_future_resolves_within_grace() {
        // 宽限期内就绪 → 快分支（Ready），慢分支绝不介入
        let forward: ForwardFuture =
            Box::pin(async { Ok(ForwardOutcome::Completion { body: json!({"ok": true}) }) });
        match race(forward, TEST_GRACE).await {
            ForwardStart::Ready(Ok(ForwardOutcome::Completion { body })) => {
                assert_eq!(body["ok"], json!(true));
            }
            _ => panic!("future 立即就绪时应走快分支"),
        }
    }

    #[tokio::test]
    async fn race_returns_slow_when_future_stays_pending_past_grace() {
        // 转发 150ms、宽限期 30ms → 慢分支（Slow），把仍未就绪的 future 交回
        let forward: ForwardFuture = Box::pin(async {
            tokio::time::sleep(TEST_FORWARD_DELAY).await;
            Ok(ForwardOutcome::Completion { body: json!({"late": true}) })
        });
        match race(forward, TEST_GRACE).await {
            ForwardStart::Slow(mut forward) => {
                // 交回的 future 仍可继续等待并最终就绪
                let outcome = forward.as_mut().await;
                assert!(matches!(outcome, Ok(ForwardOutcome::Completion { .. })));
            }
            ForwardStart::Ready(_) => panic!("future 150ms 才就绪，宽限期 30ms 内不该 Ready"),
        }
    }
}

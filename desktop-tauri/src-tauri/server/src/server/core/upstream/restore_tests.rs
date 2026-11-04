//! 回复还原（Task 5.3）的接线用例：占位符在流式 / 非流式两条出口都要还原。
//!
//! 缓冲本身的边界用例在 `core::masking` 的单元测试里（跨分片、半截前缀、
//! 非法 UTF-8）；这里只证明**接线位置**正确 —— 还原发生在 SSE 逐行解析之前
//! （`ForwardStream` / 聚合器），而不是下发之后。

use std::sync::Arc;

use bytes::Bytes;
use futures::{stream, StreamExt};
use serde_json::{json, Value};

use super::aggregate;
use super::connections::{ConnectionGuard, Connections};
use super::sse::FramePolicy;
use super::usage::RequestTelemetry;
use super::ForwardStream;
use crate::server::core::masking::{MaskSession, Masker, RulesMasker};

/// 建一个会话，把 `text` 脱敏；返回（会话, 脱敏后的文本, 占位符）。
fn masked(text: &str) -> (MaskSession, String, String) {
    let session = RulesMasker::default().session();
    let (masked, report) = session.mask_text(text);
    let placeholder = report
        .hits
        .first()
        .map(|hit| hit.placeholder.clone())
        .unwrap_or_default();
    (session, masked, placeholder)
}

/// 一帧标准 chat SSE（delta 由调用方给）。
fn frame(delta: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "c1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
        })
    )
}

fn bytes_stream(
    chunks: Vec<Vec<u8>>,
) -> futures::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(stream::iter(
        chunks.into_iter().map(|chunk| Ok(Bytes::from(chunk))),
    ))
}

fn guard() -> ConnectionGuard {
    ConnectionGuard::new(Connections::new())
}

/// 把一条 SSE 字节流喂给 `ForwardStream`，收集下发的全部字节。
async fn drain(chunks: Vec<Vec<u8>>, session: Option<MaskSession>) -> Vec<u8> {
    let stream = ForwardStream::from_translated(
        bytes_stream(chunks),
        None,
        guard(),
        Arc::new(RequestTelemetry::new()),
        FramePolicy::default(),
        session,
    );
    let mut out = Vec::new();
    let mut pinned = Box::pin(stream);
    while let Some(frame) = pinned.next().await {
        match frame {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(error) => panic!("测试流不该报错: {error}"),
        }
    }
    out
}

/// 客户端把 SSE 帧拼回正文（只看 `delta.content`）。
fn content_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split('\n')
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter_map(|chunk| {
            chunk
                .get("choices")?
                .get(0)?
                .get("delta")?
                .get("content")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// 占位符被 TCP 分片切开时，流式出口必须还原成原文（解析前拼回完整占位符）。
#[tokio::test]
async fn sse_placeholder_across_chunks() {
    let (session, masked_text, placeholder) = masked("key sk-abc123");
    assert!(!placeholder.is_empty(), "样例应当命中一条规则");
    let body = frame(json!({"content": masked_text}));
    // 在占位符内部切一刀（`[[A2A_M` 之后）
    let split = body
        .find(&placeholder)
        .map(|index| index + 7)
        .expect("帧里应当含占位符");
    let (head, tail) = body.split_at(split);

    let out = drain(
        vec![head.as_bytes().to_vec(), tail.as_bytes().to_vec()],
        Some(session),
    )
    .await;

    let text = content_of(&out);
    assert_eq!(text, "key sk-abc123");
    assert!(!text.contains("A2A_MASK"), "占位符不能漏给客户端: {text}");
}

/// 未开启脱敏（会话为 None）：下发字节必须与上游逐字节一致。
#[tokio::test]
async fn disabled_masking_is_byte_identical() {
    let body = frame(json!({"content": "hello [[A2A_MASK_email_1]] world"}));
    let out = drain(vec![body.as_bytes().to_vec()], None).await;
    assert_eq!(String::from_utf8_lossy(&out), body);
}

/// 非流式出口：`tool_calls[].function.arguments` 里的占位符（以转义 JSON 字符串
/// 的形态出现、且横跨两个 arguments 增量）同样要还原。
#[tokio::test]
async fn non_stream_json_restore_including_tool_arguments() {
    let (session, masked_text, placeholder) = masked("sk-abcdefgh");
    assert!(!placeholder.is_empty(), "样例应当命中一条规则");
    let arguments = format!(r#"{{"token":"{masked_text}"}}"#);
    let split = arguments
        .find(&placeholder)
        .map(|index| index + 7)
        .expect("arguments 里应当含占位符");
    let (head, tail) = arguments.split_at(split);

    let chunks = vec![
        frame(json!({"tool_calls": [{
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": {"name": "run", "arguments": head},
        }]}))
        .into_bytes(),
        format!(
            "data: {}\n\n",
            json!({"choices": [{
                "index": 0,
                "delta": {"tool_calls": [{"index": 0, "function": {"arguments": tail}}]},
                "finish_reason": "tool_calls",
            }]})
        )
        .into_bytes(),
        b"data: [DONE]\n\n".to_vec(),
    ];

    let result = aggregate::aggregate_frame_stream(
        bytes_stream(chunks),
        Arc::new(RequestTelemetry::new()),
        FramePolicy::default(),
        Some(session),
    )
    .await
    .expect("测试帧应当能聚合");

    let call = &result.body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "run");
    let arguments = call["function"]["arguments"]
        .as_str()
        .expect("arguments 应当是字符串");
    assert!(arguments.contains("sk-abcdefgh"), "{arguments}");
    assert!(!arguments.contains("A2A_MASK"), "{arguments}");
}

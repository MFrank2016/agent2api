//! LobsterAI（有道龙虾）上游信封：`{ code, message?, data }` 的统一解包。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 有道这家的**所有**接口（exchange / refresh / 签到三步 / 模型目录 / 余额）
//! 都套同一层信封：业务成功 `code == 0`、真实数据在 `data` 里；失败
//! `code != 0`、`message` 给文案。而 `auth_http::send_raw` 返回的是**原始
//! body**（不拆信封），所以各调用点必须自己解包 —— 这里把「拆信封」与
//! 「判业务错」收敛成一处，免得每个文件各写一份（`balance` / `models` 早先
//! 就各自手写了 `data` 取值）。
//!
//! ── 契约出处（官方客户端 netease-youdao/LobsterAI）────────────
//! ```text
//!   exchange     main.ts：`if (body.code !== 0 || !body.data) return error`
//!                之后一律读 `body.data.accessToken` / `body.data.refreshToken`
//!   refresh      authSessionManager.ts：`errorCode === 0 && data.accessToken`
//!   activities   activityClient.ts：`body.code === 0 && body.data !== undefined`
//!                成立才返回 `body.data`（slot / context / action 三步同一信封）
//! ```
//!
//! ── 容错 ────────────────────────────────────────────────────
//! [`data`] 只在**确实像信封**（顶层同时有 `code` 与 `data`）时取 `data`，
//! 否则原样返回 payload —— 上游万一去掉信封、或返回裸数组，也不会 panic。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

/// 上游业务成功码（契约：`code === 0`）
pub const CODE_OK: i64 = 0;

/// 顶层是否像有道信封：同时带 `code` 与 `data`。
pub fn is_envelope(payload: &Value) -> bool {
    payload.get("code").is_some() && payload.get("data").is_some()
}

/// 取业务数据：信封形态给 `data`，否则原样返回 payload。
pub fn data(payload: &Value) -> Value {
    if is_envelope(payload) {
        payload.get("data").cloned().unwrap_or(Value::Null)
    } else {
        payload.clone()
    }
}

/// 信封的业务码（缺失 / 非数字 → None）。
pub fn code(payload: &Value) -> Option<i64> {
    payload.get("code").and_then(Value::as_i64)
}

/// 业务失败文案：优先 `message`，其次 `msg`；都空时给 `code=<n>`。
pub fn message(payload: &Value) -> String {
    let detail = payload
        .get("message")
        .or_else(|| payload.get("msg"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !detail.is_empty() {
        return detail.to_string();
    }
    match code(payload) {
        Some(code) => format!("code={code}"),
        None => String::new(),
    }
}

/// 业务失败检测：顶层有数字 `code` 且 `!= 0` 时返回可读文案，否则 None。
pub fn business_error(payload: &Value) -> Option<String> {
    let code = code(payload)?;
    if code == CODE_OK {
        return None;
    }
    Some(message(payload))
}

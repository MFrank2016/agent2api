//! 剥离响应开头思考标签的开关：`GET/PUT /api/think-strip`。
//!
//! ── 这个开关做什么 ──────────────────────────────────────────
//! 打开后，下发给客户端的正文若以 `<think>…</think>`（以及 `THINK_TAGS` 里的
//! 另外三组）**开头**，就把那一块连同标签一起剥掉；正文中间的标签一律不动
//! （那是用户内容）。判据与执行链在 `core::middleware::think_tags`，本模块只是
//! 那个全局默认值的读写入口。
//!
//! ── 为什么只有一个开关 ──────────────────────────────────────
//! 判定只依赖文本本身（与哪家上游无关），没有词表、没有作用范围可配；形态与
//! `/api/sanitize`、`/api/keepalive-strip` 一致 —— GET 读、PUT 写、响应体就是
//! 新状态。**逐家覆盖**不在这里：`providerOptions` 里的同名键可逐家盖过本全局
//! 值（见 `core::provider_options` 的 `KNOWN_KEYS` 说明）。
//!
//! ── 写盘位置 ────────────────────────────────────────────────
//! `config` 的 `stripLeadingThink` 键（统一库 `kv` 表里的配置行），默认关。
//! 转发层每次装配下发策略时读快照，改完下一个请求立即生效，不重启进程。
//!
//! ── 为什么挂 protected ──────────────────────────────────────
//! 它改的是客户端看到的字节（开的那一刻起下发正文不再与上游逐字一致），
//! 敏感度与 `/api/keepalive-strip`、`/api/sanitize` 同级。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/think-strip —— 当前开关状态
pub async fn get_think_strip(State(_state): State<ServerState>) -> Response {
    ok_json(state_json())
}

/// PUT /api/think-strip —— body `{stripLeadingThink: true|false}`
///
/// 写配置，下一个请求立即生效（策略装配逐请求读快照，不重启进程）。
/// 开与关都记一条事件日志：开的那一刻起，下发正文开头的那块思考标签不再到达
/// 客户端；关的那一刻起又变回上游原样字节 —— 排查「开头的思考块怎么没了」时，
/// 日志里必须查得到这条线。
pub async fn put_think_strip(State(_state): State<ServerState>, body: Bytes) -> Response {
    put_response(&body)
}

/// PUT 的完整逻辑（不含 `State` 提取）。
///
/// ── 为什么与 handler 分开 ──────────────────────────────────
/// handler 需要的 `ServerState` 只能由 `ServerState::bootstrap` 构造（会开真库、
/// 写 `~/.agent2api`），用例驱动不了它。把「解析 → 校验 → 写 → 回状态」整段
/// 抽出来，用例就能对**真实的状态码与响应体**下断言 —— 比只测一个
/// `patch_from` 更贴近接口契约（与 `api::keepalive` 同形的判定也一并钉住）。
fn put_response(body: &[u8]) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let key = config::KEY_STRIP_LEADING_THINK;
    let Some(value) = object.get(key) else {
        return errors::management_error(400, format!("缺少 {key}"));
    };
    let Some(enabled) = value.as_bool() else {
        return errors::management_error(400, format!("{key} 必须是 true 或 false"));
    };
    if !config::set_strip_leading_think(enabled) {
        logging::log(
            "[Config]",
            "⚠️  思考标签剥离开关写入配置失败，本次运行内仍生效",
        );
    }
    logging::log(
        "[Config]",
        if enabled {
            "开头思考标签剥离已开启：响应正文开头的 <think> 块将被剥掉"
        } else {
            "开头思考标签剥离已关闭：下发正文恢复为上游原样字节"
        },
    );
    ok_json(state_json())
}

/// 开关状态（GET 与 PUT 共用，前端直接用响应刷新界面）
fn state_json() -> Value {
    json!({
        config::KEY_STRIP_LEADING_THINK: config::strip_leading_think(),
    })
}

#[cfg(test)]
mod tests {
    //! PUT 的三条判据 + 写后 GET 立刻读到（真实状态码与响应体）。
    //!
    //! ── 为什么要用真 handler 逻辑而不是只看一个 patch 函数 ──────
    //! 本模块的判定是「缺键 / 类型不符 → 400」「合法 → 200 且响应体就是新状态」，
    //! 这几条都在 [`put_response`] 里；只测一个纯函数会让「400 文案接错分支」
    //! 这类写歪照样绿。

    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    /// 用例串行跑：本模块改的是进程级配置快照，与 `core::provider_options`
    /// 的用例共用同一把锁（`config::update` 是非原子读改写）。
    fn hold_config() -> std::sync::MutexGuard<'static, ()> {
        crate::server::core::provider_options::CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 用例结束后把 `stripLeadingThink` 恢复原样（Drop 保证 panic 时也执行）。
    ///
    /// 不恢复的话，任何**先跑过本用例**再读这个全局键的断言都会看到它刚写的值
    /// —— 这个仓库已经吃过一次全局状态污染的苦（`providerOptions` 那组用例）。
    struct RestoreThink {
        previous: Option<Value>,
    }

    impl RestoreThink {
        fn capture() -> Self {
            Self {
                previous: config::with_raw(|raw| raw.get(config::KEY_STRIP_LEADING_THINK).cloned())
                    .flatten(),
            }
        }
    }

    impl Drop for RestoreThink {
        fn drop(&mut self) {
            // 原先没有这个键 → 恢复成 `false`（读侧 `unwrap_or(false)` 的语义与
            // 「键不存在」逐字相同，但写回一个显式值更不容易被后续读者误解）。
            let value = self.previous.clone().unwrap_or(Value::Bool(false));
            let _ = config::update_raw_field(config::KEY_STRIP_LEADING_THINK, value);
        }
    }

    /// 读回 JSON 响应体（断言用；测试里允许 expect）。
    async fn json_of(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("响应体读取不应失败");
        serde_json::from_slice(&bytes).expect("响应体必须是 JSON")
    }

    #[tokio::test]
    async fn put_then_get_round_trips_the_switch() {
        let _guard = hold_config();
        let _restore = RestoreThink::capture();
        // 打开 → 200，响应体里就是新状态
        let response = put_response(br#"{"stripLeadingThink":true}"#);
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(
            body.pointer("/data/stripLeadingThink")
                .and_then(Value::as_bool),
            Some(true),
            "PUT 的响应体必须带上写入后的值"
        );
        // GET 的响应体与 PUT 共用 `state_json()`（GET handler 只有一行
        // `ok_json(state_json())`，没有别的逻辑可测）：这里直接断言它读到了新值，
        // 也就钉住了「写后读立刻生效、不必重启」。
        let body = json_of(ok_json(state_json())).await;
        assert_eq!(
            body.pointer("/data/stripLeadingThink")
                .and_then(Value::as_bool),
            Some(true),
            "写后读必须立刻生效"
        );
        // 再关回去：关也必须真的落进快照
        let response = put_response(br#"{"stripLeadingThink":false}"#);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!config::strip_leading_think(), "关掉后必须读到 false");
    }

    #[tokio::test]
    async fn a_missing_or_mistyped_key_is_a_400() {
        let _guard = hold_config();
        let _restore = RestoreThink::capture();
        for (body, expected) in [
            (&b"{}"[..], "缺少 stripLeadingThink"),
            (br#"{"stripLeadingThink":"true"}"#, "必须是 true 或 false"),
            (br#"{"stripLeadingThink":1}"#, "必须是 true 或 false"),
            (br#"{"stripLeadingThink":null}"#, "必须是 true 或 false"),
            (br#"[true]"#, "请求体必须是 JSON 对象"),
            (b"not json", "请求体不是合法 JSON"),
        ] {
            let response = put_response(body);
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "非法请求体 {body:?} 必须是 400"
            );
            let payload = json_of(response).await;
            assert_eq!(
                payload.get("success").and_then(Value::as_bool),
                Some(false),
                "400 必须是管理 API 的失败形状"
            );
            let message = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                message.contains(expected),
                "文案 {message:?} 必须说清问题（期望包含 {expected:?}）"
            );
        }
    }
}

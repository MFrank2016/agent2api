//! 每 provider 的行为开关读写入口：`GET/PUT /api/provider-options`。
//!
//! ── 为什么需要这条端点 ──────────────────────────────────────
//! 开关的**存储**与**读取**早就在 `core::provider_options` 里（`providerOptions`
//! 配置键 + `is_enabled`），但写侧 `set()` 一直没有生产调用方 —— 用户只能手改
//! 库里的 `kv` 行才配得动。本模块补上那条缺失的入口，让逐家开关真正可达。
//!
//! ── 形状 ────────────────────────────────────────────────────
//!   - `GET`  → `{providerOptions: {…}}`（没配过就是空对象）
//!   - `PUT`  body `{providerId, key, value}` → 调 `provider_options::set(...)`，
//!     `Err` 一律 400（文案就是 `set` 的报错原文：它已经说清是哪个键/哪家不认），
//!     成功返回与 GET 相同的形状（前端直接用响应刷新界面）。
//!
//! ── 为什么不接受整块替换 ────────────────────────────────────
//! 逐项 PUT 与 `set()` 的语义一致（它只改一家的一项），也避免「整份回传」把
//! 用户没显示的键顺手抹掉 —— 那条路在 `providerOptions` 上比单开关端点危险得多
//! （它按家分组、可能有前端不认识的键）。
//!
//! ── 哪些 provider 认哪些键 ──────────────────────────────────
//! `KNOWN_KEYS` 是内置家与自定义家**共用**的一张表，但两类家的**读取能力**不同：
//! 内置家两项都读，自定义家只读 `stripLeadingThink`（保活换行丢弃恒关，见
//! `core::provider_options::KNOWN_KEYS` 的说明）。本端点不因此拒绝写入 ——
//! 写进去的 `stripNewlineKeepalive` 对自定义家仍是「存着但当前不生效」，那是
//! 有意的保守取舍，理由写在那张表的文档里。
//!
//! ── 为什么挂 protected ──────────────────────────────────────
//! 这些开关改的是出站请求 / 下发字节（脱敏、保活帧、思考标签），敏感度与
//! `/api/sanitize`、`/api/keepalive-strip` 同级。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::server::config;
use crate::server::core::provider_options;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::ServerState;

/// GET /api/provider-options —— 整块 `providerOptions`（没配过就是 `{}`）
pub async fn get_provider_options(State(_state): State<ServerState>) -> Response {
    ok_json(state_json())
}

/// PUT /api/provider-options —— body `{providerId, key, value}`
///
/// 写配置，下一个请求立即生效（读侧逐请求读内存快照，不重启进程）。
/// 开与关都记一条事件日志（由 `provider_options::set` 落，文案带家与键）。
pub async fn put_provider_options(State(_state): State<ServerState>, body: Bytes) -> Response {
    put_response(&body)
}

/// PUT 的完整逻辑（不含 `State` 提取）。
///
/// ── 为什么与 handler 分开 ──────────────────────────────────
/// handler 需要的 `ServerState` 只能由 `ServerState::bootstrap` 构造（会开真库、
/// 写 `~/.agent2api`），用例驱动不了它。把「解析 → 校验 → 写 → 回整块」整段
/// 抽出来，用例就能对**真实的状态码与响应体**下断言 —— 包括「PUT 之后 GET 读到
/// 同一个值」这条本端点存在的全部理由。
fn put_response(body: &[u8]) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let Some(provider_id) = object.get("providerId").and_then(Value::as_str) else {
        return errors::management_error(400, "缺少 providerId（字符串）");
    };
    let Some(key) = object.get("key").and_then(Value::as_str) else {
        return errors::management_error(400, "缺少 key（字符串）");
    };
    let Some(value) = object.get("value").and_then(Value::as_bool) else {
        return errors::management_error(400, "缺少 value（true 或 false）");
    };
    // `set` 的报错文案就是接口契约（未知键会列出允许的键、未知家会点名 id），
    // 所以原样透给 400，不在这一层另写一份判据。
    if let Err(message) = provider_options::set(provider_id, key, value) {
        return errors::management_error(400, message);
    }
    ok_json(state_json())
}

/// 整块状态（GET 与 PUT 共用，前端直接用响应刷新界面）。
///
/// 只取配置里的 `providerOptions` 键：没配过（或配置尚未初始化）时给空对象，
/// 与读侧 `is_enabled` 的「查不到 → 走 default」逐字同口径。
fn state_json() -> Value {
    let options = config::with_raw(|raw| raw.get("providerOptions").cloned())
        .flatten()
        .unwrap_or_else(|| Value::Object(Map::new()));
    json!({ "providerOptions": options })
}

#[cfg(test)]
mod tests {
    //! 两条契约：PUT → GET 往返、未知键 400。
    //!
    //! ── 为什么要用真 handler 逻辑 ──────────────────────────────
    //! 本端点存在的全部理由是「`provider_options::set` 终于有生产调用方了」，
    //! 所以用例必须走**这条路由的逻辑**去写，再用同一个 `state_json()` 读回来 ——
    //! 只测 `set` 会退化成「库里那条已有的用例」，证明不了入口接通。

    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    /// 用例串行跑：本模块改的是进程级配置快照，与 `core::provider_options`
    /// 的用例共用同一把锁（`config::update` 是非原子读改写）。
    fn hold_config() -> std::sync::MutexGuard<'static, ()> {
        provider_options::CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 快照未装入时补一个空的 `providerOptions`：`set()` 把「未初始化」当硬错误
    /// （写进去会丢整份配置），而单元测试进程从不调 `config::init`。
    /// 已有快照时什么都不做，别盖掉兄弟模块写进去的键。
    fn ensure_snapshot() {
        if config::with_raw(|_| ()).is_some() {
            return;
        }
        let _ = config::update_raw_field("providerOptions", Value::Object(Map::new()));
    }

    /// 用例结束后把 `providerOptions` 恢复原样（Drop 保证 panic 时也执行）。
    struct RestoreOptions {
        previous: Option<Value>,
    }

    impl RestoreOptions {
        fn capture() -> Self {
            Self {
                previous: config::with_raw(|raw| raw.get("providerOptions").cloned()).flatten(),
            }
        }
    }

    impl Drop for RestoreOptions {
        fn drop(&mut self) {
            let value = self
                .previous
                .clone()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let _ = config::update_raw_field("providerOptions", value);
        }
    }

    /// 读回 JSON 响应体（测试里允许 expect）。
    async fn json_of(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("响应体读取不应失败");
        serde_json::from_slice(&bytes).expect("响应体必须是 JSON")
    }

    #[tokio::test]
    async fn put_then_get_round_trips_one_provider_switch() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        ensure_snapshot();
        let key = config::KEY_STRIP_LEADING_THINK;

        let response = put_response(
            json!({ "providerId": "qoder", "key": key, "value": true })
                .to_string()
                .as_bytes(),
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(
            body.pointer(&format!("/data/providerOptions/qoder/{key}"))
                .and_then(Value::as_bool),
            Some(true),
            "PUT 的响应体必须已经带上新值"
        );

        // GET 的响应体与 PUT 共用 `state_json()`：写后读立刻生效（内存快照已刷新）。
        let body = json_of(ok_json(state_json())).await;
        assert_eq!(
            body.pointer(&format!("/data/providerOptions/qoder/{key}"))
                .and_then(Value::as_bool),
            Some(true),
            "写后读必须立刻生效（这是本端点存在的全部理由）"
        );
        // 读侧也认这个值（证明写的是真开关，不是一条死记录）
        assert!(
            provider_options::is_enabled("qoder", key, false),
            "写进去的开关必须被 is_enabled 读到"
        );

        // 关回去同样走端点
        let response = put_response(
            json!({ "providerId": "qoder", "key": key, "value": false })
                .to_string()
                .as_bytes(),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !provider_options::is_enabled("qoder", key, true),
            "关掉后必须读到 false"
        );
    }

    #[tokio::test]
    async fn an_unknown_key_is_rejected_with_400() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        ensure_snapshot();

        let response = put_response(
            json!({ "providerId": "qoder", "key": "nonexistentKey", "value": true })
                .to_string()
                .as_bytes(),
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_of(response).await;
        assert_eq!(body.get("success").and_then(Value::as_bool), Some(false));
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            message.contains("未知的 provider 开关"),
            "文案必须点明是未知键（拿到的是: {message}）"
        );
        assert!(
            message.contains(config::KEY_STRIP_LEADING_THINK),
            "文案必须列出允许的键（拿到的是: {message}）"
        );
        // 被拒绝的写入不许在配置里留下痕迹
        assert!(
            provider_options::get("qoder", "nonexistentKey").is_none(),
            "未知键不该被写进快照"
        );
    }

    /// 请求体形状的三道必填项：缺一即 400（不让「什么都没改」返回 200）。
    #[tokio::test]
    async fn a_malformed_body_is_rejected_with_400() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        ensure_snapshot();

        for (body, expected) in [
            (json!({}), "providerId"),
            (json!({ "providerId": "qoder" }), "key"),
            (
                json!({ "providerId": "qoder", "key": "stripLeadingThink" }),
                "value",
            ),
            (
                json!({ "providerId": "qoder", "key": "stripLeadingThink", "value": "true" }),
                "value",
            ),
            (json!([1, 2]), "请求体必须是 JSON 对象"),
        ] {
            let response = put_response(body.to_string().as_bytes());
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "非法体 {body} 必须 400"
            );
            let payload = json_of(response).await;
            let message = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                message.contains(expected),
                "文案 {message:?} 必须说清缺什么（期望包含 {expected:?}）"
            );
        }
    }
}

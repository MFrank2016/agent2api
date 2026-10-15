//! GET/POST `/api/routing-strategy` —— **统一**的账号路由设置端点。
//!
//! ── 这条端点管什么 ──────────────────────────────────────────
//! 它把两套路由设置合成一份配置：
//!   · **选路策略**（`core::routing::strategy` 的 7 档，**逐家**）：全局默认
//!     `default` + 逐家覆盖 `providers`。取值 `priority`（默认，行为与改造前
//!     逐字相同）/ `least-connections` / `round-robin`（家内轮询）/
//!     `global-round-robin`（整表轮询，上游 `rotate`）/ `least-recently-used`
//!     / `balance-weighted` / `priority-weighted`。
//!   · **上游 v3.0.2 引入的四个维度**：会话粘性（`routingSticky`）、失败退避
//!     （`routingFailoverBackoff`）、限流垫底（`routingSink`）、熔断
//!     （`routingBreaker*`）。语义与默认值原样保留。
//!
//! `/api/routing`（`api::routing_api`）是本端点的**兼容别名**：读写同一份配置，
//! 额外认一个旧的字符串字段 `routingStrategy`（`order` / `rotate` / `provider` / `usage`）。
//!
//! ── 为什么与 /api/prompt 分成两条端点 ───────────────────────
//! 两者读写的是不同的配置键、不同的语义；形状也不同（这条是「枚举 + 一张
//! 逐家表 + 四个附加维度」，那条还带提示词正文与降级状态）。与 `/api/retry`
//! 更接近：多字段 + 允许部分更新 + 返回生效后的全量值 + 无副作用（保存后对
//! 下一个请求生效，不重启进程）。
//!
//! ── 校验为什么在写侧做（与读侧的回落口径不同）───────────────
//! 读侧（`config::routing_strategy_from`）遇到非法策略值会**回落到 `priority`**：
//! 桌面应用不能因为一个手改坏的配置项起不来、也不能因此拒绝转发。写侧则相反
//! —— 用户就站在这里，非法档位要当场知道（400 + 可读原因），而不是保存成功后
//! 在界面上发现「还是旧的那一档」才知道没生效。
//!
//! ── `providers` 的替换语义 ──────────────────────────────────
//! 请求体里**出现** `providers` 就**整体替换**这张表（不是逐键合并）：界面改一家
//! 的策略时整份回传，「某一家改回跟随默认」= 从表里删掉它 —— 只有整体替换能
//! 表达删除。未带 `providers`（或传 `null`）则这张表原样不动（部分更新语义）。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::server::config::{
    self, RoutingPatch, RoutingStrategyPatch, RoutingStrategySettings, StickyMode,
    KEY_ROUTING_STRATEGY_DEFAULT, KEY_ROUTING_STRATEGY_PROVIDERS,
};
use crate::server::core::routing::RoutingStrategy;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/routing-strategy
pub async fn get_routing_strategy(State(_state): State<ServerState>) -> Response {
    ok_json(routing_json())
}

/// POST /api/routing-strategy —— body 各键全部可选（不给 = 不动）。
///
/// 校验通过后：写配置库 → 返回**生效后**的值（前端直接用响应刷新界面，不必再
/// GET 一次）。非法值 → 400 且什么都不改（先整单校验再落盘，与 `/api/retry` 同一顺序）。
pub async fn put_routing_strategy(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    if let Err(message) = apply_object(object) {
        return errors::management_error(400, message);
    }
    let strategies = config::routing_strategy_settings();
    logging::log(
        "[Config]",
        &format!(
            "路由设置已更新: 默认 {} / 逐家 {}",
            strategies.default.as_str(),
            describe_providers(&strategies)
        ),
    );
    ok_json(routing_json())
}

/// 校验并落盘一份统一的路由 patch（`/api/routing` 与 `/api/routing-strategy`
/// 共用）。非法值给**可直接显示**的 400 文案；成功返回 `Ok(())`。
///
/// 独立成纯逻辑（不碰 `State`）便于单测覆盖「非法档位 / 非对象 / 空 id / 越界」
/// 等分支。
pub(super) fn apply_object(object: &Map<String, Value>) -> Result<(), String> {
    let (strategy_patch, routing_patch) = parse_patch(object)?;
    let strategy_changed = strategy_patch.default.is_some() || strategy_patch.providers.is_some();
    if strategy_changed && !config::set_routing_strategy(strategy_patch) {
        // 写盘失败：内存快照已更新（本次运行仍生效），但重启后会回到旧值 ——
        // 必须让用户知道，否则「改了设置重启又变回去」会被当成玄学问题
        logging::log("[Config]", "⚠️  选路策略设置写入失败，本次运行内仍生效");
    }
    if !routing_patch_is_empty(&routing_patch) && !config::set_routing(routing_patch) {
        logging::log("[Config]", "⚠️  路由附加设置写入失败，本次运行内仍生效");
    }
    Ok(())
}

/// 校验请求体、产出两份部分更新入参（策略 + 附加维度）。
fn parse_patch(object: &Map<String, Value>) -> Result<(RoutingStrategyPatch, RoutingPatch), String> {
    let strategy = parse_strategy_patch(object)?;
    let routing = parse_routing_patch(object)?;
    Ok((strategy, routing))
}

/// 选路策略那半边的解析（`default` + `providers`）。
fn parse_strategy_patch(object: &Map<String, Value>) -> Result<RoutingStrategyPatch, String> {
    let mut patch = RoutingStrategyPatch::default();

    // ── default（缺省 / null = 这一项不改）────────────────────────
    match object.get(KEY_ROUTING_STRATEGY_DEFAULT) {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => match RoutingStrategy::parse(text) {
            Some(strategy) => patch.default = Some(strategy),
            None => {
                return Err(format!(
                    "{KEY_ROUTING_STRATEGY_DEFAULT} 只认 {}（收到: {text}）",
                    allowed()
                ))
            }
        },
        Some(other) => {
            return Err(format!("{KEY_ROUTING_STRATEGY_DEFAULT} 必须是字符串（收到: {other}）"))
        }
    }

    // ── providers（缺省 / null = 这张表不改；出现即整体替换）─────────
    if let Some(value) = object.get(KEY_ROUTING_STRATEGY_PROVIDERS) {
        if !value.is_null() {
            let Some(entries) = value.as_object() else {
                return Err(format!(
                    "{KEY_ROUTING_STRATEGY_PROVIDERS} 必须是对象或 null（收到: {value}）"
                ));
            };
            let mut table = std::collections::BTreeMap::new();
            for (id, item) in entries {
                let id = id.trim();
                if id.is_empty() {
                    return Err(format!(
                        "{KEY_ROUTING_STRATEGY_PROVIDERS} 里出现了空的 provider id"
                    ));
                }
                let Some(text) = item.as_str() else {
                    return Err(format!(
                        "{KEY_ROUTING_STRATEGY_PROVIDERS}.{id} 必须是字符串（收到: {item}）"
                    ));
                };
                let Some(strategy) = RoutingStrategy::parse(text) else {
                    return Err(format!(
                        "{KEY_ROUTING_STRATEGY_PROVIDERS}.{id} 只认 {}（收到: {text}）",
                        allowed()
                    ));
                };
                table.insert(id.to_string(), strategy);
            }
            patch.providers = Some(table);
        }
    }

    Ok(patch)
}

/// 附加维度那半边的解析（粘性 / 退避 / 垫底 / 熔断）。
fn parse_routing_patch(object: &Map<String, Value>) -> Result<RoutingPatch, String> {
    let mut patch = RoutingPatch::default();

    if let Some(value) = object.get(config::KEY_ROUTING_STICKY) {
        if !value.is_null() {
            let Some(text) = value.as_str() else {
                return Err(format!("{} 必须是字符串", config::KEY_ROUTING_STICKY));
            };
            let known = [
                config::STICKY_AUTO,
                config::STICKY_SESSION,
                config::STICKY_TURN,
                config::STICKY_OFF,
            ];
            if !known.contains(&text) {
                return Err(format!(
                    "未知的会话粘性档位「{text}」（只支持 {}）",
                    known.join(" / ")
                ));
            }
            patch.sticky = Some(StickyMode::from_id(text));
        }
    }

    patch.failover_backoff = parse_bool(object, config::KEY_ROUTING_FAILOVER_BACKOFF)?;
    patch.sink = parse_bool(object, config::KEY_ROUTING_SINK)?;
    patch.breaker_enabled = parse_bool(object, config::KEY_ROUTING_BREAKER_ENABLED)?;
    patch.breaker_threshold = parse_bounded_int(
        object,
        config::KEY_ROUTING_BREAKER_THRESHOLD,
        config::ROUTING_BREAKER_MIN_THRESHOLD,
        config::ROUTING_BREAKER_MAX_THRESHOLD,
    )?;
    patch.breaker_timeout_seconds = parse_bounded_int(
        object,
        config::KEY_ROUTING_BREAKER_TIMEOUT,
        config::ROUTING_BREAKER_MIN_TIMEOUT,
        config::ROUTING_BREAKER_MAX_TIMEOUT,
    )?;
    patch.breaker_success_threshold = parse_bounded_int(
        object,
        config::KEY_ROUTING_BREAKER_SUCCESS,
        config::ROUTING_BREAKER_MIN_SUCCESS,
        config::ROUTING_BREAKER_MAX_SUCCESS,
    )?;

    Ok(patch)
}

/// `null` / 缺失 = 不动；布尔原样；其余 400。
fn parse_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key} 必须是布尔值")),
    }
}

/// `null` / 缺失 = 不动；`min..=max` 的整数原样；其余 400。
fn parse_bounded_int(
    object: &Map<String, Value>,
    key: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_i64() {
            Some(count) if (min..=max).contains(&count) => Ok(Some(count)),
            _ => Err(format!("{key} 必须是 {min}~{max} 的整数")),
        },
    }
}

/// 附加维度是否一项都没给（全 `None` = 不写盘，省一次无意义的持久化）。
fn routing_patch_is_empty(patch: &RoutingPatch) -> bool {
    patch.sticky.is_none()
        && patch.failover_backoff.is_none()
        && patch.sink.is_none()
        && patch.breaker_enabled.is_none()
        && patch.breaker_threshold.is_none()
        && patch.breaker_timeout_seconds.is_none()
        && patch.breaker_success_threshold.is_none()
}

/// 统一的路由设置响应体（GET 与 POST 共用；`/api/routing` 别名也复用它）。
///
/// 键名对前端是**契约**（`settings-state.ts` / `accounts-*` 逐字对齐），因此都用
/// 常量标识符而不是手写字符串（与 `timeouts_json` 同一手法）。
pub(super) fn routing_json() -> Value {
    let strategies = config::routing_strategy_settings();
    let settings = config::routing_settings();
    json!({
        KEY_ROUTING_STRATEGY_DEFAULT: strategies.default.as_str(),
        KEY_ROUTING_STRATEGY_PROVIDERS: providers_json(&strategies),
        // 可选项清单（含中文 label / hint）：界面不写死任何一档 —— 新增一档时
        // 只改后端，下拉直接长出来
        "strategies": strategies_json(),
        config::KEY_ROUTING_STICKY: settings.sticky.as_str(),
        config::KEY_ROUTING_FAILOVER_BACKOFF: settings.failover_backoff,
        config::KEY_ROUTING_SINK: settings.sink,
        config::KEY_ROUTING_BREAKER_ENABLED: settings.breaker_enabled,
        config::KEY_ROUTING_BREAKER_THRESHOLD: settings.breaker_threshold,
        config::KEY_ROUTING_BREAKER_TIMEOUT: settings.breaker_timeout_seconds,
        config::KEY_ROUTING_BREAKER_SUCCESS: settings.breaker_success_threshold,
    })
}

/// 逐家覆盖的 JSON 形态：`{ "<providerId>": "<strategy>" }`（只含配置过的家）
fn providers_json(settings: &RoutingStrategySettings) -> Value {
    let mut map = Map::new();
    for (id, strategy) in &settings.providers {
        map.insert(id.clone(), Value::String(strategy.as_str().to_string()));
    }
    Value::Object(map)
}

/// 全部档位的 JSON 形态：`[{ "id", "label", "hint" }]`
fn strategies_json() -> Value {
    Value::Array(
        RoutingStrategy::ALL
            .iter()
            .map(|strategy| {
                json!({
                    "id": strategy.as_str(),
                    "label": strategy.label(),
                    "hint": strategy.hint(),
                })
            })
            .collect(),
    )
}

/// 日志里「逐家」那一段的可读形态（`workbuddy=round-robin, raccoon=priority`）
fn describe_providers(settings: &RoutingStrategySettings) -> String {
    if settings.providers.is_empty() {
        return "（无）".to_string();
    }
    settings
        .providers
        .iter()
        .map(|(id, strategy)| format!("{id}={}", strategy.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 400 文案里列出的合法档位
fn allowed() -> String {
    RoutingStrategy::ALL
        .iter()
        .map(|strategy| strategy.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    #[test]
    fn default_only_updates_default_and_keeps_providers() {
        let patch = parse_strategy_patch(&object(json!({ "default": "round-robin" })))
            .unwrap_or_default();
        assert_eq!(patch.default, Some(RoutingStrategy::RoundRobin));
        assert!(patch.providers.is_none(), "未带 providers 时这张表不动");
    }

    #[test]
    fn providers_replace_table_and_accept_unknown_ids() {
        let patch = parse_strategy_patch(&object(json!({
            "providers": { "workbuddy": "least-connections", "unknown-x": "round-robin" }
        })))
        .unwrap_or_default();
        let table = patch.providers.unwrap_or_default();
        assert_eq!(table.get("workbuddy"), Some(&RoutingStrategy::LeastConnections));
        assert_eq!(table.get("unknown-x"), Some(&RoutingStrategy::RoundRobin));
    }

    #[test]
    fn null_fields_leave_values_untouched() {
        let patch = parse_strategy_patch(&object(json!({ "default": null, "providers": null })))
            .unwrap_or_default();
        assert_eq!(patch.default, None);
        assert!(patch.providers.is_none());
        let extra = parse_routing_patch(&object(json!({
            "routingSticky": null,
            "routingFailoverBackoff": null,
            "routingBreakerThreshold": null,
        })))
        .unwrap_or_default();
        assert!(routing_patch_is_empty(&extra));
    }

    #[test]
    fn invalid_default_is_rejected() {
        assert!(parse_strategy_patch(&object(json!({ "default": "bogus" }))).is_err());
    }

    #[test]
    fn invalid_provider_strategy_is_rejected() {
        assert!(parse_strategy_patch(&object(json!({
            "providers": { "workbuddy": "bogus" }
        })))
        .is_err());
    }

    #[test]
    fn non_object_providers_is_rejected() {
        assert!(parse_strategy_patch(&object(json!({ "providers": "round-robin" }))).is_err());
    }

    #[test]
    fn extra_dimensions_parse_with_validation() {
        let patch = parse_routing_patch(&object(json!({
            "routingSticky": "session",
            "routingFailoverBackoff": false,
            "routingSink": true,
            "routingBreakerEnabled": true,
            "routingBreakerThreshold": 7,
            "routingBreakerTimeoutSeconds": 120,
            "routingBreakerSuccessThreshold": 3,
        })))
        .unwrap_or_default();
        assert_eq!(patch.sticky, Some(StickyMode::Session));
        assert_eq!(patch.failover_backoff, Some(false));
        assert_eq!(patch.sink, Some(true));
        assert_eq!(patch.breaker_enabled, Some(true));
        assert_eq!(patch.breaker_threshold, Some(7));
        assert_eq!(patch.breaker_timeout_seconds, Some(120));
        assert_eq!(patch.breaker_success_threshold, Some(3));
    }

    #[test]
    fn invalid_sticky_and_out_of_range_breaker_are_rejected() {
        assert!(parse_routing_patch(&object(json!({ "routingSticky": "bogus" }))).is_err());
        assert!(parse_routing_patch(&object(json!({ "routingBreakerThreshold": 0 }))).is_err());
        assert!(parse_routing_patch(&object(json!({ "routingBreakerTimeoutSeconds": 5 }))).is_err());
        assert!(parse_routing_patch(&object(json!({ "routingSink": "yes" }))).is_err());
    }

    #[test]
    fn strategies_list_has_all_values_with_labels() {
        let value = strategies_json();
        let items = value.as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 7);
        for item in items {
            assert!(item.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty()));
            assert!(item.get("label").and_then(Value::as_str).is_some_and(|label| !label.is_empty()));
        }
    }
}

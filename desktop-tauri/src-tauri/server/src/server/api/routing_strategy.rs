//! GET/POST `/api/routing-strategy` —— 账号选路策略（全局默认 + 逐家覆盖）。
//!
//! ── 这条端点管什么 ──────────────────────────────────────────
//! 网关在一组**可用账号**之间按什么规则挑一个来转发（见
//! `core::routing::strategy` 的模块头）。今天有六档：`priority`（默认，行为与
//! 改造前逐字相同）/ `least-connections` / `round-robin` / `least-recently-used`
//! / `balance-weighted` / `priority-weighted`。全局默认对所有未单独配置的家
//! 生效，`providers` 给某一家换一种（**稀疏表**：只写想单独配置的家）。
//!
//! ── 为什么与 /api/prompt 分成两条端点 ───────────────────────
//! 两者读写的是不同的配置键、不同的语义；形状也不同（这条是「枚举 + 一张
//! 逐家表」，那条还带提示词正文与降级状态）。与 `/api/retry` 更接近：多字段 +
//! 允许部分更新 + 返回生效后的全量值 + 无副作用（保存后对下一个请求生效，
//! 不重启进程）。
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
    self, RoutingStrategyPatch, RoutingStrategySettings, KEY_ROUTING_STRATEGY_DEFAULT,
    KEY_ROUTING_STRATEGY_PROVIDERS,
};
use crate::server::core::routing::RoutingStrategy;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/routing-strategy
pub async fn get_routing_strategy(State(_state): State<ServerState>) -> Response {
    ok_json(routing_json(&config::routing_strategy_settings()))
}

/// POST /api/routing-strategy —— body `{ "default"?: <strategy>, "providers"?: { "<id>": <strategy> } }`
///
/// 允许部分字段（未出现的项保持原值，`null` 同义）。校验通过后：写配置库 →
/// 返回**生效后**的值（前端直接用响应刷新界面，不必再 GET 一次）。
/// 非法策略 id → 400 且什么都不改（先整单校验再落盘，与 `/api/retry` 同一顺序）。
pub async fn put_routing_strategy(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let patch = match parse_patch(object) {
        Ok(patch) => patch,
        Err(message) => return errors::management_error(400, message),
    };

    if !config::set_routing_strategy(patch) {
        // 写盘失败：内存快照已更新（本次运行仍生效），但重启后会回到旧值 ——
        // 必须让用户知道，否则「改了设置重启又变回去」会被当成玄学问题
        logging::log("[Config]", "⚠️  选路策略设置写入失败，本次运行内仍生效");
    }
    let settings = config::routing_strategy_settings();
    logging::log(
        "[Config]",
        &format!(
            "选路策略已更新: 默认 {} / 逐家 {}",
            settings.default.as_str(),
            describe_providers(&settings)
        ),
    );
    ok_json(routing_json(&settings))
}

/// 校验请求体、产出部分更新入参（非法值给**可直接显示**的 400 文案）。
///
/// 独立成纯函数（不碰 `State`）便于单测覆盖「非法档位 / 非对象 / 空 id」等分支。
fn parse_patch(object: &Map<String, Value>) -> Result<RoutingStrategyPatch, String> {
    let mut patch = RoutingStrategyPatch::default();

    // ── default（缺省 / null = 这一项不改）────────────────────────
    match object.get(KEY_ROUTING_STRATEGY_DEFAULT) {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => match RoutingStrategy::parse(text) {
            Some(strategy) => patch.default = Some(strategy),
            None => return Err(format!("{KEY_ROUTING_STRATEGY_DEFAULT} 只认 {}（收到: {text}）", allowed())),
        },
        Some(other) => {
            return Err(format!("{KEY_ROUTING_STRATEGY_DEFAULT} 必须是字符串（收到: {other}）"));
        }
    }

    // ── providers（缺省 / null = 这张表不改；出现即整体替换）─────────
    if let Some(value) = object.get(KEY_ROUTING_STRATEGY_PROVIDERS) {
        if !value.is_null() {
            let Some(entries) = value.as_object() else {
                return Err(format!("{KEY_ROUTING_STRATEGY_PROVIDERS} 必须是对象或 null（收到: {value}）"));
            };
            let mut table = std::collections::BTreeMap::new();
            for (id, item) in entries {
                let id = id.trim();
                if id.is_empty() {
                    return Err(format!("{KEY_ROUTING_STRATEGY_PROVIDERS} 里出现了空的 provider id"));
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

/// 响应体（GET 与 POST 共用；前端直接用响应刷新界面，不必再 GET 一次）。
///
/// 键名对前端是**契约**（`settings-panel.js` 逐字对齐），因此都用常量标识符
/// 而不是手写字符串（与 `timeouts_json` 同一手法）。
fn routing_json(settings: &RoutingStrategySettings) -> Value {
    json!({
        KEY_ROUTING_STRATEGY_DEFAULT: settings.default.as_str(),
        KEY_ROUTING_STRATEGY_PROVIDERS: providers_json(settings),
        // 可选项清单（含中文 label / hint）：界面不写死任何一档 —— 新增一档时
        // 只改后端，下拉直接长出来
        "strategies": strategies_json(),
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
        let patch = parse_patch(&object(json!({ "default": "round-robin" }))).unwrap_or_default();
        assert_eq!(patch.default, Some(RoutingStrategy::RoundRobin));
        assert!(patch.providers.is_none(), "未带 providers 时这张表不动");
    }

    #[test]
    fn providers_replace_table_and_accept_unknown_ids() {
        let patch = parse_patch(&object(json!({
            "providers": { "workbuddy": "least-connections", "unknown-x": "round-robin" }
        })))
        .unwrap_or_default();
        let table = patch.providers.unwrap_or_default();
        assert_eq!(table.get("workbuddy"), Some(&RoutingStrategy::LeastConnections));
        assert_eq!(table.get("unknown-x"), Some(&RoutingStrategy::RoundRobin));
    }

    #[test]
    fn null_fields_leave_values_untouched() {
        let patch = parse_patch(&object(json!({ "default": null, "providers": null }))).unwrap_or_default();
        assert_eq!(patch.default, None);
        assert!(patch.providers.is_none());
    }

    #[test]
    fn invalid_default_is_rejected() {
        let error = parse_patch(&object(json!({ "default": "bogus" })));
        assert!(error.is_err());
    }

    #[test]
    fn invalid_provider_strategy_is_rejected() {
        let error = parse_patch(&object(json!({ "providers": { "workbuddy": "bogus" } })));
        assert!(error.is_err());
    }

    #[test]
    fn non_object_providers_is_rejected() {
        let error = parse_patch(&object(json!({ "providers": "round-robin" })));
        assert!(error.is_err());
    }

    #[test]
    fn strategies_list_has_all_six_with_labels() {
        let value = strategies_json();
        let items = value.as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 6);
        for item in items {
            assert!(item.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty()));
            assert!(item.get("label").and_then(Value::as_str).is_some_and(|label| !label.is_empty()));
        }
    }
}

//! GET/PUT `/api/routing` + GET `/api/routing/queue` —— 账号路由设置的**兼容别名**
//! 与**队列可见化**。
//!
//! ── /api/routing（统一端点 `/api/routing-strategy` 的别名）─────────
//! 读写的是同一份配置（`api::routing_strategy::apply_object`）。GET 返回统一的
//! JSON，额外附一个旧字符串字段 `routingStrategy`（`order` / `rotate` / `usage`）
//! 供旧读取方；PUT 接受统一的 patch，也接受旧形态的 `routingStrategy` 字符串
//! （翻成统一的 `default`）。**新界面一律走 `/api/routing-strategy`**，这条别名
//! 只为兼容而保留。
//!
//! ── /api/routing/queue（队列可见化，账号页面板的数据源）────────
//! 回答「按当前策略，这个模型的候选账号会怎么排、谁被跳过、为什么」。
//! **排序实现与转发选路是同一个函数**（`routing::plan_route`）—— 转发那次
//! 怎么排，这里就怎么画；两处若各写一份，界面标的顺序迟早和实际转发分叉
//! （这正是「候选队列可见化」要消灭的那类问题）。
//!
//! 与 `/api/session` 的 `routedAccountId` 的关系：那个字段只回答「队首是谁」
//! （★ 的数据源，保持不变），本端点给的是**整张清单 + 原因句** —— 面板要
//! 展示每一位，顺带把被跳过者也摆出来（禁用 / 冷却 / 并发满 / 限制器 /
//! 无凭证各有各的原因，用户不用再猜）。
//!
//! `model` 查询参数可省：省略（或空）时按「不限模型」的全局队列展开
//! （不收窄到「提供该模型的家」）—— 面板在没有模型上下文时也有得画。

use std::collections::{HashMap, HashSet};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::providers::router;
use crate::server::core::routing::{self, RoutingStrategy};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::ServerState;

/// GET /api/routing —— 统一端点 `/api/routing-strategy` 的兼容别名。
pub async fn get_routing(State(_state): State<ServerState>) -> Response {
    ok_json(with_legacy_strategy(super::routing_strategy::routing_json()))
}

/// PUT /api/routing —— 统一 patch，另接受旧形态的 `routingStrategy` 字符串。
pub async fn put_routing(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(source) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let mut object = source.clone();
    // 旧形态：`routingStrategy` 是字符串（`order` / `rotate` / `usage` 或新档位 id）
    // → 翻成统一的 `default`。未知值当场 400（与统一端点同一口径）。
    if let Some(value) = object.remove(config::KEY_ROUTING_STRATEGY) {
        if !value.is_null() {
            let Some(text) = value.as_str() else {
                return errors::management_error(
                    400,
                    format!("{} 必须是字符串", config::KEY_ROUTING_STRATEGY),
                );
            };
            let Some(mapped) = legacy_strategy(text) else {
                return errors::management_error(400, format!("未知的路由策略「{text}」"));
            };
            object.insert(
                config::KEY_ROUTING_STRATEGY_DEFAULT.to_string(),
                Value::String(mapped.as_str().to_string()),
            );
        }
    }
    if let Err(message) = super::routing_strategy::apply_object(&object) {
        return errors::management_error(400, message);
    }
    ok_json(with_legacy_strategy(super::routing_strategy::routing_json()))
}

/// 旧字符串 → 策略（写侧口径：未知给 `None`，由调用方报 400）。
fn legacy_strategy(text: &str) -> Option<RoutingStrategy> {
    match text.trim().to_ascii_lowercase().as_str() {
        "order" => Some(RoutingStrategy::Priority),
        "rotate" => Some(RoutingStrategy::RoundRobin),
        "usage" => Some(RoutingStrategy::BalanceWeighted),
        _ => RoutingStrategy::parse(text),
    }
}

/// 在统一 JSON 上补一个旧字符串字段 `routingStrategy`（旧界面 / 旧脚本读它）。
/// 只映射 `priority` → `order`、`round-robin` → `rotate`，其余给新档位 id。
fn with_legacy_strategy(mut value: Value) -> Value {
    let default = config::routing_strategy_settings().default;
    let legacy = match default {
        RoutingStrategy::Priority => "order",
        RoutingStrategy::RoundRobin => "rotate",
        other => other.as_str(),
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            config::KEY_ROUTING_STRATEGY.to_string(),
            Value::String(legacy.to_string()),
        );
    }
    value
}

/// GET /api/routing/queue?model=
pub async fn get_routing_queue(
    State(state): State<ServerState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let model = params
        .get("model")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let snapshot = config::current();
    let accounts_value = state.store().list_accounts();
    let accounts = routing::accounts_of(&accounts_value);
    // 候选收窄：点名了模型 → 只留「清单里有它的家」（与转发同一张家清单，
    // `route_for_forward`）；没点名 → 全部账号。与 session.rs 的 ★ 推算同一取材。
    let providers: Vec<String> = match &model {
        Some(model) => router::route_for_forward(model),
        None => Vec::new(),
    };
    let scoped: Vec<Value> = match model.as_deref() {
        Some(_) if providers.is_empty() => accounts.clone(),
        Some(_) => accounts
            .iter()
            .filter(|account| providers.contains(&routing::provider_of(account).to_string()))
            .cloned()
            .collect(),
        None => accounts.clone(),
    };
    let now = crate::server::logging::now_ms();
    let counts: HashMap<String, usize> = state
        .upstream()
        .connections()
        .snapshot()
        .into_iter()
        .collect();
    let keys = routing::CooldownKeys::new(model.as_deref().unwrap_or(""));
    // 限制器软跳过与转发的选路同一判据（被拦的账号在面板上也是「跳过」，
    // 原因写明是限制器而不是限流 —— 两回事）
    let facts = crate::server::core::usage_records::balance_facts();
    let token_facts = crate::server::core::limiter::token_facts();
    let limiter_blocked: HashSet<String> = scoped
        .iter()
        .filter(|account| {
            crate::server::core::usage_records::balance_blocked(account, &facts)
                || crate::server::core::limiter::token_skip_blocked(account, &token_facts, now)
        })
        .filter_map(|account| routing::account_id(account).map(str::to_string))
        .collect();
    let settings = config::routing_settings();
    let strategies = config::routing_strategy_settings();
    // 余额事实表：余额加权的权重来源（未知余额的账号不在表里）
    let balances: HashMap<String, f64> = crate::server::core::usage_records::balance_facts()
        .into_iter()
        .filter_map(|(id, fact)| fact.remaining.map(|remaining| (id, remaining)))
        .collect();
    // 面板只**看**当前状态，不做粘性查找、不推进有状态策略（游标 / LRU / 加权轮询）
    // —— 预演不该刷新任何会话的计时，也不该消耗下一次轮询
    let routing_state = state.upstream().routing();
    let (sunk, resting, open) = state.upstream().route_queue_facts(now);
    let meta = routing::PlanMeta {
        strategies: &strategies,
        state: &routing_state,
        balances: &balances,
        model: model.as_deref().unwrap_or(""),
        advance: false,
        sunk: &sunk,
        resting: &resting,
        open: &open,
        limiter_blocked: &limiter_blocked,
        sticky_account: None,
    };
    let plan = routing::plan_route(&scoped, &keys, &counts, &[], now, &meta);
    let entry_json = |entry: &routing::PlanEntry| {
        json!({
            "id": entry.id,
            "name": entry.name,
            "provider": entry.provider,
            "providerLabel": crate::server::core::providers::kind_from_id(&entry.provider)
                .map(|kind| crate::server::core::providers::meta(kind).label.to_string())
                .unwrap_or_else(|| entry.provider.clone()),
            "state": entry.state,
            "reason": entry.reason,
            "resetAt": if entry.reset_at > 0 { Value::from(entry.reset_at) } else { Value::Null },
            "priority": entry.account.get("priority").cloned().unwrap_or(Value::Null),
        })
    };
    ok_json(json!({
        "strategy": strategies.default.as_str(),
        "sticky": settings.sticky.as_str(),
        "model": model.clone().map(Value::String).unwrap_or(Value::Null),
        "lastRequestModel": snapshot.last_request_model(),
        "pickedId": plan.ordered.first().map(|entry| entry.id.clone()).unwrap_or_default(),
        "ordered": plan.ordered.iter().map(entry_json).collect::<Vec<_>>(),
        "blocked": plan.blocked.iter().map(entry_json).collect::<Vec<_>>(),
    }))
}

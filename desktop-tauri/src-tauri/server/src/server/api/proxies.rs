//! 代理管理：手动地址 CRUD、Clash Verge 端点同步、启停与连通性测试。
//! 写操作返回最新代理列表，测试失败以 `success:false` 返回 200。

use axum::body::Bytes;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::normalize_account_proxy;
use crate::server::core::{proxy_pool, proxy_settings};
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

use super::accounts::proxy_error;

pub async fn list_proxies(state: &ServerState) -> Response {
    let snapshot = state.store().list_accounts();
    let accounts: Vec<Value> = snapshot.get("accounts").and_then(Value::as_array)
        .map(|items| items.iter().map(|account| json!({
            "id": account.get("id"),
            "name": account.get("displayName").filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
                .or_else(|| account.get("name")),
            "priority": account.get("priority"),
            "enabled": account.get("enabled"),
            "proxy": account.get("proxy"),
        })).collect()).unwrap_or_default();
    ok_json(json!({
        "accounts": accounts,
        "clash": crate::server::core::proxies::clash_proxy_options(),
        "note": "支持手动 HTTP / SOCKS5 地址与 Clash Verge 端点；未配置代理时直连",
    }))
}

pub async fn test_proxy(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let mut proxy: Option<crate::server::core::proxies::ResolvedProxy> = None;
    if let Some(id) = payload.get("id").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        let Some(_) = state.store().get_credentials_by_id(id) else {
            return management_error(404, "账号不存在");
        };
        // 账号记录里存的是**原始**配置：按 egress 语义自己解析，
        // 这样 `pool-rotate` 也能测（首选成员），而不是报「不支持的代理来源」。
        let raw = state.store().proxy_config_by_id(id);
        match test_egress_primary(raw.as_ref()) {
            Ok(Some(resolved)) => proxy = Some(resolved),
            Ok(None) => {}
            Err(message) => return management_error(400, message),
        }
    } else if let Some(config_value) = payload.get("proxy") {
        let config = match normalize_account_proxy(config_value) {
            Ok(value) => value,
            Err(error) => return proxy_error(error),
        };
        match test_egress_primary(config.as_ref()) {
            Ok(Some(resolved)) => proxy = Some(resolved),
            Ok(None) => {}
            Err(message) => return management_error(400, message),
        }
    }
    let result = egress::test_proxy(proxy.as_ref()).await;
    logging::log("[Accounts]", &format!("代理测试{}: {}",
        proxy.as_ref().map(|proxy| format!("（{}）", proxy.label)).unwrap_or_else(|| "（直连）".to_string()),
        if result.success { format!("✅ 出口 IP {}", result.ip) } else { format!("❌ {}", result.error.clone().unwrap_or_default()) },
    ));
    let mut data = result.to_json();
    data["proxy"] = egress::describe_public(proxy.as_ref());
    ok_json(data)
}

/// 解析出「该配置的测试出口」：直连 → None；单出口 → 该出口；
/// 轮询组 → 首选成员（`members.first()`，与转发层 `primary()` 同口径）；
/// 解析失败 → 原因（调用方 400）。
fn test_egress_primary(
    config: Option<&Value>,
) -> Result<Option<crate::server::core::proxies::ResolvedProxy>, String> {
    use crate::server::core::proxies::{resolve_account_egress, AccountEgress, EgressResolution};
    match resolve_account_egress(config) {
        EgressResolution::Resolved(AccountEgress::Direct) => Ok(None),
        EgressResolution::Resolved(AccountEgress::Single(proxy)) => Ok(Some(proxy)),
        EgressResolution::Resolved(AccountEgress::Rotate(plan)) => Ok(plan.members.first().cloned()),
        EgressResolution::Failed(message) => Err(message),
    }
}

// ─── 代理池（「网络代理」页）─────────────────────────────────

/// 引用统计：`poolItemId → [{id, name, enabled}]`（引用它的账号）。
///
/// 三类引用都统计：
///   - `source == "pool"`：账号直接引用了某条目（按 `proxyId`）。存的是引用
///     而非快照，所以「引用了一个已被删掉的代理」同样统计得到（那正是最需要
///     提醒用户的情况）。
///   - `source == "pool-rotate"`：轮询组账号能用到的**每个**成员条目都算一次
///     引用 —— 命中判据与 `proxy_pool::members_for` 同口径：条目的 `group`
///     等于配置里的非空 `group`，或条目的 `id` 列在配置的 `proxyIds` 里。
///     删除确认框据此提示「有 N 个账号正在使用」，轮询组也算数。
///   - `source == "rotate"`（上游「轮询档」）：候选来自池里同一批条目（全部
///     启用 / 指定几条），也用同一套候选判定算引用 —— 不列出来，用户会以为
///     「这条没人用」而删掉一条正在被轮询的出口。
///
/// `items` 是本次池快照（由 `pool_payload` 读一次后传入），避免重复读库。
fn pool_references(
    state: &ServerState,
    items: &[Value],
) -> std::collections::HashMap<String, Vec<Value>> {
    fn push_reference(
        references: &mut std::collections::HashMap<String, Vec<Value>>,
        key: &str,
        account: &Value,
    ) {
        references.entry(key.to_string()).or_default().push(json!({
            "id": account.get("id").cloned().unwrap_or(Value::Null),
            // 统一展示名优先（与列表接口同一取舍）：后端给的 `displayName` 为空
            // 时回落账号名（`name`）。
            "name": account
                .get("displayName")
                .filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
                .cloned()
                .or_else(|| account.get("name").cloned())
                .unwrap_or(Value::Null),
            "enabled": account.get("enabled").cloned().unwrap_or(Value::Bool(true)),
        }));
    }
    let mut references: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    let snapshot = state.store().list_accounts();
    let Some(accounts) = snapshot.get("accounts").and_then(Value::as_array) else {
        return references;
    };
    for account in accounts {
        let Some(config) = account.get("proxy").and_then(|proxy| proxy.get("config")) else {
            continue;
        };
        // 上游「轮询档」（source == "rotate"）：候选来自池里同一批条目，也算引用。
        if crate::server::core::proxies::is_rotate_config(config) {
            for proxy_id in crate::server::core::proxy_rotate::candidate_ids(config) {
                push_reference(&mut references, &proxy_id, account);
            }
            continue;
        }
        match config.get("source").and_then(Value::as_str) {
            Some("pool") => {
                let Some(proxy_id) = config.get("proxyId").and_then(Value::as_str) else {
                    continue;
                };
                push_reference(&mut references, proxy_id, account);
            }
            Some("pool-rotate") => {
                let group = config.get("group").and_then(Value::as_str).unwrap_or("");
                let proxy_ids: Vec<&str> = config
                    .get("proxyIds")
                    .and_then(Value::as_array)
                    .map(|ids| ids.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                for item in items {
                    let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let group_match = !group.is_empty()
                        && item.get("group").and_then(Value::as_str) == Some(group);
                    if group_match || proxy_ids.contains(&item_id) {
                        push_reference(&mut references, item_id, account);
                    }
                }
            }
            _ => {}
        }
    }
    references
}

/// 池列表的响应体：`{items, clash, groups}`。
///
/// 每个条目注入 `usedBy`（引用它的账号）—— 删除确认框要用它提示
/// 「有 N 个账号正在使用」；`usedByServices` 单列服务用途（update / kernelDownload），
/// 不能把更新线路计入账号数量；`clash` 是实时快照，前端据此显示「Clash 是否可用 /
/// 有几个出口」；`groups` 是去重后的分组摘要（`[{name, count}]`，按名称升序），
/// 供轮询组的分组下拉框渲染。所有写操作也返回这份（与模型管理页的写接口同约定：
/// 前端就地替换），并在删除确认中提示账号与服务线路的影响。
fn pool_payload(state: &ServerState) -> Value {
    let items_all = proxy_pool::list();
    let references = pool_references(state, &items_all);
    let service_references = proxy_pool::usage::service_references();
    let items: Vec<Value> = items_all
        .iter()
        .map(|item| {
            let mut item = item.clone();
            let used = item
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| references.get(id))
                .cloned()
                .unwrap_or_default();
            let services = service_references
                .iter()
                .filter(|(_, proxy_id)| item.get("id").and_then(Value::as_str) == Some(proxy_id.as_str()))
            .map(|(service, _)| Value::String((*service).to_string())).collect();
            if let Some(object) = item.as_object_mut() {
                object.insert("usedBy".to_string(), Value::Array(used));
                object.insert("usedByServices".to_string(), Value::Array(services));
            }
            item
    }).collect();
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for item in &items_all {
        if let Some(group) = item.get("group").and_then(Value::as_str).filter(|g| !g.is_empty()) {
            *counts.entry(group.to_string()).or_insert(0) += 1;
        }
    }
    let groups: Vec<Value> = counts
        .into_iter()
        .map(|(name, count)| json!({"name": name, "count": count}))
        .collect();
    json!({
        "items": items,
        "clash": crate::server::core::proxies::clash_proxy_options(),
        "groups": groups,
    })
}

pub async fn pool_list(state: &ServerState) -> Response {
    let _ = proxy_pool::sync_clash(false);
    ok_json(pool_payload(state))
}

pub async fn pool_sync_clash(state: &ServerState) -> Response {
    let report = proxy_pool::sync_clash(true);
    let mut data = pool_payload(state);
    data["changes"] = json!(report.changes);
    data["syncError"] = json!(report.error);
    ok_json(data)
}

pub async fn pool_create(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match proxy_pool::create(&payload) {
        Ok(_) => ok_json(pool_payload(state)),
        Err(message) => management_error(400, message),
    }
}

pub async fn pool_update(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match proxy_pool::update(&payload) {
        Ok(_) => ok_json(pool_payload(state)),
        Err(message) => management_error(400, message),
    }
}

pub async fn pool_remove(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return management_error(400, "缺少代理 id");
    }
    let exists = proxy_pool::list().iter().any(|item| item.get("id").and_then(Value::as_str) == Some(id));
    match proxy_pool::remove(id) {
        Ok((_, name)) => {
            let mut data = pool_payload(state);
            data["removed"] = json!(name);
            ok_json(data)
        }
        Err(message) => management_error(if exists { 400 } else { 404 }, message),
    }
}

pub async fn pool_enable(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.trim().is_empty() {
        return management_error(400, "缺少代理 id");
    }
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return management_error(400, "缺少 enabled（true / false）");
    };
    match proxy_pool::set_enabled(id, enabled) {
        Ok((_, change)) => {
            let mut data = pool_payload(state);
            data["name"] = json!(change.name);
            data["enabled"] = json!(change.enabled);
            ok_json(data)
        }
        Err(message) => management_error(400, message),
    }
}

pub async fn pool_test(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    let proxy = match proxy_pool::resolve_for_test(id) {
        Ok(proxy) => proxy,
        Err(message) => return management_error(400, message),
    };
    let result = egress::test_proxy(Some(&proxy)).await;
    let write_error = proxy_pool::record_test(id, &proxy_pool::TestOutcome {
        success: result.success,
        ip: result.ip.clone(),
        duration_ms: result.duration_ms,
        error: result.error.clone(),
    }).err();
    logging::log("[Proxies]", &format!("代理测试（{}）: {}", proxy.label,
        if result.success { format!("✅ 出口 IP {}", result.ip) } else { format!("❌ {}", result.error.clone().unwrap_or_default()) },
    ));
    let mut data = result.to_json();
    data["proxy"] = egress::describe_public(Some(&proxy));
    if let Some(error) = write_error { data["saveError"] = json!(error); }
    data["items"] = pool_payload(state)["items"].clone();
    ok_json(data)
}

pub async fn settings_get() -> Response {
    ok_json(proxy_settings::get())
}

pub async fn settings_save(body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match proxy_settings::save(&payload) {
        Ok(value) => ok_json(value),
        Err(message) => management_error(400, message),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_egress_primary_handles_direct_single_and_empty_rotation() {
        // 直连（None / null）→ 没有可测出口
        assert!(matches!(test_egress_primary(None), Ok(None)));

        // 单出口 → 解析出那一个出口
        let single = test_egress_primary(Some(&serde_json::json!({
            "source": "custom", "protocol": "http", "host": "1.2.3.4", "port": 8080
        })));
        match single {
            Ok(Some(resolved)) => assert_eq!(resolved.host, "1.2.3.4"),
            other => panic!("expected a single egress, got {other:?}"),
        }

        // 轮询组无成员（测试库为空）→ 真原因，而不是旧的「不支持的代理来源」
        let rotate = test_egress_primary(Some(&serde_json::json!({
            "source": "pool-rotate", "group": "__nope__"
        })));
        match rotate {
            Err(message) => {
                assert_eq!(message, "轮询组没有可用出口");
                assert_ne!(message, "不支持的代理来源: pool-rotate");
            }
            other => panic!("expected a resolution failure, got {other:?}"),
        }
    }
}

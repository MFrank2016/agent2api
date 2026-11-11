pub(crate) mod global_accounts;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{RoutingProfile, RoutingProfiles, CONFIG, CONFIG_UPDATE_LOCK};
use crate::server::core::{
    account_order, account_store::store::AccountStore, routing, routing_scope,
};

/// 全局 profile 的 `strategy` 来自 workspace 的**展示面**（`routing_json` 只输出
/// `order` / `rotate` / `provider` 三档）。这些展示 id 是本地 7 档的有损投影，
/// 绝不能反推回去覆盖权威默认值 —— 否则用户在 `least-connections` 上做任何一次
/// 全局保存（哪怕只是账号排序）都会被静默改成 `priority`。
///
/// 只有真正来自本地策略面的 7 档 id（`/api/routing-strategy` 的 `default`）才更新
/// 权威默认值；展示 id 一律保留现状。
fn canonical_default_after_profile_strategy(
    existing: routing::RoutingStrategy,
    incoming: Option<&str>,
) -> routing::RoutingStrategy {
    const DISPLAY_IDS: [&str; 3] = ["order", "rotate", "provider"];
    match incoming {
        Some(id) if !DISPLAY_IDS.contains(&id.trim().to_ascii_lowercase().as_str()) => {
            routing::RoutingStrategy::parse(id).unwrap_or(existing)
        }
        _ => existing,
    }
}

pub fn revision(
    profile: &RoutingProfile,
    settings: super::RoutingSettings,
    customized: bool,
) -> String {
    let value = json!({ "profile": profile, "strategy": settings.strategy.as_str(), "sticky": settings.sticky.as_str(), "customized": customized });
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

pub fn save_workspace(
    store: &AccountStore,
    model: &str,
    profile: Option<RoutingProfile>,
    expected: &str,
) -> Result<(), String> {
    let _update = CONFIG_UPDATE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // 模型目录可能回读账号状态，必须在获取账号锁前解析。
    let available = (!model.is_empty()).then(|| crate::server::core::providers::catalog::forwarding_providers(model));
    let guard = store.guard();
    let state = store.load(&guard);
    let visible: Vec<Value> = state.accounts.iter()
        .filter(|record| crate::server::core::account_store::store_view::account_visible(&record.provider()))
        .map(|record| store.public_account(record)).collect();
    let available = available.unwrap_or_else(|| routing_scope::available_providers(&visible));
    let mut next = super::current();
    if next.routing_profiles.group_for_model(model).is_some() {
        return Err("此模型 ID 已被自定义路由组占用".into());
    }
    let effective = routing_scope::materialize_profile_with(
        &visible,
        &available,
        model,
        &next.routing_profiles,
    );
    let current_settings = next.routing_profiles.settings_for(model, next.routing);
    if revision(
        &effective,
        current_settings,
        next.routing_profiles.model_profile(model).is_some(),
    ) != expected
    {
        return Err("路由配置已在其他位置修改，请刷新当前配置后重试；草稿未丢失".to_string());
    }
    let mut profiles: RoutingProfiles = (*next.routing_profiles).clone();
    if model.is_empty() && (profiles.global.as_ref().is_some_and(global_accounts::has_restrictions)
        || profile.as_ref().is_some_and(global_accounts::has_restrictions)) {
        return Err("全局默认已改为账号启停，请先转换旧版全局参与设置；模型独立路由仍可设置参与名单".into());
    }
    if !model.is_empty()
        && profile.is_some()
        && !profiles.models.contains_key(model)
        && profiles.models.len() >= super::routing_profiles::MAX_ROUTING_MODELS
    {
        return Err("模型独立配置数量已达上限".to_string());
    }
    let mut priorities = Vec::new();
    if model.is_empty() {
        if let Some(profile) = &profile {
            let sorting = account_order::sorting(&visible, crate::server::logging::now_ms());
            let mut groups = account_order::manual_groups(&visible);
            for (provider, members) in &mut groups {
                if let Some(order) = profile.account_order.get(provider) {
                    let changed = effective.account_order.get(provider) != Some(order);
                    if changed {
                        crate::server::core::routing_order::global_queue(
                            &visible,
                            profile,
                            Some(provider),
                        )?;
                        let positions: HashMap<_, _> = order
                            .iter()
                            .enumerate()
                            .map(|(index, id)| (id.as_str(), index))
                            .collect();
                        members.sort_by_key(|account| {
                            routing::account_id(account)
                                .and_then(|id| positions.get(id))
                                .copied()
                                .unwrap_or(usize::MAX)
                        });
                    }
                }
            }
            let positions: HashMap<_, _> = profile
                .providers
                .iter()
                .enumerate()
                .map(|(index, id)| (id.as_str(), index))
                .collect();
            let mut selected: Vec<_> = groups
                .iter()
                .filter(|(id, _)| positions.contains_key(id.as_str()))
                .cloned()
                .collect();
            selected
                .sort_by_key(|(id, _)| positions.get(id.as_str()).copied().unwrap_or(usize::MAX));
            let mut replacement = selected.into_iter();
            for group in &mut groups {
                if positions.contains_key(group.0.as_str()) {
                    if let Some(next) = replacement.next() {
                        *group = next;
                    }
                }
            }
            let mut ids = groups.into_iter().flat_map(|(_, members)| {
                members
                    .into_iter()
                    .filter_map(|account| routing::account_id(&account).map(str::to_string))
            });
            let visible_ids: std::collections::HashSet<_> =
                sorting.account_order.values().flatten().cloned().collect();
            let mut records: Vec<_> = state.accounts.iter().collect();
            records.sort_by_key(|record| record.order_key());
            priorities = records
                .into_iter()
                .enumerate()
                .map(|(index, record)| {
                    (
                        if visible_ids.contains(record.id()) {
                            ids.next().unwrap_or_else(|| record.id().to_string())
                        } else {
                            record.id().to_string()
                        },
                        index as i64,
                    )
                })
                .collect();
        }
        let mut global = profile;
        if let Some(profile) = &mut global {
            if let Some(strategy) = profile.strategy.take() {
                // 展示 id（order/rotate/provider）是 workspace 面的有损投影，
                // 保留现有权威默认值；只有本地 7 档 id 才走规范写入口。
                let canonical = next.routing_strategy.clone();
                let default = canonical_default_after_profile_strategy(
                    canonical.default,
                    Some(&strategy),
                );
                if default != canonical.default {
                    let mut updated = canonical;
                    updated.default = default;
                    super::write_routing_strategy_canonical(&mut next, &updated);
                }
            }
            if let Some(sticky) = profile.sticky.take() {
                next.routing.sticky = super::StickyMode::from_id(&sticky);
                next.raw
                    .insert(super::KEY_ROUTING_STICKY.into(), sticky.into());
            }
            profile.account_order.clear();
        }
        profiles.global = global;
    } else if let Some(profile) = profile {
        profiles.models.insert(model.to_string(), profile);
    } else {
        profiles.models.remove(model);
    }
    let serialized = serde_json::to_value(&profiles).map_err(|error| error.to_string())?;
    next.raw
        .insert("routingProfiles".into(), serialized.clone());
    store.with_conn_mut(&guard, |conn| {
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO kv(key,value) VALUES('routingProfiles',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", rusqlite::params![serialized.to_string()])?;
        if model.is_empty() {
            for key in [super::KEY_ROUTING_STRATEGY, super::KEY_ROUTING_STICKY] {
                if let Some(value) = next.raw.get(key) { tx.execute("INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", rusqlite::params![key, value.to_string()])?; }
            }
            crate::server::core::account_store::sql::apply_priorities(&tx, &priorities)?;
        }
        tx.commit()
    }).map_err(|error| error.to_string())?;
    next.routing_profiles = Arc::new(profiles);
    crate::server::core::providers::catalog::invalidate();
    *CONFIG
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(next);
    crate::server::core::providers::catalog::invalidate();
    drop(guard);
    crate::server::core::cloud_sync::auto_sync::notify_change();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::routing::RoutingStrategy;

    /// 权威默认值为 least-connections 时，任何一次全局 profile 保存（携带
    /// workspace 展示面上的 order/rotate/provider）都必须原样保留，不能被
    /// 静默降级成 priority。
    #[test]
    fn global_save_preserves_local_strategy_when_profile_carries_display_id() {
        let canonical = RoutingStrategy::LeastConnections;
        for display in ["order", "rotate", "provider"] {
            assert_eq!(
                canonical_default_after_profile_strategy(canonical, Some(display)),
                canonical,
                "展示 id {display} 不得改写本地权威档位"
            );
        }
        assert_eq!(
            canonical_default_after_profile_strategy(canonical, Some("least-recently-used")),
            RoutingStrategy::LeastRecentlyUsed
        );
        assert_eq!(canonical_default_after_profile_strategy(canonical, None), canonical);
        assert_eq!(
            canonical_default_after_profile_strategy(canonical, Some("nonsense")),
            canonical
        );
    }
}

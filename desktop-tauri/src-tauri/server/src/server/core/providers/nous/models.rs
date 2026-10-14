//! Nous 模型目录：`GET {base}/models`（Bearer）+ 10 分钟缓存 + 内置兜底清单。
//!
//! ── 上游形态（规格 C）────────────────────────────────────────
//! ```text
//!   { "data": [ { "id": "nous/welcome", "aliases": [...], "context_length": 131072,
//!                 "pricing": {"prompt": …, "completion": …},
//!                 "supported_parameters": ["tools", "reasoning", …],
//!                 "top_provider": {"max_completion_tokens": 4096} } ] }
//! ```
//! 只依赖 `data[].id` 与几个可选字段；拿不到就回落到内置兜底清单，**绝不把
//! 目录清空**（清了会让「这家有登录态但模型列表是空的」，见 `catalog.rs` 的
//! 可用性判据）。
//!
//! ── 能力位的来源 ────────────────────────────────────────────
//! 上游目录**不报**结构化能力位，但给了 `supported_parameters`：它列出这个
//! 模型接受哪些请求参数 —— `tools` / `tool_choice` ⇒ 工具调用，
//! `reasoning` / `include_reasoning` / `reasoning_effort` ⇒ 思考。
//! 图片位没有依据 → conservative false（报错方向只能选「少报」）。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 与各家同款：内存缓存 + `providers::catalog_cache` 落盘（进程重启后读回，
//! scope 是 `SCOPE_NOUS`；本家**没有地区之分**，单格即可）。自动路径 10 分钟
//! TTL；用户手动「刷新模型清单」时 `force = true` 绕过。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::catalog_cache;

use super::endpoints;

/// 本家在 `catalog_cache` 里的 scope 名（单一推理面，一格即可）
pub const SCOPE: &str = catalog_cache::SCOPE_NOUS;

/// 自动路径的缓存 TTL
const TTL_MS: i64 = 10 * 60 * 1000;

/// 目录查询超时
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 内置兜底清单（远程拉不到时用；`incl nous/welcome` 是硬要求）。
///
/// 只列**确定存在**的几支：`nous/welcome` 是免费档唯一模型；其余是 Nous
/// Research 的旗舰 Hermes 发布。账号一旦登录并远程刷新成功，这份兜底即被
/// 真实目录覆盖。
const FALLBACK_MODELS: &[(&str, &str)] = &[
    ("nous/welcome", "Nous Welcome"),
    ("NousResearch/Hermes-4-405B", "Hermes 4 405B"),
    ("NousResearch/Hermes-4-70B", "Hermes 4 70B"),
    ("NousResearch/Hermes-3-Llama-3.1-405B", "Hermes 3 405B"),
    ("NousResearch/Hermes-3-Llama-3.1-70B", "Hermes 3 70B"),
];

/// 进程内缓存（首次访问时从落盘缓存读回）
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
    /// 这份清单是不是**远程拉到**的（落盘缓存装回来的也算）
    remote: bool,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        if let Some(cached) = catalog_cache::load(SCOPE) {
            *guard = Some(CatalogState {
                models: cached.models,
                fetched_at: cached.fetched_at,
                remote: true,
            });
        }
    }
    f(&mut guard)
}

/// `supported_parameters` 里有没有给定标记（大小写不敏感）
fn has_param(params: &[Value], markers: &[&str]) -> bool {
    params.iter().any(|item| {
        item.as_str()
            .map(|text| {
                let lowered = text.to_ascii_lowercase();
                markers.iter().any(|marker| lowered == *marker)
            })
            .unwrap_or(false)
    })
}

/// 把一个上游目录条目映射成聚合层认的形态（键名对照 `core/models/shape.rs::list_item`）
fn map_entry(row: &Value) -> Option<Value> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    // 展示名：优先 `aliases` 的第一个，其次 id 本身
    let name = row
        .get("aliases")
        .and_then(Value::as_array)
        .and_then(|aliases| aliases.iter().find_map(|item| item.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    let params: Vec<Value> = row
        .get("supported_parameters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let supports_tool_call = has_param(&params, &["tools", "tool_choice"]);
    let supports_reasoning = has_param(
        &params,
        &["reasoning", "include_reasoning", "reasoning_effort"],
    );
    let context_length = row.get("context_length").and_then(Value::as_i64).unwrap_or(0);
    let max_output = row
        .get("top_provider")
        .and_then(|top| top.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut entry = json!({
        "id": id,
        "name": name,
        "supportsToolCall": supports_tool_call,
        "supportsReasoning": supports_reasoning,
        // 图片位无依据：conservative false（见模块头）
        "supportsImages": false,
    });
    if let Some(object) = entry.as_object_mut() {
        if context_length > 0 {
            object.insert("maxInputTokens".to_string(), Value::from(context_length));
        }
        if max_output > 0 {
            object.insert("maxOutputTokens".to_string(), Value::from(max_output));
        }
    }
    Some(entry)
}

/// 内置兜底清单的聚合层形态
fn fallback_models() -> Vec<Value> {
    FALLBACK_MODELS
        .iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "name": name,
                "supportsToolCall": true,
                "supportsReasoning": true,
                "supportsImages": false,
            })
        })
        .collect()
}

/// 解析 `/models` 的响应（`{data:[…]}` 或裸数组）
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(map_entry).collect()
}

/// 当前模型清单（内存 → 落盘缓存 → 内置兜底；永远非空）
pub fn list() -> Vec<Value> {
    with_state(|slot| {
        slot.as_ref()
            .map(|state| state.models.clone())
            .filter(|models| !models.is_empty())
            .unwrap_or_else(fallback_models)
    })
}

/// 这份清单是否来自**远程拉取**（`catalog::refresh_meta` 的「来源」列判据）
pub fn remote_refreshed() -> bool {
    with_state(|slot| slot.as_ref().map(|state| state.remote).unwrap_or(false))
}

/// 最近一次成功刷新的时刻（毫秒；没刷过为 0）—— 界面「更新日期」列读它
pub fn last_refreshed_at() -> i64 {
    with_state(|slot| slot.as_ref().map(|state| state.fetched_at).unwrap_or(0))
}

/// 拉一次模型目录并落地（内存 + 持久化缓存）。
///
/// `force = false`（自动路径）时 10 分钟 TTL 内早退为 `unchanged()`；
/// `force = true`（用户手动刷新）真打上游。失败时保留现有清单。
pub async fn refresh(token: &str, base: &str, force: bool) -> ModelRefreshOutcome {
    let now = crate::server::logging::now_ms();
    if !force {
        let fresh = with_state(|slot| {
            slot.as_ref()
                .map(|state| state.remote && now - state.fetched_at < TTL_MS)
                .unwrap_or(false)
        });
        if fresh {
            return ModelRefreshOutcome::unchanged();
        }
    }
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || token.trim().is_empty() {
        return ModelRefreshOutcome::failed("Nous 模型目录刷新缺少推理基址或凭证（保留现有清单）");
    }
    let mut headers = endpoints::auth_headers(token);
    headers.push(("Accept".to_string(), "application/json".to_string()));
    let response = match send_raw(
        "GET",
        &endpoints::inference_url(base, endpoints::MODELS_PATH),
        None,
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return ModelRefreshOutcome::failed(format!(
                "Nous 模型目录查询失败：{error}（保留现有清单）"
            ))
        }
    };
    if !response.ok {
        return ModelRefreshOutcome::failed(format!(
            "Nous 模型目录返回 HTTP {}（保留现有清单）",
            response.status
        ));
    }
    let payload = response.payload.unwrap_or(Value::Null);
    let models = parse_models(&payload);
    if models.is_empty() {
        return ModelRefreshOutcome::failed("Nous 模型目录为空或格式不认识（保留现有清单）");
    }
    let count = models.len();
    with_state(|slot| {
        *slot = Some(CatalogState {
            models: models.clone(),
            fetched_at: now,
            remote: true,
        });
    });
    catalog_cache::save(SCOPE, &models, now);
    ModelRefreshOutcome::refreshed(count)
}

/// 从账号记录直接刷（适配器用）。
pub async fn refresh_from_record(record: Option<&Value>, force: bool) -> ModelRefreshOutcome {
    let credential = match super::credentials::from_record(record) {
        Ok(credential) => credential,
        Err(error) => return ModelRefreshOutcome::failed(error.message),
    };
    // 匿名档只有 `nous/welcome`，远程目录意义不大 —— 直接保留兜底清单
    if credential.anonymous {
        return ModelRefreshOutcome::unchanged();
    }
    refresh(&credential.token, &credential.base_url, force).await
}

/// 模型目录刷新结果（与各家同一类型）
pub use crate::server::core::providers::adapter::ModelRefreshOutcome;

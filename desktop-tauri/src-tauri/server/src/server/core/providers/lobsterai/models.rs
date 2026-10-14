//! LobsterAI 模型目录：`GET {base}/api/models/available` + 5 分钟缓存。
//!
//! ── 上游形态 ────────────────────────────────────────────────
//! ```text
//! GET {base}/api/models/available?firstKeyfrom=..&latestKeyfrom=..&version=0.1.0&uuid=..&userId=..
//!     header: Authorization: Bearer <accessToken>（+ 能力头）
//!   → { code, data: [ { modelId, modelName, provider, apiFormat } ] }
//! ```
//! 目录**没有内置兜底清单**（上游清单就是唯一事实来源）：拉不到时保持现有清单
//! （内存 → 落盘缓存 → 空）。清单为空的家不进 `/v1/models`（见 `catalog` 的
//! 可用性判据）—— 这与 Loomy「上游没有内置兜底」的处境一致。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 与各家同款：内存缓存 + `providers::catalog_cache` 落盘（进程重启后读回，
//! scope 是 `SCOPE_LOBSTERAI`）。自动路径 5 分钟 TTL；用户手动「刷新模型清单」
//! 时 `force = true` 绕过。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::catalog_cache;

use super::{credentials, endpoints};

/// 本家在 `catalog_cache` 里的 scope 名（单一入口，一格即可）
pub const SCOPE: &str = catalog_cache::SCOPE_LOBSTERAI;

/// 自动路径的缓存 TTL
const TTL_MS: i64 = 5 * 60 * 1000;

/// 目录查询超时
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 进程内缓存（首次访问时从落盘缓存读回）
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
    /// 这份清单是不是远程拉到的（落盘缓存装回来的也算）
    remote: bool,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// 在锁内执行一段取值，并在首次访问时把落盘缓存装回内存
fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// 把一个上游目录条目映射成聚合层认的形态。
///
/// 上游只给 `modelId` / `modelName` / `provider` / `apiFormat`，**不返回**结构化
/// 能力位；本家 chat 恒下发 tools 与思考字段，因此工具 / 思考两位置 true，
/// 图片位保守 false（无目录依据时不报「支持」，免得客户端发出上游 400 的请求）。
fn map_entry(row: &Value) -> Option<Value> {
    let id = row
        .get("modelId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let name = row
        .get("modelName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    Some(json!({
        "id": id,
        "name": name,
        "provider": row.get("provider").cloned().unwrap_or(Value::Null),
        "apiFormat": row.get("apiFormat").cloned().unwrap_or(Value::Null),
        "supportsToolCall": true,
        "supportsReasoning": true,
        "supportsImages": false,
    }))
}

/// 解析 `/api/models/available` 的响应（`{data:[…]}` 或裸数组）
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(map_entry).collect()
}

/// 当前模型清单（内存 → 落盘缓存；可能为空）
pub fn list() -> Vec<Value> {
    with_state(|slot| {
        slot.as_ref()
            .map(|state| state.models.clone())
            .unwrap_or_default()
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

/// 构造目录查询 URL（keyfrom + version + uuid + userId 都进查询串）
fn models_url(cred: &credentials::LobsterAiCredentials) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("firstKeyfrom", &cred.first_keyfrom.to_string());
    serializer.append_pair("latestKeyfrom", &cred.latest_keyfrom.to_string());
    serializer.append_pair("version", endpoints::PROTOCOL_VERSION);
    if !cred.device_id.is_empty() {
        serializer.append_pair("uuid", &cred.device_id);
    }
    if !cred.user_id.is_empty() {
        serializer.append_pair("userId", &cred.user_id);
    }
    format!("{}?{}", endpoints::url(endpoints::MODELS_PATH), serializer.finish())
}

/// 拉一次模型目录并落地（内存 + 持久化缓存）。
///
/// `force = false`（自动路径）时 TTL 内早退为 `unchanged()`；`force = true` 真打上游。
/// 失败时保留现有清单，返回 `failed(原因)` 让手动路径如实汇报。
pub async fn refresh(
    cred: &credentials::LobsterAiCredentials,
    force: bool,
) -> ModelRefreshOutcome {
    if cred.access_token.trim().is_empty() {
        return ModelRefreshOutcome::failed("LobsterAI 账号缺少 access token，无法刷新目录");
    }
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
    // 目录请求也带真实版本头（与聊天同一套伪装，见 endpoints 模块头）
    endpoints::refresh_real_version().await;
    let mut headers = endpoints::capability_headers();
    headers.extend(endpoints::auth_headers(&cred.access_token));
    let response = match send_raw(
        "GET",
        &models_url(cred),
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
                "LobsterAI 模型目录查询失败：{error}（保留现有清单）"
            ))
        }
    };
    if !response.ok {
        return ModelRefreshOutcome::failed(format!(
            "LobsterAI 模型目录返回 HTTP {}（保留现有清单）",
            response.status
        ));
    }
    let payload = response.payload.unwrap_or(Value::Null);
    let models = parse_models(&payload);
    if models.is_empty() {
        return ModelRefreshOutcome::failed(
            "LobsterAI 模型目录为空或格式不认识（保留现有清单）",
        );
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

/// 模型目录刷新结果（用于 `ProviderAdapter::refresh_models` 的返回）
pub use crate::server::core::providers::adapter::ModelRefreshOutcome;

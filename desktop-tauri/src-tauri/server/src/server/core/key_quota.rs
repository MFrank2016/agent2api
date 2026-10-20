//! 网关 Key 的**用量查询与配额判定**（schema v16）。
//!
//! ── 这个模块解决什么问题 ─────────────────────────────────────
//! 网关可以有多把 Key（`core::api_keys` 的 `apiKeys` 列表），但改造前
//! 「哪把 Key 用了多少」在库里根本不存在 —— `requests` 表没有 Key 维度。
//! 本模块提供两件事：
//!   1. **用量查询**：按 Key 聚合区间内的请求数与 Token（Key 页的用量列）；
//!   2. **配额判定**：某把 Key 在当前窗口内是否已经用超（中间件的拦截点）。
//!
//! ── 为什么从明细聚合，而不是像 one-api 那样挂计数器 ─────────
//! one-api / new-api 在 Token 行上直接挂 `used_quota` / `remain_quota`，
//! 并默认走**内存批量刷盘**（`batchUpdateStores` 每 N 秒刷一次）。那条路的
//! 代价是明确的：进程在刷盘前崩溃，这段时间的消耗永久丢失 —— 计数器的值
//! 就此与明细对不上，而且无法恢复（内存里的增量已经没了）。
//!
//! 本项目的取向不同：`requests` 是明细的唯一事实来源，`request_daily` 是它的
//! 聚合。从明细算用量天然崩溃安全（记账与查询读的是同一份持久数据），代价是
//! 查询要扫索引 —— 用一个短 TTL 的内存缓存把代价摊掉，而不是把正确性换成性能。
//!
//! ── 缓存怎么工作（懒加载，没有心跳）────────────────────────────
//! `USAGE_CACHE` 存「窗口起点 + 每把 Key 的已用量」，TTL [`CACHE_TTL_MS`]。
//! 判定 / 查询时若缓存过期，就地跑一次聚合查询并整表替换。
//!
//! 为什么不挂心跳循环（`limiter` 的做法）：那边的 `TokenFact` 要被**选路热路径**
//! 读（每次尝试一个账号都要判一次），所以必须常驻内存。配额判定的频率低得多
//! （每请求一次，且只判命中的那一把），懒加载足够；而且懒加载**重启后天然正确**
//! —— 不需要「等首轮刷新」，也不会有「重启后 10 秒内配额失效」的窗口。
//!
//! ── 一条硬约束：拿不出证据就不拦 ──────────────────────────────
//! 库不可用、缓存空、Key 刚建（明细里还没有它的行）—— 这些情况一律按
//! 「已用 0」处理，**不拦**。与 `limiter::token_skip_blocked` 的口径逐字一致：
//! 拦截是对「已经用超」的断言，没有证据就不该断言。误拦的代价（正常请求被
//! 429 挡回）远大于漏拦（一个窗口多放几个请求）。
//!
//! ── 硬约束：持锁期间不打日志 ─────────────────────────────────
//! 与 `limiter` / `usage_records` 同一条：`Db::with` 拿的是全局唯一的连接锁，
//! 闭包里打日志会当场死锁。日志一律在闭包之外打。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::server::core::api_keys::{ApiKeyEntry, QuotaReset};
use crate::server::core::limiter;
use crate::server::db::Db;

/// 缓存有效期：5 秒。
///
/// 取舍：更短 → 配额判定更贴近实时，但每次过期都要扫一次索引；更长 → 数据库
/// 压力更小，但「刚用超的 Key 还能再发几个请求」。5 秒与报表页的刷新节奏
/// （默认 1 秒轮询、但报表走的是聚合表）无关，只服务配额这一个读点。
///
/// 这个值决定了配额的**软**程度：最坏情况下一个 Key 在超限后还能再用
/// [`CACHE_TTL_MS`] 毫秒（叠加上单请求本身的耗时）。对本地单机网关是够用的
/// 精度 —— 要严格限流得预扣额度，那需要事务锁，与这里的取向冲突。
pub const CACHE_TTL_MS: i64 = 5_000;

/// 用量查询的默认区间：近 7 天（Key 页展示用）。
pub const USAGE_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

static DB: OnceLock<Option<Db>> = OnceLock::new();

/// 缓存槽：`(抓取时刻, 窗口种类 → (窗口起点, key_id → 已用 Token))`
///
/// 键是窗口种类而不是 Key：同一次抓取要覆盖所有「启用的 Key 用到的全部窗口」，
/// 一张表按种类分组存下，判定时按 Key 自己的种类取对应那一组。
static USAGE_CACHE: Mutex<Option<CachedUsage>> = Mutex::new(None);

struct CachedUsage {
    /// 抓取时刻（毫秒）：TTL 判据
    fetched_at: i64,
    /// 窗口种类 → (该种类的窗口起点, key_id → 已用 Token)
    by_kind: HashMap<i64, (i64, HashMap<String, i64>)>,
}

/// 装库句柄（`ServerState::bootstrap` 调一次，与 `limiter::install` 并排）。
pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn database() -> Option<&'static Db> {
    DB.get().and_then(Option::as_ref)
}

/// 某把 Key 在当前窗口内是否已经用超。
///
/// `quota_tokens = None`（无限，默认）→ 恒 `false`，**不查库**：这是最常见的情形，
/// 让「没配配额」的请求在热路径上零成本。
///
/// 判定是「已用 ≥ 上限」（含等于）：与 `limiter::token_skip_blocked` 同一口径，
/// 达到阈值即视为用满。
pub fn exceeded(entry: &ApiKeyEntry, now_ms: i64) -> bool {
    let Some(limit) = entry.quota_tokens else {
        return false;
    };
    if limit <= 0 {
        return false;
    }
    let used = usage_for(entry, now_ms);
    used >= limit
}

/// 某把 Key 在当前窗口内的已用 Token（拿不到证据按 0）。
pub fn usage_for(entry: &ApiKeyEntry, now_ms: i64) -> i64 {
    let kind = entry.quota_reset.kind();
    // ── 先在锁内看一眼缓存，锁外再取数 ──────────────────────────
    // `fetch` 会读配置并查库（各取一把锁）。**不能在持有 USAGE_CACHE 时做这些**：
    // 那样锁序就成了「USAGE_CACHE → Db」，而本模块之外的任何一处若反过来
    // 先拿 Db 再进 key_quota，就是一次死锁。锁序只保留一个方向：这里先释放
    // USAGE_CACHE，取完数再拿回来写。
    let hit = {
        let mut cache = match USAGE_CACHE.lock() {
            Ok(guard) => guard,
            // 锁中毒（另一个线程 panic 在持锁期间）：按「没有证据」处理，不拦
            Err(poisoned) => poisoned.into_inner(),
        };
        if cache
            .as_ref()
            .map(|cached| now_ms.saturating_sub(cached.fetched_at) < CACHE_TTL_MS)
            .unwrap_or(false)
        {
            Some(cached_lookup(cache.as_ref(), kind, &entry.id, now_ms))
        } else {
            *cache = None;
            None
        }
    };
    if let Some(used) = hit {
        return used;
    }
    // 锁外取数（可能读配置 + 查库）
    let fresh = fetch(now_ms);
    let used = cached_lookup(fresh.as_ref(), kind, &entry.id, now_ms);
    if let Ok(mut cache) = USAGE_CACHE.lock() {
        // 只在「还没有更新的快照」时写回：另一个线程可能刚抓过一份更新的
        if cache
            .as_ref()
            .map(|cached| cached.fetched_at <= fresh.as_ref().map(|f| f.fetched_at).unwrap_or(i64::MIN))
            .unwrap_or(true)
        {
            *cache = fresh;
        }
    }
    used
}

/// 从缓存里取某 Key 在当前窗口的已用；窗口已翻页或 Key 不在表里 → 0。
///
/// `now_ms` 由调用方带入（不在内部再取一次时间）：判定路径上「缓存是否过期」
/// 与「窗口是否翻页」必须用**同一个时刻**判，两次取时间会让边界上的读数
/// 自相矛盾（缓存判定用的是 T 时刻、窗口判定用的是 T+ε）。
fn cached_lookup(cache: Option<&CachedUsage>, kind: i64, key_id: &str, now_ms: i64) -> i64 {
    let Some(cache) = cache else { return 0 };
    let Some((window_start, usage)) = cache.by_kind.get(&kind) else {
        return 0;
    };
    // 窗口翻页的兜底：缓存里的窗口起点若不是「现在这个窗口」的起点，说明缓存
    // 跨越了窗口边界（例如自然日 0 点），旧读数属于上一个窗口 —— 按 0 算，
    // 下一次抓取会给出新窗口的读数
    let now_kind_start = limiter::kind_window_start(kind, now_ms);
    if *window_start != now_kind_start {
        return 0;
    }
    usage.get(key_id).copied().unwrap_or(0)
}

/// 抓一次当前窗口的用量（所有种类一次查完）。
///
/// 返回 `None` = 库不可用 / 语句失败 —— 调用方按「没有证据」处理（不拦）。
///
/// ── 为什么一条 SQL 覆盖所有种类 ────────────────────────────
/// 每个种类对应一个窗口起点，理想写法是每个种类一条查询。但实际配置里
/// 种类极少（绝大多数 Key 用默认的「自然日」，即种类 0），一条
/// `GROUP BY key_id` 加一个「最早窗口起点」的下界就能覆盖全部：
/// 取所有种类里**最早的**窗口起点作为 `ts >=` 的下界，读回来的行在 Rust 侧
/// 按各自的窗口起点再筛一次。多读一点行，换一次查询 —— 窗口最长 30 天，
/// 与 5 秒的缓存周期相比，多读的量可以忽略。
fn fetch(now_ms: i64) -> Option<CachedUsage> {
    let db = database()?;
    // 启用中的 Key 各自的窗口种类（去重）。配额判定只关心这些。
    let entries = crate::server::core::api_keys::list();
    let mut kinds: Vec<i64> = entries
        .iter()
        .filter(|entry| entry.quota_tokens.is_some())
        .map(|entry| entry.quota_reset.kind())
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    if kinds.is_empty() {
        // 一把配了配额的 Key 都没有：空表（判定恒 false，不必查库）
        return Some(CachedUsage {
            fetched_at: now_ms,
            by_kind: HashMap::new(),
        });
    }
    let windows: Vec<(i64, i64)> = kinds
        .iter()
        .map(|kind| (*kind, limiter::kind_window_start(*kind, now_ms)))
        .collect();
    // 最早的窗口起点当下界：SQL 读多了由 Rust 侧按种类再筛
    let lower_bound = windows.iter().map(|(_, start)| *start).min()?;
    let rows = db.with(|conn| {
        let Ok(mut statement) = conn.prepare(
            "SELECT key_id, ts, total_tokens FROM requests \
             WHERE key_id != '' AND ts >= ?1",
        ) else {
            return None;
        };
        let Ok(rows) = statement.query_map([lower_bound], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        }) else {
            return None;
        };
        Some(rows.filter_map(|item| item.ok()).collect::<Vec<_>>())
    })
    // 两层 Option：外层 = 库不可用，内层 = 语句准备/执行失败
    .flatten()?;
    let mut by_kind: HashMap<i64, (i64, HashMap<String, i64>)> = HashMap::new();
    for (kind, start) in &windows {
        by_kind.insert(*kind, (*start, HashMap::new()));
    }
    for (key_id, ts, tokens) in rows {
        // 每种窗口各自按自己的起点筛：SQL 用的是最早窗口的下界，短窗口多读的
        // 行在这里被挡掉（`kind` 不参与筛选，故不绑定）
        for (_kind, (start, usage)) in by_kind.iter_mut() {
            if ts >= *start {
                *usage.entry(key_id.clone()).or_insert(0) += tokens;
            }
        }
    }
    Some(CachedUsage {
        fetched_at: now_ms,
        by_kind,
    })
}

/// Key 页的用量汇总：近 [`USAGE_WINDOW_MS`] 的请求数 / Token / 最近使用时刻。
///
/// 返回 `key_id → KeyUsage`。与配额判定**分开**：那边看的是「当前窗口」，
/// 这边看的是「近 7 天」，两个区间不同、也不必共享缓存。
///
/// 直接从明细聚合（`GROUP BY key_id`），走 `idx_requests_key_ts`。**包含测试
/// 流量**：模型测试的 `key_id` 恒为空串，自然被 `key_id != ''` 排除。
pub fn usage_summary(now_ms: i64) -> HashMap<String, KeyUsage> {
    let Some(db) = database() else { return HashMap::new() };
    let since = now_ms - USAGE_WINDOW_MS;
    let rows = db.with(|conn| {
        let Ok(mut statement) = conn.prepare(
            "SELECT key_id, COUNT(*), \
                    COALESCE(SUM(total_tokens), 0), \
                    COALESCE(SUM(CASE WHEN status >= 200 AND status < 300 THEN 1 ELSE 0 END), 0), \
                    COALESCE(MAX(ts), 0) \
             FROM requests WHERE key_id != '' AND ts >= ?1 GROUP BY key_id",
        ) else {
            return None;
        };
        let Ok(rows) = statement.query_map([since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                KeyUsage {
                    requests: row.get(1)?,
                    tokens: row.get(2)?,
                    successful: row.get(3)?,
                    last_used_at: row.get(4)?,
                },
            ))
        }) else {
            return None;
        };
        Some(rows.filter_map(|item| item.ok()).collect::<Vec<_>>())
    });
    rows.flatten()
        .map(|items| items.into_iter().collect())
        .unwrap_or_default()
}

/// 单把 Key 的近 7 天用量
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyUsage {
    pub requests: i64,
    pub tokens: i64,
    pub successful: i64,
    pub last_used_at: i64,
}

impl KeyUsage {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "requests": self.requests,
            "tokens": self.tokens,
            "successful": self.successful,
            "lastUsedAt": self.last_used_at,
        })
    }
}

/// 某把 Key 的配额快照（Key 页的进度条读它）
///
/// `limit = None` 表示无限 —— 前端据此显示「无限」而不是 0/0 的进度条。
pub fn quota_json(entry: &ApiKeyEntry, now_ms: i64) -> serde_json::Value {
    let Some(limit) = entry.quota_tokens else {
        return serde_json::json!({
            "limit": serde_json::Value::Null,
            "used": 0,
            "windowStart": serde_json::Value::Null,
            "reset": entry.quota_reset.to_json(),
            "exceeded": false,
        });
    };
    let used = usage_for(entry, now_ms);
    let window_start = limiter::kind_window_start(entry.quota_reset.kind(), now_ms);
    serde_json::json!({
        "limit": limit,
        "used": used,
        "windowStart": window_start,
        "reset": entry.quota_reset.to_json(),
        "exceeded": used >= limit,
    })
}

/// 清空缓存（Key 增删改后调用：配置变了，窗口种类集合也可能变）
pub fn invalidate() {
    if let Ok(mut cache) = USAGE_CACHE.lock() {
        *cache = None;
    }
}

/// 配额重置方式的默认值（新建 Key 时用）：自然日
pub fn default_reset() -> QuotaReset {
    QuotaReset::Daily
}

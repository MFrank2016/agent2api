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

use crate::server::core::api_keys::{ApiKeyEntry, KeyQuotaRule, QuotaReset};
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
/// 没配配额（规则与旧的 `quota_tokens` 都空，默认）→ 恒 `false`，**不查库**：
/// 这是最常见的情形，让「没配配额」的请求在热路径上零成本。
///
/// 判定是「已用 ≥ 上限」（含等于）：与 `limiter::token_skip_blocked` 同一口径，
/// 达到阈值即视为用满。
///
/// 签名保持不变：调用点（鉴权中间件）不该知道「单条还是多条」这件事。
pub fn exceeded(entry: &ApiKeyEntry, now_ms: i64) -> bool {
    exceeded_rule(entry, now_ms).is_some()
}

/// 判定是否超限，并返回**第一条**超限的规则（`None` = 全部通过）。
///
/// ── 全部满足才放行 ─────────────────────────────────────────
/// 多条规则是叠加约束（日限额 + 月限额同时生效）。「任一满足」等于只保留
/// 最宽松的一条，让另一条形同虚设。
///
/// ── 「等于上限也算超」────────────────────────────────────────
/// 既有约定（`used >= limit`）不改：前端显示的「已用 50000 / 50000」与
/// 「被拦」必须一致。
///
/// ── 热路径零成本 ───────────────────────────────────────────
/// 没配配额（规则与旧字段都空）时第一行就 `?` 掉，直接返回 `None`，
/// 连缓存锁都不碰、也不分配 —— 这是最常见的情形，也是 `exceeded` 签名
/// 可以保持不变（调用点不必知道单条还是多条）的底气。
pub fn exceeded_rule(entry: &ApiKeyEntry, now_ms: i64) -> Option<KeyQuotaRule> {
    // 新形态直接借规则表；旧形态就地造一条。**两条路都不克隆 `Vec`** ——
    // 这是每请求都跑的热路径，`effective_rules` 的分配只留给 `fetch` /
    // `quota_json` 那些低频调用点。
    if !entry.quota_rules.is_empty() {
        return exceeded_in(entry, &entry.quota_rules, now_ms);
    }
    let limit = entry.quota_tokens?;
    if limit <= 0 {
        return None;
    }
    let rule = KeyQuotaRule { limit, reset: entry.quota_reset };
    exceeded_in(entry, std::slice::from_ref(&rule), now_ms)
}

/// 在给定的规则表里找第一条超限的（`None` = 全部通过）。
fn exceeded_in(
    entry: &ApiKeyEntry,
    rules: &[KeyQuotaRule],
    now_ms: i64,
) -> Option<KeyQuotaRule> {
    for rule in rules {
        if rule.limit <= 0 {
            continue;
        }
        let used = usage_for_window(entry, rule.reset.kind(), now_ms);
        if used >= rule.limit {
            return Some(*rule);
        }
    }
    None
}

/// 有效规则：新形态优先，空则从旧字段造一条。
///
/// ── 为什么读侧兼容而不是写侧迁移 ─────────────────────────
/// `apiKeys` 存在配置（kv / 旧 config.json）里，没有事务可依赖。迁移一旦写坏，
/// 用户的 Key 会带着一个猜出来的阈值上线；读侧兼容则无论如何都读得出旧行为。
fn effective_rules(entry: &ApiKeyEntry) -> Vec<KeyQuotaRule> {
    if !entry.quota_rules.is_empty() {
        return entry.quota_rules.clone();
    }
    match entry.quota_tokens {
        Some(limit) => vec![KeyQuotaRule { limit, reset: entry.quota_reset }],
        None => Vec::new(),
    }
}

/// 某把 Key 在当前窗口内的已用 Token（拿不到证据按 0）。
///
/// 既有入口：读 `entry.quota_reset` 对应的窗口。行为与改造前逐字一致。
pub fn usage_for(entry: &ApiKeyEntry, now_ms: i64) -> i64 {
    usage_for_window(entry, entry.quota_reset.kind(), now_ms)
}

/// 按**指定窗口种类**查已用量。
///
/// 实现 = 改造前 `usage_for` 的整个函数体，只把首行
/// `let kind = entry.quota_reset.kind();` 去掉（kind 现在是参数）。
/// 其余一字不改 —— 包括那段「先锁内看一眼缓存、锁外取数」的锁序注释。
fn usage_for_window(entry: &ApiKeyEntry, kind: i64, now_ms: i64) -> i64 {
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
        // **关键改动**：不再只看 `quota_tokens`，而是看**有效规则**。
        // 旧写法会让「只有 quota_rules」的 Key 拿不到读数 → 恒 0 → 永不超限
        // （静默无效：界面上规则看着生效了，实际一个请求都拦不住）。
        // 走 `effective_rules` 而不是直接遍历 `quota_rules`：旧字段形态的 Key
        // 也要收集它的窗口，否则升级后旧配额立刻失效。
        .flat_map(|entry| effective_rules(entry).into_iter().map(|rule| rule.reset.kind()))
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
/// ── 两种读法，一个形状 ──────────────────────────────────
/// 旧的单条字段（`limit` / `used` / `windowStart` / `reset` / `exceeded`）
/// 描述**第一条**规则：前端的进度条与列文案照旧读它们，升级后零改动；
/// 新的 `rules` 数组给出全部规则（每条各自的读数与是否超限），规则编辑区用它。
/// 只读 `quota_tokens` 的写法会让规则型 Key 在 Key 页显示「无限」——
/// 那正是这次要修的一处静默错误。
///
/// `limit = None` 表示无限 —— 前端据此显示「无限」而不是 0/0 的进度条。
pub fn quota_json(entry: &ApiKeyEntry, now_ms: i64) -> serde_json::Value {
    let rules = effective_rules(entry);
    let Some(first) = rules.first() else {
        return serde_json::json!({
            "limit": serde_json::Value::Null,
            "used": 0,
            "windowStart": serde_json::Value::Null,
            "reset": entry.quota_reset.to_json(),
            "exceeded": false,
            "rules": serde_json::Value::Array(Vec::new()),
        });
    };
    let used = usage_for_window(entry, first.reset.kind(), now_ms);
    let window_start = limiter::kind_window_start(first.reset.kind(), now_ms);
    // 每条规则各自查一次读数：规则数 ≤ MAX_QUOTA_RULES（5），且同一窗口种类
    // 复用同一份缓存，代价可忽略。
    let rules_json: Vec<serde_json::Value> = rules
        .iter()
        .map(|rule| {
            let rule_used = usage_for_window(entry, rule.reset.kind(), now_ms);
            serde_json::json!({
                "limit": rule.limit,
                "used": rule_used,
                "windowStart": limiter::kind_window_start(rule.reset.kind(), now_ms),
                "reset": rule.reset.to_json(),
                // 「等于上限也算超」：与 `exceeded_rule` 同一口径
                "exceeded": rule.limit > 0 && rule_used >= rule.limit,
            })
        })
        .collect();
    serde_json::json!({
        "limit": first.limit,
        "used": used,
        "windowStart": window_start,
        "reset": first.reset.to_json(),
        "exceeded": exceeded_rule(entry, now_ms).is_some(),
        "rules": rules_json,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::config;
    use crate::server::core::api_keys;
    use serde_json::{json, Map, Value};
    use std::sync::MutexGuard;

    /// 与 `core::provider_options` 的用例共用同一把配置锁：本模块的用例也整份
    /// 改 `apiKeys`（读整份 → 改 → 写整份），并行跑会互相盖掉对方的底稿。
    fn lock_config() -> MutexGuard<'static, ()> {
        crate::server::core::provider_options::CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 用**真实读侧**（`entries_from`）造记录：`ApiKeyEntry` 有一个私有字段，
    /// 测试里不能直接字面量构造，走 JSON 顺带把 `quotaRules` 的解析也覆盖了。
    fn entries_of(items: Value) -> Vec<ApiKeyEntry> {
        let mut raw = Map::new();
        raw.insert(api_keys::KEY_API_KEYS.to_string(), items);
        api_keys::entries_from(&raw)
    }

    fn one_entry(items: Value) -> ApiKeyEntry {
        let mut entries = entries_of(items);
        assert_eq!(entries.len(), 1, "测试记录必须能被读侧解析出来");
        entries.remove(0)
    }

    fn legacy_entry(tokens: Option<i64>, reset: QuotaReset) -> ApiKeyEntry {
        let mut object = json!({
            "id": "k-legacy",
            "name": "legacy",
            "key": "sk-legacy-test-key",
            "enabled": true,
            "quotaReset": reset.to_json(),
        });
        if let Some(limit) = tokens {
            object["quotaTokens"] = json!(limit);
        }
        one_entry(Value::Array(vec![object]))
    }

    /// 把给定条目写成配置里的 `apiKeys`（`fetch` 读的是配置快照），
    /// 用例结束时把原值写回去 —— 不还原就是把「这些 Key」留给后面的用例。
    struct RestoreKeys {
        previous: Option<Value>,
    }

    impl RestoreKeys {
        fn apply(items: Value) -> Self {
            let previous = config::with_raw(|raw| raw.get(api_keys::KEY_API_KEYS).cloned())
                .flatten();
            let _ = config::update_raw_field(api_keys::KEY_API_KEYS, items);
            invalidate();
            Self { previous }
        }
    }

    impl Drop for RestoreKeys {
        fn drop(&mut self) {
            let value = self.previous.take().unwrap_or(Value::Null);
            let _ = config::update_raw_field(api_keys::KEY_API_KEYS, value);
            invalidate();
        }
    }

    /// 整个测试进程共用一个临时库：`key_quota::install` 是 `OnceLock`，
    /// 一旦装入就换不掉。用例之间靠**互不相同的 key_id** 与 `invalidate()`
    /// 隔离，不靠换库。
    fn test_db() -> &'static Db {
        static TEST_DB: OnceLock<Db> = OnceLock::new();
        TEST_DB.get_or_init(|| {
            let path = std::env::temp_dir()
                .join(format!("agent2api-test-key-quota-{}.db", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let db = Db::open(&path).expect("临时库应当能建起来");
            install(Some(db.clone()));
            db
        })
    }

    fn seed_usage(db: &Db, key_id: &str, ts: i64, tokens: i64) {
        db.with(|conn| {
            conn.execute(
                "INSERT INTO requests (id, ts, model, key_id, total_tokens) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![format!("req-{key_id}-{ts}"), ts, "m", key_id, tokens],
            )
        })
        .expect("库句柄应当可用")
        .expect("应当能插入一条用量明细");
    }

    /// 当前自然日窗口里一个**远离 0 点**的时刻（避免 `ts` 落到前一天）。
    fn midday_after(base: i64) -> i64 {
        limiter::kind_window_start(0, base) + 3 * 3_600_000
    }

    /// 回归锁定：只有 `quotaRules`、没有 `quotaTokens` 的 Key 必须被判定。
    ///
    /// 反例（v1）：`fetch` 只收集 `quota_tokens.is_some()` 的窗口种类，
    /// rule-only 的 Key 拿不到读数 → 恒 0 → 永不超限（静默无效）。
    /// 这里种一条真实用量再断言「超限」，比断言 `None` 强：坏实现同样返回
    /// `None`，只有「有读数才会超」能把两者分开。
    #[test]
    fn rules_only_key_reaches_the_database() {
        let _guard = lock_config();
        let db = test_db();
        let now = midday_after(1_760_000_000_000);
        let _keys = RestoreKeys::apply(json!([{
            "id": "k-rules-only",
            "name": "rules-only",
            "key": "sk-rules-only-test",
            "enabled": true,
            "quotaRules": [{ "limit": 1, "reset": "daily" }],
        }]));
        let entry = one_entry(json!([{
            "id": "k-rules-only",
            "name": "rules-only",
            "key": "sk-rules-only-test",
            "enabled": true,
            "quotaRules": [{ "limit": 1, "reset": "daily" }],
        }]));
        assert_eq!(entry.quota_tokens, None, "前提：旧字段为空");
        seed_usage(db, "k-rules-only", now - 60_000, 5);
        invalidate();
        let hit = exceeded_rule(&entry, now);
        assert!(hit.is_some(), "规则型 Key 必须真的读到库里的用量并判定超限");
        assert_eq!(hit.map(|rule| rule.limit), Some(1));
        assert!(exceeded(&entry, now), "签名不变的入口同样要判定");
    }

    #[test]
    fn legacy_single_quota_becomes_one_rule() {
        let entry = legacy_entry(Some(50_000), QuotaReset::Daily);
        let rules = effective_rules(&entry);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].limit, 50_000);
        assert_eq!(rules[0].reset, QuotaReset::Daily);
    }

    #[test]
    fn legacy_single_quota_exceeded_is_unchanged() {
        let _guard = lock_config();
        let db = test_db();
        let now = midday_after(1_760_100_000_000);
        let _keys = RestoreKeys::apply(json!([{
            "id": "k-legacy-exceeded",
            "name": "legacy-exceeded",
            "key": "sk-legacy-exceeded-test",
            "enabled": true,
            "quotaTokens": 1,
            "quotaReset": "daily",
        }]));
        let entry = one_entry(json!([{
            "id": "k-legacy-exceeded",
            "name": "legacy-exceeded",
            "key": "sk-legacy-exceeded-test",
            "enabled": true,
            "quotaTokens": 1,
            "quotaReset": "daily",
        }]));
        seed_usage(db, "k-legacy-exceeded", now - 60_000, 3);
        invalidate();
        assert!(exceeded(&entry, now), "旧字段路径行为必须与改造前一致");
    }

    /// 多条规则是**叠加约束**：一条超、一条没超 → 返回超的那条。
    /// 「任一满足」的实现会只保留最宽松的一条，把另一条形同虚设。
    #[test]
    fn all_rules_must_pass() {
        let _guard = lock_config();
        let db = test_db();
        let now = midday_after(1_760_200_000_000);
        let rules = json!([
            { "limit": 1, "reset": "daily" },
            { "limit": 1_000_000, "reset": { "fixed": 2_592_000 } }
        ]);
        let _keys = RestoreKeys::apply(json!([{
            "id": "k-two-rules",
            "name": "two-rules",
            "key": "sk-two-rules-test",
            "enabled": true,
            "quotaRules": rules.clone(),
        }]));
        let entry = one_entry(json!([{
            "id": "k-two-rules",
            "name": "two-rules",
            "key": "sk-two-rules-test",
            "enabled": true,
            "quotaRules": rules,
        }]));
        let effective = effective_rules(&entry);
        assert_eq!(effective.len(), 2, "两条规则都要保留（叠加约束，不是取最宽）");
        seed_usage(db, "k-two-rules", now - 60_000, 5);
        invalidate();
        let hit = exceeded_rule(&entry, now);
        assert_eq!(
            hit.map(|rule| rule.limit),
            Some(1),
            "必须返回超限的那条（日限额），不是宽松的那条"
        );
    }

    #[test]
    fn no_rules_means_unlimited() {
        let entry = legacy_entry(None, QuotaReset::Daily);
        assert!(effective_rules(&entry).is_empty(), "空规则 = 无限，热路径不查库");
        assert!(!exceeded(&entry, crate::server::logging::now_ms()));
    }

    /// Key 页的配额快照必须认规则：只读 `quotaTokens` 时规则型 Key 会显示「无限」。
    #[test]
    fn quota_json_reports_rule_limits() {
        let _guard = lock_config();
        let db = test_db();
        let now = midday_after(1_760_300_000_000);
        let _keys = RestoreKeys::apply(json!([{
            "id": "k-quota-json",
            "name": "quota-json",
            "key": "sk-quota-json-test",
            "enabled": true,
            "quotaRules": [{ "limit": 1, "reset": "daily" }],
        }]));
        let entry = one_entry(json!([{
            "id": "k-quota-json",
            "name": "quota-json",
            "key": "sk-quota-json-test",
            "enabled": true,
            "quotaRules": [{ "limit": 1, "reset": "daily" }],
        }]));
        seed_usage(db, "k-quota-json", now - 60_000, 5);
        invalidate();
        let snapshot = quota_json(&entry, now);
        assert_eq!(snapshot["limit"], json!(1), "规则的上限必须出现在快照里");
        assert_eq!(snapshot["used"], json!(5));
        assert_eq!(snapshot["exceeded"], json!(true));
        assert_eq!(snapshot["rules"].as_array().map(Vec::len), Some(1));
    }
}

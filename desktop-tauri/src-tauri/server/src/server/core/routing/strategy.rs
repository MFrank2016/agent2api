//! 账号选路策略 —— 「同一家内有多个可用账号时，按什么规则挑一个」。
//!
//! ── 与 [`super::pick_account_by_priority`] 的关系（本模块最要紧的一条）──
//! 改造前选路是**唯一确定**的：过滤出可用账号，按 `(优先级, 加入时间)` 排序取
//! 首位。本模块在这条规则之上加了一层**可配置的策略**，让一家内的请求能摊到
//! 多个账号上（轮询 / 最少连接 / 加权……）。但它**不改变默认行为**：
//!   - 全局默认策略是 [`RoutingStrategy::Priority`]；
//!   - 当候选里**每一家**解析出来的策略都是 `priority` 时，挑选走
//!     [`super::pick_account_by_priority`] 的**同一条扁平路径**（见
//!     [`pick_account`] 里的兼容性分支）—— 结果与改造前逐字相同。
//! 兼容性是**结构性**的：两条路径共用 [`super::usable_candidates`] 这一份过滤，
//! 不是靠两份代码碰巧写得一样。
//!
//! ── 跨家顺序为什么保持「全局优先级」─────────────────────────
//! 账号排在**全局一条队列**里（四家混排，见 `super` 模块头）。策略只作用于
//! **一家之内**：先按各家「最优先的那个账号」的全局优先级排出**组**的顺序
//! （与改造前「谁优先级小谁先用」完全一致），再在**队首那一组**里按该家的策略
//! 挑一个。于是「workbuddy 的请求不会借到 raccoon 的账号」这条仍然成立，
//! 而「先试哪一家」也没有因为引入策略而改变。
//!
//! ── 运行时状态（[`RoutingState`]）───────────────────────────
//! 三种策略需要**跨请求记忆**（进程内、重启即清零）：
//!   - 轮询：`(provider, 请求名) → 游标`；
//!   - 最近最少使用：`账号 id → 上次被选中的时刻`；
//!   - 平滑加权轮询（SWRR）：`账号 id → 当前权重`。
//! 全挂在一个 `Arc<Mutex<..>>` 上（与 `upstream::connections` 同一形态），
//! 由 `UpstreamService` 持有、`service.routing()` 取用。挑选本身是**同步**的
//! （不跨 `.await` 持锁）：进入时拿锁、算完即放。
//!
//! ── 权重与「无读数」账号（不给它们断粮）──────────────────────
//! 余额加权读 `usage_records::balance_facts()` 的内存事实表（零 IO）。**没有
//! 已知余额**的账号（从未查过 / unlimited / 形状不认）拿一个**基线权重**
//! （已知余额的中位数，一个都没有时取 1.0）—— 否则它们会被永久饿死。
//! 权重恒为正；全为 0 时退化成等权（各 1.0）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::server::config::RoutingStrategySettings;
use crate::server::core::account_store::priority::by_priority_order;

use super::{account_id, compare_by_priority, priority_key, provider_of, usable_candidates, CooldownKeys};

/// 一家的账号挑选规则（配置值就是字符串，见 `config::KEY_ROUTING_STRATEGY`）。
///
/// `as_str()` 是配置与接口响应里的字符串形态，也是前端下拉的取值 ——
/// 三处只有一套名字（与 `core::prompt::PromptMode` 同一手法）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RoutingStrategy {
    /// 优先级优先（**默认**）：`(优先级, 加入时间)` 取首位 —— 与改造前逐字相同
    #[default]
    Priority,
    /// 最少连接：在途请求数最少者优先，并列按优先级
    LeastConnections,
    /// 轮询：在可用账号之间依次轮流
    RoundRobin,
    /// 最近最少使用：上次被选中最早者优先，并列按优先级
    LeastRecentlyUsed,
    /// 余额加权：平滑加权轮询，权重 = 账号剩余余额
    BalanceWeighted,
    /// 优先级加权：平滑加权轮询，权重 = 优先级数字的单调递减函数（同级等权）
    PriorityWeighted,
}

impl RoutingStrategy {
    /// 全部合法值（界面下拉与 400 文案共用一份，避免两处各写一遍）
    pub const ALL: [RoutingStrategy; 6] = [
        RoutingStrategy::Priority,
        RoutingStrategy::LeastConnections,
        RoutingStrategy::RoundRobin,
        RoutingStrategy::LeastRecentlyUsed,
        RoutingStrategy::BalanceWeighted,
        RoutingStrategy::PriorityWeighted,
    ];

    /// 配置值与接口响应里的字符串形态
    pub fn as_str(self) -> &'static str {
        match self {
            RoutingStrategy::Priority => "priority",
            RoutingStrategy::LeastConnections => "least-connections",
            RoutingStrategy::RoundRobin => "round-robin",
            RoutingStrategy::LeastRecentlyUsed => "least-recently-used",
            RoutingStrategy::BalanceWeighted => "balance-weighted",
            RoutingStrategy::PriorityWeighted => "priority-weighted",
        }
    }

    /// 解析配置值：大小写不敏感 + 去首尾空白（与 `PromptMode::parse` 同一口径）。
    /// 认不出（含空串）给 `None`，由调用方决定是回落 `priority` 还是报 400。
    pub fn parse(text: &str) -> Option<Self> {
        let normalized = text.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|strategy| strategy.as_str() == normalized)
    }

    /// 界面与日志用的中文短名
    pub fn label(self) -> &'static str {
        match self {
            RoutingStrategy::Priority => "优先级优先",
            RoutingStrategy::LeastConnections => "最少连接",
            RoutingStrategy::RoundRobin => "轮询",
            RoutingStrategy::LeastRecentlyUsed => "最近最少使用",
            RoutingStrategy::BalanceWeighted => "余额加权",
            RoutingStrategy::PriorityWeighted => "优先级加权",
        }
    }

    /// 界面里显示在控件下方的说明（可空）
    pub fn hint(self) -> &'static str {
        match self {
            RoutingStrategy::Priority => "按账号页的全局优先级从小到大取第一个可用的（默认，行为与改造前完全一致）",
            RoutingStrategy::LeastConnections => "优先挑当前在途请求数最少的账号，把并发摊平",
            RoutingStrategy::RoundRobin => "在可用账号之间依次轮流，均衡且可预期",
            RoutingStrategy::LeastRecentlyUsed => "优先挑最久没有被选中的账号",
            RoutingStrategy::BalanceWeighted => "按账号余额加权轮询：余额越多分到的请求越多（平滑加权轮询）",
            RoutingStrategy::PriorityWeighted => "按优先级加权轮询：优先级数字越小权重越高（同级等权）",
        }
    }
}

/// 选路策略的**运行时状态**（进程内、重启即清零）。
///
/// `Clone` 是浅拷贝（内部 `Arc`）：`ServerState` 的 handler 克隆后拿到的仍是
/// 同一份状态（与 `Connections` 同一形态）。三张表各自的键见模块头。
#[derive(Clone)]
pub struct RoutingState {
    inner: Arc<Mutex<RoutingInner>>,
}

#[derive(Default)]
struct RoutingInner {
    /// `(provider, 请求名) → 轮询游标`（下一次从哪个下标开始）
    rr_cursor: HashMap<(String, String), usize>,
    /// `账号 id → 上次被选中的毫秒时刻`
    last_selected: HashMap<String, i64>,
    /// `账号 id → 平滑加权轮询的当前权重`
    current_weight: HashMap<String, f64>,
}

impl RoutingState {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(RoutingInner::default())) }
    }

    /// 取状态表锁；锁中毒不致命（与 `upstream::connections` 同一策略）
    fn lock(&self) -> MutexGuard<'_, RoutingInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 轮询游标前进一格：返回本次应取的**组内下标**（`0..len`）。
    ///
    /// 游标按 `(provider, 请求名)` 记忆 —— 同一家的同一模型请求依次轮流，
    /// 不同模型互不干扰。`len == 0` 是防御（调用点不会用空组），返回 0。
    pub fn next_round_robin(&self, provider: &str, model: &str, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let mut inner = self.lock();
        let cursor = inner
            .rr_cursor
            .entry((provider.to_string(), model.to_string()))
            .or_insert(0);
        let index = *cursor % len;
        *cursor = cursor.wrapping_add(1);
        index
    }

    /// 该账号上次被选中的时刻（从未选中 = `None`）
    pub fn last_selected(&self, id: &str) -> Option<i64> {
        self.lock().last_selected.get(id).copied()
    }

    /// 记下「这个账号这次被选中了」（空 id 是「没有账号」，不记）
    pub fn mark_selected(&self, id: &str, now: i64) {
        if id.is_empty() {
            return;
        }
        self.lock().last_selected.insert(id.to_string(), now);
    }

    /// 平滑加权轮询（Nginx 同款 SWRR）：`ids` 与 `weights` 一一对应，返回本次
    /// 应取的**下标**。当前权重跨调用累积，于是选出的序列在长程上按权重成比例。
    ///
    /// 并列时取**先见者**（调用方已把组按优先级排好，于是并列回落到优先级序）。
    /// `ids` 为空返回 0（防御）。
    pub fn smooth_pick(&self, ids: &[String], weights: &[f64]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let mut inner = self.lock();
        let mut total = 0.0_f64;
        let mut best_index = 0usize;
        let mut best_weight = f64::NEG_INFINITY;
        for (index, (id, weight)) in ids.iter().zip(weights.iter()).enumerate() {
            let current = inner.current_weight.entry(id.clone()).or_insert(0.0);
            *current += *weight;
            total += *weight;
            if *current > best_weight {
                best_weight = *current;
                best_index = index;
            }
        }
        // 选中者减去总权重：这一步是 SWRR「长程成比例」的来源
        if let Some(id) = ids.get(best_index) {
            if let Some(current) = inner.current_weight.get_mut(id) {
                *current -= total;
            }
        }
        best_index
    }
}

impl Default for RoutingState {
    fn default() -> Self {
        Self::new()
    }
}

/// 带**策略**的账号挑选：过滤 → 分组 → 组内按该家策略挑一个。
///
/// ── 算法（与任务约定逐条对应）───────────────────────────────
///   1. 用 [`usable_candidates`] 过滤出可用候选（与优先级路径**同一份**）；
///   2. 候选按 provider 分组，组间按「组内最优先账号」的全局优先级排序
///      —— 保持改造前的跨家顺序；
///   3. 取**队首组**，组内按优先级排好，再按该家策略（配置默认 + 逐家覆盖）
///      挑一个账号；
///   4. 兼容性守卫：候选里**每一家**的策略都是 `priority` 时，直接走
///      [`super::pick_account_by_priority`] —— 结果与改造前逐字相同。
///
/// `state` 是跨请求的运行时状态（轮询游标 / LRU 时刻 / SWRR 权重）；
/// `balances` 是「账号 id → 已知剩余余额」（未知的账号不在表里，用基线权重）。
///
/// `keys` 里的**请求名**是轮询游标的第二维（[`CooldownKeys::requested`]）。
pub fn pick_account(
    accounts: &[Value],
    keys: &CooldownKeys<'_>,
    counts: &HashMap<String, usize>,
    exclude_ids: &[String],
    now: i64,
    settings: &RoutingStrategySettings,
    state: &RoutingState,
    balances: &HashMap<String, f64>,
) -> Option<Value> {
    let candidates = usable_candidates(accounts, keys, counts, exclude_ids, now);
    if candidates.is_empty() {
        return None;
    }

    // ── 兼容性守卫：全 priority → 与改造前逐字一致的扁平路径 ──────────
    let all_priority = candidates
        .iter()
        .all(|account| settings.resolve(provider_of(account)) == RoutingStrategy::Priority);
    if all_priority {
        let mut sorted = candidates;
        sorted.sort_by(compare_by_priority);
        return sorted.into_iter().next();
    }

    let model = keys.requested();
    let mut groups = group_by_provider(candidates);
    // 组间按「组内最优先账号」的全局优先级排 —— 队首组就是改造前会先用的那一家
    groups.sort_by(|left, right| {
        by_priority_order(best_priority_key(&left.1), best_priority_key(&right.1))
    });
    let (provider, mut group) = groups.into_iter().next()?;
    // 组内按优先级排好：所有策略的并列兜底与轮询顺序都基于它
    group.sort_by(compare_by_priority);

    let strategy = settings.resolve(&provider);
    let index = match strategy {
        // 组已按优先级排好，首位即答案
        RoutingStrategy::Priority => 0,
        RoutingStrategy::LeastConnections => least_connections_index(&group, counts),
        RoutingStrategy::RoundRobin => state.next_round_robin(&provider, model, group.len()),
        RoutingStrategy::LeastRecentlyUsed => least_recently_used_index(&group, state),
        RoutingStrategy::BalanceWeighted => {
            let (ids, weights) = balance_weights(&group, balances);
            state.smooth_pick(&ids, &weights)
        }
        RoutingStrategy::PriorityWeighted => {
            let (ids, weights) = priority_weights(&group);
            state.smooth_pick(&ids, &weights)
        }
    };
    // LRU 的「更新上次选中时刻」放在挑中之后（挑不中就不该改记忆）
    if matches!(strategy, RoutingStrategy::LeastRecentlyUsed) {
        if let Some(id) = group.get(index).and_then(|account| account_id(account)) {
            state.mark_selected(id, now);
        }
    }
    group.into_iter().nth(index)
}

/// 按 provider 分组，**保持候选第一次出现的组序**（后续再按优先级排组）。
fn group_by_provider(candidates: Vec<Value>) -> Vec<(String, Vec<Value>)> {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for account in candidates {
        let provider = provider_of(&account).to_string();
        match groups.iter_mut().find(|(id, _)| *id == provider) {
            Some(existing) => existing.1.push(account),
            None => groups.push((provider, vec![account])),
        }
    }
    groups
}

/// 一组账号里「最优先」那个的排序键（组间排序用）
fn best_priority_key(accounts: &[Value]) -> (i64, i64) {
    accounts
        .iter()
        .map(priority_key)
        .min_by(|left, right| by_priority_order(*left, *right))
        .unwrap_or((
            crate::server::core::account_store::priority::DEFAULT_PRIORITY,
            0,
        ))
}

/// 最少连接：`counts[id]` 最小者优先；并列取**先见者**（组已按优先级排好）
fn least_connections_index(group: &[Value], counts: &HashMap<String, usize>) -> usize {
    let mut best = 0usize;
    let mut best_count = usize::MAX;
    for (index, account) in group.iter().enumerate() {
        let count = account_id(account)
            .and_then(|id| counts.get(id))
            .copied()
            .unwrap_or(0);
        if count < best_count {
            best_count = count;
            best = index;
        }
    }
    best
}

/// 最近最少使用：上次选中时刻最小者优先（从未选中 = 最小）；并列取先见者
fn least_recently_used_index(group: &[Value], state: &RoutingState) -> usize {
    let mut best = 0usize;
    let mut best_time = i64::MAX;
    for (index, account) in group.iter().enumerate() {
        let stamp = account_id(account)
            .and_then(|id| state.last_selected(id))
            .unwrap_or(i64::MIN);
        if stamp < best_time {
            best_time = stamp;
            best = index;
        }
    }
    best
}

/// 余额加权的权重表：`ids` 与 `weights` 一一对应（顺序 = 组内优先级序）。
///
/// 未知余额的账号取**基线**（组内已知余额的中位数，一个都没有时 1.0）。
/// 权重收成非负；全为 0 时退化成等权（各 1.0），保证组内没有账号被饿死。
fn balance_weights(group: &[Value], balances: &HashMap<String, f64>) -> (Vec<String>, Vec<f64>) {
    let ids: Vec<String> = group
        .iter()
        .map(|account| account_id(account).unwrap_or_default().to_string())
        .collect();
    let mut known: Vec<f64> = ids
        .iter()
        .filter_map(|id| balances.get(id).copied())
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    let baseline = median(&mut known).unwrap_or(1.0);
    let mut weights: Vec<f64> = ids
        .iter()
        .map(|id| {
            let raw = balances.get(id).copied().unwrap_or(baseline);
            if raw.is_finite() && raw > 0.0 {
                raw
            } else {
                0.0
            }
        })
        .collect();
    if weights.iter().sum::<f64>() <= 0.0 {
        weights = vec![1.0; ids.len()];
    }
    (ids, weights)
}

/// 优先级加权的权重表：把组内出现的优先级去重升序，第 `r` 名（0 起）权重 =
/// `(去重总数 - r)` —— 数字越小权重越高，同级共享同一权重。下限 1.0。
fn priority_weights(group: &[Value]) -> (Vec<String>, Vec<f64>) {
    let ids: Vec<String> = group
        .iter()
        .map(|account| account_id(account).unwrap_or_default().to_string())
        .collect();
    let mut distinct: Vec<i64> = group.iter().map(|account| priority_key(account).0).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let total = distinct.len() as f64;
    let weights: Vec<f64> = group
        .iter()
        .map(|account| {
            let priority = priority_key(account).0;
            let rank = distinct.iter().position(|value| *value == priority).unwrap_or(0);
            (total - rank as f64).max(1.0)
        })
        .collect();
    (ids, weights)
}

/// 一组数字的中位数（排序后取中；偶数取中间两个的平均）。空表给 `None`。
fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[mid])
    } else {
        Some((values[mid - 1] + values[mid]) / 2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn account(id: &str, provider: &str, priority: i64, added: i64) -> Value {
        json!({
            "id": id,
            "provider": provider,
            "priority": priority,
            "addedAt": added,
            "enabled": true,
        })
    }

    fn settings(default: RoutingStrategy, providers: &[(&str, RoutingStrategy)]) -> RoutingStrategySettings {
        let mut table = std::collections::BTreeMap::new();
        for (id, strategy) in providers {
            table.insert((*id).to_string(), *strategy);
        }
        RoutingStrategySettings { default, providers: table }
    }

    fn pick(
        accounts: &[Value],
        strategy: RoutingStrategy,
        state: &RoutingState,
        counts: &HashMap<String, usize>,
        balances: &HashMap<String, f64>,
    ) -> Option<String> {
        let keys = CooldownKeys::new("m");
        let config = settings(strategy, &[]);
        pick_account(accounts, &keys, counts, &[], 1_000, &config, state, balances)
            .and_then(|value| account_id(&value).map(str::to_string))
    }

    #[test]
    fn parse_round_trips_and_rejects_unknown() {
        for strategy in RoutingStrategy::ALL {
            assert_eq!(RoutingStrategy::parse(strategy.as_str()), Some(strategy));
        }
        assert_eq!(RoutingStrategy::parse("  ROUND-ROBIN "), Some(RoutingStrategy::RoundRobin));
        assert_eq!(RoutingStrategy::parse("bogus"), None);
        assert_eq!(RoutingStrategy::parse(""), None);
    }

    #[test]
    fn resolve_prefers_provider_override_then_default() {
        let config = settings(
            RoutingStrategy::Priority,
            &[("workbuddy", RoutingStrategy::RoundRobin)],
        );
        assert_eq!(config.resolve("workbuddy"), RoutingStrategy::RoundRobin);
        assert_eq!(config.resolve("raccoon"), RoutingStrategy::Priority);
        assert_eq!(config.resolve("unknown"), RoutingStrategy::Priority);
    }

    #[test]
    fn priority_only_matches_pick_account_by_priority() {
        let accounts = vec![
            account("b", "p", 200, 2),
            account("a", "p", 100, 1),
            account("c", "q", 150, 3),
        ];
        let keys = CooldownKeys::new("m");
        let counts = HashMap::new();
        let config = RoutingStrategySettings::default();
        let state = RoutingState::new();
        let balances = HashMap::new();
        let with_strategy =
            pick_account(&accounts, &keys, &counts, &[], 1_000, &config, &state, &balances);
        let by_priority =
            super::super::pick_account_by_priority(&accounts, &keys, &counts, &[], 1_000);
        assert_eq!(with_strategy, by_priority);
        // 默认策略下取全局优先级最小者（跨家也成立）
        assert_eq!(account_id(&with_strategy.unwrap_or(Value::Null)), Some("a"));
    }

    #[test]
    fn least_connections_prefers_idle_account_then_priority() {
        let accounts = vec![
            account("a", "p", 100, 1),
            account("b", "p", 101, 2),
            account("c", "p", 102, 3),
        ];
        let state = RoutingState::new();
        let mut counts = HashMap::new();
        counts.insert("a".to_string(), 5);
        counts.insert("b".to_string(), 1);
        counts.insert("c".to_string(), 3);
        let picked = pick(
            &accounts,
            RoutingStrategy::LeastConnections,
            &state,
            &counts,
            &HashMap::new(),
        );
        assert_eq!(picked.as_deref(), Some("b"));

        // 全部空闲 → 回落到优先级序（a 最优先）
        let picked = pick(
            &accounts,
            RoutingStrategy::LeastConnections,
            &state,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(picked.as_deref(), Some("a"));
    }

    #[test]
    fn round_robin_rotates_over_priority_order() {
        let accounts = vec![
            account("a", "p", 100, 1),
            account("b", "p", 101, 2),
            account("c", "p", 102, 3),
        ];
        let state = RoutingState::new();
        let sequence: Vec<Option<String>> = (0..6)
            .map(|_| {
                pick(
                    &accounts,
                    RoutingStrategy::RoundRobin,
                    &state,
                    &HashMap::new(),
                    &HashMap::new(),
                )
            })
            .collect();
        let ids: Vec<&str> = sequence.iter().filter_map(|id| id.as_deref()).collect();
        assert_eq!(ids, vec!["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn round_robin_cursor_is_per_model() {
        let accounts = vec![account("a", "p", 100, 1), account("b", "p", 101, 2)];
        let state = RoutingState::new();
        let config = settings(RoutingStrategy::RoundRobin, &[]);
        let counts = HashMap::new();
        let balances = HashMap::new();
        let first = pick_account(
            &accounts,
            &CooldownKeys::new("m1"),
            &counts,
            &[],
            1_000,
            &config,
            &state,
            &balances,
        );
        // 换一个请求名：游标互不干扰，仍从 a 开始
        let other = pick_account(
            &accounts,
            &CooldownKeys::new("m2"),
            &counts,
            &[],
            1_000,
            &config,
            &state,
            &balances,
        );
        assert_eq!(account_id(&first.unwrap_or(Value::Null)), Some("a"));
        assert_eq!(account_id(&other.unwrap_or(Value::Null)), Some("a"));
    }

    #[test]
    fn least_recently_used_advances_after_pick() {
        let accounts = vec![
            account("a", "p", 100, 1),
            account("b", "p", 101, 2),
            account("c", "p", 102, 3),
        ];
        let state = RoutingState::new();
        let first = pick(
            &accounts,
            RoutingStrategy::LeastRecentlyUsed,
            &state,
            &HashMap::new(),
            &HashMap::new(),
        );
        let second = pick(
            &accounts,
            RoutingStrategy::LeastRecentlyUsed,
            &state,
            &HashMap::new(),
            &HashMap::new(),
        );
        let third = pick(
            &accounts,
            RoutingStrategy::LeastRecentlyUsed,
            &state,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(first.as_deref(), Some("a"));
        assert_eq!(second.as_deref(), Some("b"));
        assert_eq!(third.as_deref(), Some("c"));
    }

    #[test]
    fn balance_weighted_distributes_proportionally() {
        let accounts = vec![account("a", "p", 100, 1), account("b", "p", 101, 2)];
        let state = RoutingState::new();
        let mut balances = HashMap::new();
        balances.insert("a".to_string(), 3.0);
        balances.insert("b".to_string(), 1.0);
        let mut counts = HashMap::new();
        counts.insert("a".to_string(), 0);
        for _ in 0..4 {
            let _ = pick(
                &accounts,
                RoutingStrategy::BalanceWeighted,
                &state,
                &counts,
                &balances,
            );
        }
        // 权重 3:1 → 4 次里 a 应被选中 3 次（用当前权重反推选中次数）
        let inner = state.lock();
        let a = inner.current_weight.get("a").copied().unwrap_or(0.0);
        let b = inner.current_weight.get("b").copied().unwrap_or(0.0);
        drop(inner);
        // SWRR 的当前权重在 4 次后回到 0（整数权重、整除总权重时的稳态）
        assert!((a - b).abs() < 1e-9, "当前权重应收敛: a={a} b={b}");
    }

    #[test]
    fn balance_weighted_sequence_is_proportional() {
        let accounts = vec![account("a", "p", 100, 1), account("b", "p", 101, 2)];
        let state = RoutingState::new();
        let mut balances = HashMap::new();
        balances.insert("a".to_string(), 3.0);
        balances.insert("b".to_string(), 1.0);
        let sequence: Vec<String> = (0..4)
            .filter_map(|_| {
                pick(
                    &accounts,
                    RoutingStrategy::BalanceWeighted,
                    &state,
                    &HashMap::new(),
                    &balances,
                )
            })
            .collect();
        let a_count = sequence.iter().filter(|id| id.as_str() == "a").count();
        let b_count = sequence.iter().filter(|id| id.as_str() == "b").count();
        assert_eq!((a_count, b_count), (3, 1), "序列 = {sequence:?}");
    }

    #[test]
    fn balance_weighted_uses_baseline_for_unknown_balance() {
        let accounts = vec![account("a", "p", 100, 1), account("b", "p", 101, 2)];
        let state = RoutingState::new();
        // 只有 a 有已知余额；b 用基线（=a 的中位数），不应被饿死
        let mut balances = HashMap::new();
        balances.insert("a".to_string(), 2.0);
        let sequence: Vec<String> = (0..2)
            .filter_map(|_| {
                pick(
                    &accounts,
                    RoutingStrategy::BalanceWeighted,
                    &state,
                    &HashMap::new(),
                    &balances,
                )
            })
            .collect();
        assert!(sequence.contains(&"b".to_string()), "序列 = {sequence:?}");
    }

    #[test]
    fn priority_weighted_biases_toward_better_priority() {
        let accounts = vec![account("a", "p", 100, 1), account("b", "p", 200, 2)];
        let state = RoutingState::new();
        let sequence: Vec<String> = (0..3)
            .filter_map(|_| {
                pick(
                    &accounts,
                    RoutingStrategy::PriorityWeighted,
                    &state,
                    &HashMap::new(),
                    &HashMap::new(),
                )
            })
            .collect();
        let a_count = sequence.iter().filter(|id| id.as_str() == "a").count();
        assert_eq!(a_count, 2, "序列 = {sequence:?}");
    }

    #[test]
    fn strategy_applies_within_top_provider_group_only() {
        // 队首组（优先级更小）是 p；策略只作用于 p 内，不会把请求给到 q
        let accounts = vec![
            account("q1", "q", 90, 1),
            account("p1", "p", 100, 2),
            account("p2", "p", 101, 3),
        ];
        let state = RoutingState::new();
        let config = settings(
            RoutingStrategy::RoundRobin,
            &[("p", RoutingStrategy::RoundRobin), ("q", RoutingStrategy::Priority)],
        );
        let keys = CooldownKeys::new("m");
        let counts = HashMap::new();
        let balances = HashMap::new();
        // 第一次：队首组是 q（优先级 90）→ 取 q1
        let first = pick_account(&accounts, &keys, &counts, &[], 1_000, &config, &state, &balances);
        assert_eq!(account_id(&first.unwrap_or(Value::Null)), Some("q1"));
        // 移除 q 的账号后，队首组变成 p，轮询在 p 内展开
        let only_p = vec![account("p1", "p", 100, 2), account("p2", "p", 101, 3)];
        let p_first = pick_account(&only_p, &keys, &counts, &[], 1_000, &config, &state, &balances);
        let p_second = pick_account(&only_p, &keys, &counts, &[], 1_000, &config, &state, &balances);
        assert_eq!(account_id(&p_first.unwrap_or(Value::Null)), Some("p1"));
        assert_eq!(account_id(&p_second.unwrap_or(Value::Null)), Some("p2"));
    }
}

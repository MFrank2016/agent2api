//! 选路策略注册表 —— 字符串 id → 策略实现。
//!
//! ── 为什么要有这一层（Task 4.2）──────────────────────────────
//! 改造前策略是 [`super::strategy::RoutingStrategy`] 枚举上的一个 `match`：
//! 新增一档要同时改枚举、`as_str`、`label`、`hint` 和选路器里的 `match`，
//! 也没法让插件提供策略。注册表把「策略 = 一个能对座位排序的实现」这件事
//! 抽出来（[`Strategy`]），内置的七档只是注册表里的七条记录：调用方按 id
//! 取实现（[`lookup`]），[`all`] 供界面 / 文档列出全部内置策略。
//!
//! ── 排序逻辑仍然只有一份 ────────────────────────────────────
//! 内置七档全部委托给 [`super::strategy::order_seats`]（与 `order_group`
//! 逐字同语义的那份实现），注册表本身**不做**排序。于是「按 id 取策略」与
//! 「按枚举选路」在 Task 4.4 接上之前就已经共用同一段代码，不会分叉。
//!
//! ── `global-round-robin` 为什么在策略层等于 `priority` ────────
//! 它是**池级**策略：轮转边界是整条候选队列（跨家），由 `order_candidates`
//! 在拼完所有组之后统一 `rotate_left`。策略层只拿到「一家之内」的一组座位，
//! 在这一层再转一次会变成「整表转一次 + 组内又转一次」，与上游 `rotate` 不等价。
//!
//! ── 失败一律 fail-open ─────────────────────────────────────
//! 认不出的 id（空串 / 拼错 / 旧配置里已删除的策略）回落 `priority`：策略是
//! 「把请求摊开」的优化，认不出时按最保守的顺序（严格优先级）走，绝不因为
//! 一个配置字符串让请求无路可走。

use super::seat::Seat;
use super::strategy::order_seats;
use super::{RoutingState, RoutingStrategy};

/// 策略排序要读的**本次请求**上下文（有状态策略的输入）。
///
/// 与 [`Strategy::order`] 的「只有座位表」相比，多出来的就是这一层：家内轮询
/// 的游标要 `(pool, 模型)`、LRU 要 `now`、加权轮询要推进 SWRR 权重 —— 没有它，
/// 有状态策略根本没法表达成 `order(pool, seats)`。
pub struct StrategyCtx<'a> {
    /// 跨请求的运行时状态（轮询游标 / LRU 时刻 / SWRR 权重）
    pub state: &'a RoutingState,
    /// 本请求的**客户端模型名**（家内轮询游标的第二维，与冷却键同源）
    pub model: &'a str,
    /// 当前时刻（毫秒；LRU 记「上次被选中」用）
    pub now: i64,
    /// 是否推进有状态策略（`false` = 预演：只排序、不写任何运行时状态）
    pub advance: bool,
}

/// 一条选路策略：把一组座位排成先后，返回**座位下标**的排列。
///
/// 契约：
///   - 返回 `0..seats.len()` 的一个排列（不重不漏）；
///   - 空座位表返回空表；
///   - 只读座位与 `ctx`，不碰 rest / 粘性 / 垫底（那些留在选路器里）；
///   - `ctx.advance == false` 时**不得**写任何运行时状态。
///
/// `pool` 是「这组座位属于谁」：内置策略里只有家内轮询用得上（游标键
/// `(provider, 模型)`），其余忽略；插件策略可以用它区分池子。
///
/// `Sync` 是必需的：策略以 `&'static dyn Strategy` 登记在静态表里，而 `static`
/// 要求类型 `Sync`。
pub trait Strategy: Sync {
    fn order(&self, ctx: &StrategyCtx<'_>, pool: &str, seats: &[Seat]) -> Vec<usize>;
}

/// 内置策略：枚举值 + 名字的薄壳，排序委托给 [`super::strategy::order_seats`]。
struct Builtin(RoutingStrategy);

impl Strategy for Builtin {
    fn order(&self, ctx: &StrategyCtx<'_>, pool: &str, seats: &[Seat]) -> Vec<usize> {
        order_seats(self.0, ctx, pool, seats)
    }
}

// 七档内置策略各一个静态实例：注册表里存的是它们的引用，`lookup` 因此可以
// 返回 `&'static dyn Strategy` 而不必每次现造一个（策略本身无状态）。
static PRIORITY: Builtin = Builtin(RoutingStrategy::Priority);
static LEAST_CONNECTIONS: Builtin = Builtin(RoutingStrategy::LeastConnections);
static ROUND_ROBIN: Builtin = Builtin(RoutingStrategy::RoundRobin);
static GLOBAL_ROUND_ROBIN: Builtin = Builtin(RoutingStrategy::GlobalRoundRobin);
static LEAST_RECENTLY_USED: Builtin = Builtin(RoutingStrategy::LeastRecentlyUsed);
static BALANCE_WEIGHTED: Builtin = Builtin(RoutingStrategy::BalanceWeighted);
static PRIORITY_WEIGHTED: Builtin = Builtin(RoutingStrategy::PriorityWeighted);

/// 注册表本体：id 用 [`RoutingStrategy::as_str`] 的**规范**形态，顺序与
/// `RoutingStrategy::ALL` 一致（测试钉住两者同名单）。
static ALL: [(&str, &dyn Strategy); 7] = [
    ("priority", &PRIORITY),
    ("least-connections", &LEAST_CONNECTIONS),
    ("round-robin", &ROUND_ROBIN),
    ("global-round-robin", &GLOBAL_ROUND_ROBIN),
    ("least-recently-used", &LEAST_RECENTLY_USED),
    ("balance-weighted", &BALANCE_WEIGHTED),
    ("priority-weighted", &PRIORITY_WEIGHTED),
];

/// 全部内置策略：`(规范 id, 实现)`。界面下拉与文档共用这一份名单。
pub fn all() -> &'static [(&'static str, &'static dyn Strategy)] {
    &ALL
}

/// 按 id 取策略实现。
///
/// 走 [`RoutingStrategy::from_config`] 解析，因此旧配置里的别名
/// （`order` / `rotate` / `provider` / `usage` / `lru` / snake_case）一并认得；
/// 认不出（空串 / 拼错）回落 `priority`（fail-open，见模块头）。
pub fn lookup(id: &str) -> &'static dyn Strategy {
    let strategy = RoutingStrategy::from_config(id);
    ALL.iter()
        .find(|(name, _)| *name == strategy.as_str())
        .map(|(_, implementation)| *implementation)
        .unwrap_or(&PRIORITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用座位：只填策略真正读的字段，其余给「无」。
    fn seat(id: &str, priority: i64, added_at: i64, in_flight: usize) -> Seat {
        Seat {
            account: json!({ "id": id, "provider": "p", "priority": priority }),
            id: id.to_string(),
            provider: "p".to_string(),
            priority,
            added_at,
            balance: None,
            in_flight,
            resting: None,
            sunk: None,
            limiter_blocked: false,
            sticky: false,
            model_resting: None,
            max_rpm_blocked: false,
            reserve: false,
        }
    }

    fn ctx<'a>(state: &'a RoutingState, advance: bool) -> StrategyCtx<'a> {
        StrategyCtx {
            state,
            model: "m",
            now: 1_000,
            advance,
        }
    }

    fn ids_of<'a>(order: &[usize], seats: &'a [Seat]) -> Vec<&'a str> {
        order
            .iter()
            .map(|index| seats[*index].id.as_str())
            .collect()
    }

    const IDS: [&str; 7] = [
        "priority",
        "least-connections",
        "round-robin",
        "global-round-robin",
        "least-recently-used",
        "balance-weighted",
        "priority-weighted",
    ];

    #[test]
    fn registry_resolves_all_seven_ids() {
        let state = RoutingState::new();
        let ctx = ctx(&state, true);
        for id in IDS {
            assert!(
                lookup(id).order(&ctx, "p", &[]).is_empty(),
                "策略 {id} 没有解析到实现"
            );
        }
    }

    #[test]
    fn all_lists_the_seven_canonical_ids() {
        let names: Vec<&str> = all().iter().map(|(id, _)| *id).collect();
        assert_eq!(names, IDS.to_vec());
        // `all()` 与 `RoutingStrategy::ALL` 必须是同一份名单（两处都声称「全部策略」）
        let from_enum: Vec<&str> = RoutingStrategy::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, from_enum);
    }

    #[test]
    fn empty_seats_yield_empty_order() {
        let state = RoutingState::new();
        let ctx = ctx(&state, true);
        for (id, strategy) in all() {
            assert!(strategy.order(&ctx, "p", &[]).is_empty(), "策略 {id}");
        }
    }

    #[test]
    fn every_strategy_returns_a_permutation() {
        let state = RoutingState::new();
        let ctx = ctx(&state, true);
        let seats = vec![
            seat("a", 100, 1, 3),
            seat("b", 100, 2, 0),
            seat("c", 50, 3, 1),
            seat("d", 200, 4, 0),
        ];
        let expected: Vec<usize> = (0..seats.len()).collect();
        for (id, strategy) in all() {
            let mut order = strategy.order(&ctx, "p", &seats);
            order.sort_unstable();
            assert_eq!(order, expected, "策略 {id} 返回的不是座位下标排列");
        }
    }

    #[test]
    fn priority_orders_by_priority_then_added_at() {
        let state = RoutingState::new();
        let ctx = ctx(&state, true);
        // 乱序传入：策略必须自己按 (优先级, 加入时间) 排
        let seats = vec![
            seat("late", 100, 900, 0),
            seat("best", 50, 5, 0),
            seat("early", 100, 100, 0),
        ];
        let order = lookup("priority").order(&ctx, "p", &seats);
        assert_eq!(ids_of(&order, &seats), vec!["best", "early", "late"]);
    }

    #[test]
    fn least_connections_orders_by_in_flight() {
        let state = RoutingState::new();
        let ctx = ctx(&state, true);
        let seats = vec![
            seat("busy", 100, 1, 5),
            seat("idle_late", 300, 4, 0),
            seat("idle_early", 200, 2, 0),
            seat("half", 50, 3, 2),
        ];
        let order = lookup("least-connections").order(&ctx, "p", &seats);
        // 在途少的在前；同为 0 时按 (优先级, 加入时间) 兜底
        assert_eq!(
            ids_of(&order, &seats),
            vec!["idle_early", "idle_late", "half", "busy"]
        );
    }

    /// `global-round-robin` 的轮转是**整表**层面的事（`order_candidates` 做），
    /// 策略层只交优先级序 —— 否则「整表转一次 + 组内又转一次」会与上游
    /// `rotate` 不等价。
    #[test]
    fn global_round_robin_orders_by_priority() {
        let state = RoutingState::new();
        let ctx = ctx(&state, true);
        let seats = vec![seat("b", 200, 2, 0), seat("a", 100, 1, 0)];
        let order = lookup("global-round-robin").order(&ctx, "p", &seats);
        assert_eq!(ids_of(&order, &seats), vec!["a", "b"]);
    }

    #[test]
    fn round_robin_rotates_and_preview_does_not_advance() {
        let state = RoutingState::new();
        let seats = vec![seat("a", 100, 1, 0), seat("b", 101, 2, 0)];
        let strategy = lookup("round-robin");
        // 预演（advance = false）：不消耗游标，永远是基础序
        for _ in 0..2 {
            let order = strategy.order(&ctx(&state, false), "p", &seats);
            assert_eq!(ids_of(&order, &seats), vec!["a", "b"]);
        }
        // 真跑：依次轮流
        let first = strategy.order(&ctx(&state, true), "p", &seats);
        let second = strategy.order(&ctx(&state, true), "p", &seats);
        assert_eq!(ids_of(&first, &seats), vec!["a", "b"]);
        assert_eq!(ids_of(&second, &seats), vec!["b", "a"]);
    }

    #[test]
    fn lookup_accepts_legacy_aliases_and_falls_back_to_priority() {
        // 旧配置里的别名继续认得（`lru` → `least-recently-used`）
        assert!(std::ptr::eq(lookup("lru"), lookup("least-recently-used")));
        // 认不出的 id 一律回落 priority（fail-open，绝不因配置拼错无路可走）
        assert!(std::ptr::eq(lookup("no-such-strategy"), lookup("priority")));
        assert!(std::ptr::eq(lookup(""), lookup("priority")));
    }
}

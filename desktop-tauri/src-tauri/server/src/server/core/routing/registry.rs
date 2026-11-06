//! 选路策略注册表 —— 字符串 id → 策略实现。
//!
//! ── 为什么要有这一层（Task 4.2）──────────────────────────────
//! 改造前策略是 [`super::strategy::RoutingStrategy`] 枚举上的一个 `match`：
//! 新增一档要同时改枚举、`as_str`、`label`、`hint` 和选路器里的 `match`，
//! 也没法让插件提供策略。注册表把「策略 = 一个能对座位排序的实现」这件事
//! 抽出来（[`Strategy`]），内置的七档只是注册表里的七条记录：调用方按 id
//! 取实现（[`lookup`]），[`all`] 供界面 / 文档列出全部内置策略。
//! Task 4.5 又加了两档注册表独有的 id（`free-first` / `drain`），见下文。
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
//!
//! ── `free-first` / `drain`（Task 4.5）──────────────────────
//! 这两档是**注册表独有**的 id，`RoutingStrategy` 枚举与配置校验**尚未**加
//! 变体：界面下拉 / 配置选择器在后续任务里才接。因此 [`lookup`] 先按规范 id
//! 直接命中注册表，再走 `RoutingStrategy::from_config`（后者认不出新 id 会回落
//! `priority`，不能当唯一入口）。
//!   - `free-first`：模型 id 以 `:free` / `-free` 结尾的座位排在最前，免费组
//!     与付费组内各自按 `(优先级, 加入时间)`；模型 id 取 `ctx.model`（其次
//!     账号的 `model`，**不用展示名 `name`**），取法见 [`model_id_of`]。
//!   - `drain`：按 `(优先级, 加入时间)` 的顺序逐条取用，每次调用把**池级**
//!     游标推进一格（用完一轮回到队首）。游标键是 provider，**不按模型分**
//!     —— 见 [`RoutingState::next_drain`]。
//!
//! ── `fastest` / `cheapest` 为什么没登记（刻意推迟）──────────
//! [`Seat`] 里没有首字延迟（ttft）、吞吐（tps）或价格读数；要在选路热路径里
//! 用它们，得先接一套「按模型 / 按账号」的速度与价格数据源，那是独立的一件
//! 事。这里**不**登记同名别名：配置写 `fastest` / `cheapest` 时按 fail-open
//! 回落 `priority`，而不是挂一个「名字对、行为却仍是优先级」的假策略 ——
//! 后者会让用户以为策略生效了，比明确不可用更难排查。

use super::seat::Seat;
use super::strategy::order_seats;
use super::{RoutingState, RoutingStrategy};

/// `free-first` 判断「免费模型」用的模型 id。
///
/// 优先级：**`ctx.model`（本次请求真正要走的模型）优先**，其次账号记录上的
/// `model`（插件 / 自定义家可能带）。
///
/// **不读账号的 `name`**：那是展示名，几乎每个账号都有。读它会让策略按标签
/// 而不是按被路由的模型分类 —— 一个叫 `cline-free` 的账号会对所有模型都被
/// 判成免费，而正常命名的账号请求 `llama:free` 反被判成付费。
///
/// 判定只看后缀：以 `:free` 或 `-free` 结尾即视为免费（与 Cline 免费池的
/// `poolside/laguna-s-2.1:free` 等模型 id 同形），大小写不敏感。
fn model_id_of<'a>(seat: &'a Seat, requested: &'a str) -> &'a str {
    // `ctx.model` 优先：它是本次请求真正要走的模型。
    if !requested.trim().is_empty() {
        return requested;
    }
    // 兜底：账号记录上的 `model`。没有就返回空串（不算免费）。
    seat.account
        .get("model")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("")
}

fn is_free_model(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    normalized.ends_with(":free") || normalized.ends_with("-free")
}

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
/// `pool` 是「这组座位属于谁」：内置策略里家内轮询（游标键
/// `(provider, 模型)`）与 `drain`（游标键 `provider`）用得上，其余忽略；
/// 插件策略可以用它区分池子。
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

/// `free-first`：免费模型优先（后缀判定见 [`is_free_model`]），组内按
/// `(优先级, 加入时间)`。纯函数，不读写运行时状态。
struct FreeFirst;

impl Strategy for FreeFirst {
    fn order(&self, ctx: &StrategyCtx<'_>, _pool: &str, seats: &[Seat]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..seats.len()).collect();
        order.sort_by_key(|index| {
            let seat = &seats[*index];
            (
                // `false`（免费）排在 `true`（付费）之前
                !is_free_model(model_id_of(seat, ctx.model)),
                seat.priority,
                seat.added_at,
            )
        });
        order
    }
}

/// `drain`：按 `(优先级, 加入时间)` 逐条取用，每次调用推进**池级**游标。
///
/// 预演（`ctx.advance == false`）只给基础序、不消耗游标；单座位不推进
/// （与 [`order_seats`] 对轮询的口径一致：只有一个号的家不该被一条请求
/// 消耗掉一次游标位）。
struct Drain;

impl Strategy for Drain {
    fn order(&self, ctx: &StrategyCtx<'_>, pool: &str, seats: &[Seat]) -> Vec<usize> {
        let len = seats.len();
        let mut order: Vec<usize> = (0..len).collect();
        order.sort_by_key(|index| (seats[*index].priority, seats[*index].added_at));
        if len <= 1 || !ctx.advance {
            return order;
        }
        let start = ctx.state.next_drain(pool, len);
        order.rotate_left(start % len);
        order
    }
}

static FREE_FIRST: FreeFirst = FreeFirst;
static DRAIN: Drain = Drain;

/// 注册表本体：id 用 [`RoutingStrategy::as_str`] 的**规范**形态，前七条顺序与
/// `RoutingStrategy::ALL` 一致（测试钉住两者同名单），后两条是 Task 4.5 的
/// 注册表独有 id（枚举 / 配置尚未加变体，见模块头）。
static ALL: [(&str, &dyn Strategy); 9] = [
    ("priority", &PRIORITY),
    ("least-connections", &LEAST_CONNECTIONS),
    ("round-robin", &ROUND_ROBIN),
    ("global-round-robin", &GLOBAL_ROUND_ROBIN),
    ("least-recently-used", &LEAST_RECENTLY_USED),
    ("balance-weighted", &BALANCE_WEIGHTED),
    ("priority-weighted", &PRIORITY_WEIGHTED),
    ("free-first", &FREE_FIRST),
    ("drain", &DRAIN),
];

/// 全部内置策略：`(规范 id, 实现)`。界面下拉与文档共用这一份名单。
pub fn all() -> &'static [(&'static str, &'static dyn Strategy)] {
    &ALL
}

/// 按 id 取策略实现。
///
/// 先按**规范 id** 直接命中注册表（`free-first` / `drain` 这类注册表独有的
/// 策略不在 `RoutingStrategy` 枚举里，`from_config` 认不出会回落 `priority`）；
/// 未命中再走 [`RoutingStrategy::from_config`]，因此旧配置里的别名
/// （`order` / `rotate` / `provider` / `usage` / `lru` / snake_case）一并认得。
/// 两者都认不出（空串 / 拼错）回落 `priority`（fail-open，见模块头）。
pub fn lookup(id: &str) -> &'static dyn Strategy {
    let canonical = id.trim().to_ascii_lowercase();
    if let Some((_, implementation)) = ALL.iter().find(|(name, _)| *name == canonical) {
        return *implementation;
    }
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

    const IDS: [&str; 9] = [
        "priority",
        "least-connections",
        "round-robin",
        "global-round-robin",
        "least-recently-used",
        "balance-weighted",
        "priority-weighted",
        "free-first",
        "drain",
    ];

    /// 带 `model` 字段的座位（`free-first` 读它判断免费模型）。
    fn seat_model(id: &str, priority: i64, added_at: i64, model: &str) -> Seat {
        let mut seat = seat(id, priority, added_at, 0);
        seat.account["model"] = json!(model);
        seat
    }

    #[test]
    fn registry_resolves_all_canonical_ids() {
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
    fn all_lists_the_canonical_ids_and_extends_the_enum() {
        let names: Vec<&str> = all().iter().map(|(id, _)| *id).collect();
        assert_eq!(names, IDS.to_vec());
        // 内置七档与 `RoutingStrategy::ALL` 同名单（两处都声称「全部策略」）；
        // `free-first` / `drain` 是**注册表独有**的 id，枚举与配置尚未加变体。
        let from_enum: Vec<&str> = RoutingStrategy::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names[..from_enum.len()], from_enum[..]);
        assert_eq!(names.len(), from_enum.len() + 2);
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

    /// `free-first`：模型 id 以 `:free` / `-free` 结尾的座位排在最前；免费组内
    /// 与付费组内都按 `(优先级, 加入时间)` 排（与 `compare_by_priority` 同口径）。
    ///
    /// 这里把 `ctx.model` 置空，让分类走「账号 `model`」兜底：请求模型非空时
    /// 所有座位共享同一个分类（见下面的优先级用例）。
    #[test]
    fn free_first_sorts_free_models_first_then_priority_and_added_at() {
        let state = RoutingState::new();
        let mut ctx = ctx(&state, true);
        ctx.model = "";
        let seats = vec![
            seat_model("paid_best", 1, 10, "gpt-4o"),
            seat_model("free_late", 100, 900, "llama:free"),
            seat_model("free_early", 100, 100, "deepseek-free"),
            seat_model("free_best", 50, 500, "qwen:free"),
        ];
        let order = lookup("free-first").order(&ctx, "p", &seats);
        // 免费组（free_best → free_early → free_late）整体在付费组之前；
        // 免费组内按 (priority, added_at)，付费组同样。
        assert_eq!(
            ids_of(&order, &seats),
            vec!["free_best", "free_early", "free_late", "paid_best"]
        );
    }

    /// `ctx.model` 是分类的**首选**来源：座位既没有 `model` 也没有 `name` 时
    /// 只能靠它；账号自带 `model` 也不能盖过本次请求的模型。
    #[test]
    fn free_first_uses_requested_model_for_seats_without_model_or_name() {
        let state = RoutingState::new();
        let mut ctx = ctx(&state, true);
        ctx.model = "llama:free";
        let bare = seat("bare", 500, 5, 0);
        // 既没有 model 也没有 name：分类只能来自 ctx.model
        assert_eq!(model_id_of(&bare, ctx.model), "llama:free");
        assert!(is_free_model(model_id_of(&bare, ctx.model)));

        // ctx.model 优先于账号 model：账号写 gpt-4o 也按请求的免费模型分类
        let paid_account = seat_model("paid_account", 1, 10, "gpt-4o");
        let seats = vec![bare, paid_account];
        let order = lookup("free-first").order(&ctx, "p", &seats);
        // 两者都判为免费 → 回落到 (priority, added_at)
        assert_eq!(ids_of(&order, &seats), vec!["paid_account", "bare"]);
    }

    /// 账号的 `name` 是展示名，**不参与**分类：叫 `acme-free` 的账号在付费
    /// 模型请求里不能被提升。
    #[test]
    fn free_first_does_not_use_account_display_name() {
        let state = RoutingState::new();
        let ctx = ctx(&state, true); // ctx.model = "m"（付费）
        let mut named = seat("named_free", 500, 5, 0);
        named.account["name"] = json!("acme-free");
        let seats = vec![named, seat("plain", 1, 10, 0)];
        let order = lookup("free-first").order(&ctx, "p", &seats);
        assert_eq!(ids_of(&order, &seats), vec!["plain", "named_free"]);
    }

    /// 请求模型为空时（预演 / 无模型上下文的直接调用）回落到账号的 `model`。
    #[test]
    fn free_first_falls_back_to_account_model_when_request_is_empty() {
        let state = RoutingState::new();
        let mut ctx = ctx(&state, true);
        ctx.model = "";
        let seats = vec![
            seat_model("acct_paid", 1, 10, "gpt-4o"),
            seat_model("acct_free", 500, 5, "llama:free"),
        ];
        let order = lookup("free-first").order(&ctx, "p", &seats);
        assert_eq!(ids_of(&order, &seats), vec!["acct_free", "acct_paid"]);
    }

    /// `drain`：按 `(优先级, 加入时间)` 的座位顺序逐条取用，每次调用把游标
    /// 推进一格（用完一轮再回到队首）；预演不消耗游标。
    #[test]
    fn drain_cycles_the_pool_and_advances_its_cursor() {
        let state = RoutingState::new();
        let seats = vec![
            seat("late", 100, 900, 0),
            seat("best", 50, 5, 0),
            seat("mid", 75, 100, 0),
        ];
        let strategy = lookup("drain");
        // 预演（advance = false）：永远是基础序，不推进游标
        for _ in 0..2 {
            let order = strategy.order(&ctx(&state, false), "p", &seats);
            assert_eq!(ids_of(&order, &seats), vec!["best", "mid", "late"]);
        }
        // 真跑：依次从队首滑到队尾，再绕回队首
        let sequence: Vec<Vec<&str>> = (0..4)
            .map(|_| ids_of(&strategy.order(&ctx(&state, true), "p", &seats), &seats))
            .collect();
        assert_eq!(sequence[0], vec!["best", "mid", "late"]);
        assert_eq!(sequence[1], vec!["mid", "late", "best"]);
        assert_eq!(sequence[2], vec!["late", "best", "mid"]);
        assert_eq!(sequence[3], vec!["best", "mid", "late"]);
    }

    /// `drain` 的游标按**池**记忆：另一家从队首开始，不受上一家推进影响。
    #[test]
    fn drain_cursor_is_per_pool() {
        let state = RoutingState::new();
        let seats = vec![seat("a", 100, 1, 0), seat("b", 101, 2, 0)];
        let strategy = lookup("drain");
        let first = strategy.order(&ctx(&state, true), "p", &seats);
        assert_eq!(ids_of(&first, &seats), vec!["a", "b"]);
        // 同一家再取一次 → 游标已推进
        let second = strategy.order(&ctx(&state, true), "p", &seats);
        assert_eq!(ids_of(&second, &seats), vec!["b", "a"]);
        // 换一家 → 从队首开始
        let other = strategy.order(&ctx(&state, true), "q", &seats);
        assert_eq!(ids_of(&other, &seats), vec!["a", "b"]);
    }

    #[test]
    fn lookup_accepts_legacy_aliases_and_falls_back_to_priority() {
        // 旧配置里的别名继续认得（`lru` → `least-recently-used`）
        assert!(std::ptr::eq(lookup("lru"), lookup("least-recently-used")));
        // 注册表独有的新 id 按**规范名**命中，不会回落 priority
        assert!(!std::ptr::eq(lookup("free-first"), lookup("priority")));
        assert!(!std::ptr::eq(lookup("drain"), lookup("priority")));
        // 认不出的 id 一律回落 priority（fail-open，绝不因配置拼错无路可走）
        assert!(std::ptr::eq(lookup("no-such-strategy"), lookup("priority")));
        assert!(std::ptr::eq(lookup(""), lookup("priority")));
    }
}

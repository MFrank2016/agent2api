//! 选路「座位」：把一个候选账号在**本次请求**里要读的事实收成一个结构。
//!
//! ── 为什么要有这一层 ─────────────────────────────────────────
//! 改造前，策略（[`super::strategy`]）直接读 `Value` + 五六张 map
//! （余额 / 在途 / 垫底 / 休息 / 熔断 / 粘性 / RPM），每加一条事实就要给
//! `order_candidates` 加一个参数、给每个策略加一次查表。`Seat` 把这些
//! **本次请求的判定事实**收成一张平表：策略只读 `Seat`，不再关心事实从哪来，
//! 后续注册表化（Task 4.2）与 golden tests（Task 4.3）都只需要看这一种形状。
//!
//! ── 为什么是 owned（没有 `Seat<'a>`）────────────────────────
//! `order_candidates` 现在拿到的是 `Vec<Value>`（候选已克隆）；如果 `Seat`
//! 借用这些 `Value`，策略注册表在 `&[Seat]` 上排序时会被生命周期绑死，
//! 调用侧还得多活一层 `Vec<Value>`。座位数量 = 候选数（≤ 账号总数），
//! 克隆一次的代价可以忽略，换来的是「座位可以自由移动 / 重排 / 跨函数传递」。
//!
//! ── 本模块的边界 ─────────────────────────────────────────
//! 只做「账号 + 外部事实 → 座位」的**纯映射**，不排序、不推进状态、不改
//! `plan_route` 的现有行为。`reserve` 是选路阶段的标记（`maxAccounts`），
//! 构造时恒为 `false`，由调用方在完成排序后写入 —— 这样 `seats_from` 不需要
//! 知道「排在第几位」，策略层也能在排序前就拿到完整的候选事实。

use std::collections::HashMap;

use serde_json::Value;

use super::{account_id, priority_key, provider_of, PlanMeta};

/// 一个候选账号在**本次请求**里的全部判定事实。
///
/// 字段全部是「读一次快照」的标量 / 小对象：策略层只读它，不再回查 map。
#[derive(Clone, Debug)]
pub struct Seat {
    /// 账号公开形态（转发最终要用的就是它）
    pub account: Value,
    /// 账号 id（非空；没有 id 的候选不会形成座位）
    pub id: String,
    /// 账号所属 provider id（缺失按默认家兜底，见 [`provider_of`]）
    pub provider: String,
    /// 归一后的优先级（与 [`super::strategy::order_candidates`] 的排序键同口径：
    /// `routingPriority` 投影优先，其次账号库里的 `priority`）
    pub priority: i64,
    /// 已知剩余余额（未知 = `None`，不是 0）
    pub balance: Option<f64>,
    /// 当前在途请求数（快照里没有 = 0）
    pub in_flight: usize,
    /// 账号级**软**休息截止时刻（网络失败退避；只垫底，不硬跳过）
    pub resting: Option<i64>,
    /// 垫底截止时刻（限流垫底；只后置，不硬跳过）
    pub sunk: Option<i64>,
    /// 被限制器软跳过（余额不足 / Token 周期用量到顶）
    pub limiter_blocked: bool,
    /// 粘性命中（会话上一轮用的就是它）
    pub sticky: bool,
    /// 模型级 rest 截止时刻（只跳过「这个账号 × 这个模型」）
    pub model_resting: Option<i64>,
    /// 滚动 RPM 超限（`maxRpm`）
    pub max_rpm_blocked: bool,
    /// 储备账号（`maxAccounts` 之外；由选路阶段标记，构造时恒 `false`）
    pub reserve: bool,
}

/// 把候选账号映射成座位（顺序与 `candidates` 一致）。
///
/// `counts` 是各账号当前在途请求数（账号 id → 数，`upstream::connections`
/// 的快照）：`in_flight` 的唯一来源，缺键按 0 算。
///
/// ── `model_resting` 的键约定（与 Task 1.2 对齐）──────────────
/// 键是 `(账号 id, 模型名)`，模型名取 `meta.model`。Task 1.2 的
/// `RouteState::rest_mark` 按**写入方给的模型名**落键，`plan_route` 查表时用
/// `CooldownKeys::for_provider` 解析成该家的**上游真名**。因此调用 `seats_from`
/// 时 `meta.model` 必须是同一口径的模型名（有映射的家要传真名，而不是请求名）；
/// 用请求名查会在有映射时静默漏命中。
///
/// 没有 id 的候选（手工编辑出来的坏记录）不形成座位：没有 id 就既不能进
/// 「已试过」集合，也不能作余额 / 粘性 / 计数表的键。
pub fn seats_from(
    candidates: &[Value],
    meta: &PlanMeta<'_>,
    counts: &HashMap<String, usize>,
) -> Vec<Seat> {
    candidates
        .iter()
        .filter_map(|account| {
            let id = account_id(account)?.to_string();
            Some(Seat {
                account: account.clone(),
                provider: provider_of(account).to_string(),
                priority: priority_key(account).0,
                balance: meta.balances.get(&id).copied(),
                in_flight: counts.get(&id).copied().unwrap_or(0),
                resting: meta.resting.get(&id).copied(),
                sunk: meta.sunk.get(&id).copied(),
                limiter_blocked: meta.limiter_blocked.contains(&id),
                sticky: meta.sticky_account == Some(id.as_str()),
                model_resting: meta
                    .model_resting
                    .get(&(id.clone(), meta.model.to_string()))
                    .copied(),
                max_rpm_blocked: meta.rpm_blocked.contains(&id),
                reserve: false,
                id,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::config::RoutingStrategySettings;
    use crate::server::core::routing::{PlanMeta, RoutingState, RoutingStrategy};
    use serde_json::{json, Value};
    use std::collections::{BTreeMap, HashMap, HashSet};

    /// 测试用的一整套外部事实：自己持有各张表，`meta()` 再借用出来。
    ///
    /// `PlanMeta` 里全是借用字段，没法从 helper 直接返回 —— 所以让 helper
    /// 持有所有权，用例按需往表里塞事实。
    struct Facts {
        strategies: RoutingStrategySettings,
        state: RoutingState,
        balances: HashMap<String, f64>,
        model: String,
        sunk: HashMap<String, i64>,
        resting: HashMap<String, i64>,
        account_resting: HashMap<String, i64>,
        model_resting: HashMap<(String, String), i64>,
        open: HashSet<String>,
        limiter_blocked: HashSet<String>,
        rpm_blocked: HashSet<String>,
        max_accounts: HashMap<String, u64>,
        sticky_account: Option<String>,
    }

    impl Facts {
        fn new(model: &str) -> Self {
            Self {
                strategies: RoutingStrategySettings {
                    default: RoutingStrategy::Priority,
                    providers: BTreeMap::new(),
                },
                state: RoutingState::new(),
                balances: HashMap::new(),
                model: model.to_string(),
                sunk: HashMap::new(),
                resting: HashMap::new(),
                account_resting: HashMap::new(),
                model_resting: HashMap::new(),
                open: HashSet::new(),
                limiter_blocked: HashSet::new(),
                rpm_blocked: HashSet::new(),
                max_accounts: HashMap::new(),
                sticky_account: None,
            }
        }

        fn meta(&self) -> PlanMeta<'_> {
            PlanMeta {
                strategies: &self.strategies,
                state: &self.state,
                balances: &self.balances,
                model: &self.model,
                advance: false,
                sunk: &self.sunk,
                resting: &self.resting,
                account_resting: &self.account_resting,
                model_resting: &self.model_resting,
                open: &self.open,
                limiter_blocked: &self.limiter_blocked,
                rpm_blocked: &self.rpm_blocked,
                max_accounts: &self.max_accounts,
                sticky_account: self.sticky_account.as_deref(),
            }
        }
    }

    fn account(id: &str, provider: &str, priority: i64) -> Value {
        json!({
            "id": id,
            "provider": provider,
            "priority": priority,
            "enabled": true,
            "hasCredentials": true,
        })
    }

    /// 直接拼一个 `Seat`（brief 的 `seat(...)` helper，去掉借用后就是结构字面量）。
    fn seat(account: Value, resting: Option<i64>, reserve: bool) -> Seat {
        Seat {
            account,
            id: "a".to_string(),
            provider: "p".to_string(),
            priority: 1,
            balance: None,
            in_flight: 0,
            resting,
            sunk: None,
            limiter_blocked: false,
            sticky: false,
            model_resting: None,
            max_rpm_blocked: false,
            reserve,
        }
    }

    #[test]
    fn seat_carries_rest_and_reserve() {
        let seat = seat(json!({"id":"a","provider":"p"}), Some(1_000), true);
        assert_eq!(seat.resting, Some(1_000));
        assert!(seat.reserve);
    }

    #[test]
    fn seats_from_maps_all_facts() {
        let mut facts = Facts::new("wire-model");
        facts.balances.insert("a".to_string(), 12.5);
        facts.resting.insert("a".to_string(), 5_000);
        facts.sunk.insert("a".to_string(), 6_000);
        facts.limiter_blocked.insert("a".to_string());
        facts.rpm_blocked.insert("a".to_string());
        facts
            .model_resting
            .insert(("a".to_string(), "wire-model".to_string()), 7_000);
        facts.sticky_account = Some("a".to_string());

        let mut counts = HashMap::new();
        counts.insert("a".to_string(), 3usize);

        let candidates = vec![account("a", "p", 1), account("b", "q", 2)];
        let seats = seats_from(&candidates, &facts.meta(), &counts);
        assert_eq!(seats.len(), 2);

        let first = &seats[0];
        assert_eq!(first.id, "a");
        assert_eq!(first.provider, "p");
        assert_eq!(first.priority, 1);
        assert_eq!(first.balance, Some(12.5));
        assert_eq!(first.in_flight, 3);
        assert_eq!(first.resting, Some(5_000));
        assert_eq!(first.sunk, Some(6_000));
        assert!(first.limiter_blocked);
        assert!(first.sticky);
        assert_eq!(first.model_resting, Some(7_000));
        assert!(first.max_rpm_blocked);
        assert!(!first.reserve);
        assert_eq!(first.account["id"], json!("a"));

        let second = &seats[1];
        assert_eq!(second.id, "b");
        assert_eq!(second.provider, "q");
        assert_eq!(second.priority, 2);
        assert_eq!(second.balance, None);
        assert_eq!(second.in_flight, 0);
        assert_eq!(second.resting, None);
        assert_eq!(second.sunk, None);
        assert!(!second.limiter_blocked);
        assert!(!second.sticky);
        assert_eq!(second.model_resting, None);
        assert!(!second.max_rpm_blocked);
        assert!(!second.reserve);
    }

    #[test]
    fn seats_from_unknown_fields_are_none_or_zero() {
        let facts = Facts::new("m");
        let counts = HashMap::new();
        // 一条缺 id 的候选（手工编辑出来的坏记录）不能形成座位：
        // 没有 id 就既不能进 `tried` 也不能作粘性 / 余额的键。
        let candidates = vec![json!({"provider": "p"}), json!({"id": "solo"})];

        let seats = seats_from(&candidates, &facts.meta(), &counts);
        assert_eq!(seats.len(), 1);
        let solo = &seats[0];
        assert_eq!(solo.id, "solo");
        assert_eq!(
            solo.provider,
            crate::server::core::providers::DEFAULT_PROVIDER_ID
        );
        assert_eq!(
            solo.priority,
            crate::server::core::account_store::priority::DEFAULT_PRIORITY
        );
        assert_eq!(solo.balance, None);
        assert_eq!(solo.in_flight, 0);
        assert_eq!(solo.resting, None);
        assert_eq!(solo.sunk, None);
        assert!(!solo.limiter_blocked);
        assert!(!solo.sticky);
        assert_eq!(solo.model_resting, None);
        assert!(!solo.max_rpm_blocked);
        assert!(!solo.reserve);
    }
}

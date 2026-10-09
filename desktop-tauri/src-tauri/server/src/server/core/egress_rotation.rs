//! 轮询出口的进程内状态：round-robin 游标 + least-latency 的 EWMA（重启清零）。
use std::collections::HashMap;
use std::sync::Mutex;

use crate::server::core::proxies::{AccountEgress, ResolvedProxy, RotationPlan, Strategy};

/// EWMA 平滑系数。
const ALPHA: f64 = 0.3;

#[derive(Default)]
struct State { cursor: u64, ewma: HashMap<String, f64> }

static STATE: Mutex<Option<HashMap<String, State>>> = Mutex::new(None);

fn with_state<R>(key: &str, f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = match STATE.lock() { Ok(g) => g, Err(p) => p.into_inner() };
    let map = guard.get_or_insert_with(HashMap::new);
    f(map.entry(key.to_string()).or_default())
}

fn member_key(proxy: &ResolvedProxy) -> String {
    format!("{}://{}:{}", proxy.protocol, proxy.host, proxy.port.map(|p| p.to_string()).unwrap_or_default())
}

fn random_below(bound: u64) -> u64 {
    if bound == 0 { return 0; }
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_err() { return 0; }
    u64::from_le_bytes(buf) % bound
}

/// 本次请求的候选顺序（首个为首选；其后为 onError=next 的重试顺序）。
pub fn candidates(egress: &AccountEgress) -> Vec<ResolvedProxy> {
    match egress {
        AccountEgress::Direct => Vec::new(),
        AccountEgress::Single(proxy) => vec![proxy.clone()],
        AccountEgress::Rotate(plan) => rotate_candidates(plan),
    }
}

fn rotate_candidates(plan: &RotationPlan) -> Vec<ResolvedProxy> {
    let n = plan.members.len();
    if n == 0 { return Vec::new(); }
    match plan.strategy {
        Strategy::RoundRobin => {
            let start = with_state(&plan.group_key, |s| {
                let c = s.cursor; s.cursor = s.cursor.wrapping_add(1); c
            }) as usize % n;
            (0..n).map(|i| plan.members[(start + i) % n].clone()).collect()
        }
        Strategy::Random => {
            let mut items = plan.members.clone();
            for i in (1..items.len()).rev() {
                let j = random_below(i as u64 + 1) as usize;
                items.swap(i, j);
            }
            items
        }
        Strategy::LeastLatency => {
            let mut ranked: Vec<(f64, usize)> = plan.members.iter().enumerate()
                .map(|(i, m)| (with_state(&plan.group_key, |s| *s.ewma.get(&member_key(m)).unwrap_or(&0.0)), i))
                .collect();
            ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            ranked.into_iter().map(|(_, i)| plan.members[i].clone()).collect()
        }
    }
}

fn sample(plan: &RotationPlan, proxy: &ResolvedProxy, value: f64) {
    if plan.strategy != Strategy::LeastLatency { return; }
    let key = member_key(proxy);
    with_state(&plan.group_key, |s| {
        let entry = s.ewma.entry(key).or_insert(value);
        *entry = ALPHA * value + (1.0 - ALPHA) * *entry;
    });
}

/// 记录一次成功延迟样本（毫秒）。
pub fn record_success(plan: &RotationPlan, proxy: &ResolvedProxy, latency_ms: f64) { sample(plan, proxy, latency_ms); }

/// 记录一次连接失败（惩罚样本抬高 EWMA，使后续倾向避开该出口）。
pub fn record_failure(plan: &RotationPlan, proxy: &ResolvedProxy, penalty_ms: f64) { sample(plan, proxy, penalty_ms); }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::proxies::OnError;
    fn m(host: &str) -> ResolvedProxy {
        ResolvedProxy { source: "pool".into(), protocol: "http".into(), host: host.into(),
            port: Some(80), username: String::new(), password: String::new(), label: String::new() }
    }

    #[test]
    fn round_robin_cycles_through_all_members() {
        let plan = RotationPlan {
            members: vec![m("1.1.1.1"), m("2.2.2.2"), m("3.3.3.3")],
            strategy: Strategy::RoundRobin, on_error: OnError::Next, group_key: "rr-test".into(),
        };
        let egress = AccountEgress::Rotate(plan);
        let first = candidates(&egress)[0].host.clone();
        let second = candidates(&egress)[0].host.clone();
        let third = candidates(&egress)[0].host.clone();
        assert_eq!(vec![first, second, third], vec!["1.1.1.1", "2.2.2.2", "3.3.3.3"]);
    }

    #[test]
    fn least_latency_prefers_the_fast_member_after_a_failure_penalty() {
        let plan = RotationPlan {
            members: vec![m("1.1.1.1"), m("2.2.2.2")],
            strategy: Strategy::LeastLatency, on_error: OnError::Next, group_key: "ll-test".into(),
        };
        let egress = AccountEgress::Rotate(plan.clone());
        record_success(&plan, &m("2.2.2.2"), 50.0);
        record_failure(&plan, &m("1.1.1.1"), 30_000.0);
        assert_eq!(candidates(&egress)[0].host, "2.2.2.2");
    }
}

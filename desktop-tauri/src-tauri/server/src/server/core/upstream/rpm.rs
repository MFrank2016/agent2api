//! 每分钟请求数（RPM）滑动窗口：按账号 / 密钥记「最近 60 秒打了几次」。
//!
//! ── 为什么要滑动窗口而不是固定分钟桶 ──────────────────────────
//! 固定桶（「第 N 分钟记一个计数」）在整分钟边界会放过 2× 限额：59.9s 打满
//! 一批、60.1s 再打满一批。滑动窗口按**每次命中的时间戳**判定，任意连续
//! 60 秒内都不会超过 `limit`，与上游「每分钟 N 次」的口径一致。
//!
//! ── 两个入口的分工 ─────────────────────────────────────────
//!   [`RpmWindow::allow`]   占名额：能发就记一次并返回 true，否则返回 false
//!   [`RpmWindow::wait_ms`] 只问不占：0 = 现在就能发；>0 = 还要等多少毫秒
//! 选路时先 `wait_ms` 挑一个「等得最少」的账号，再 `allow` 真正占名额 ——
//! 两者都先按 `now` 淘汰过期命中，因此可以放心在任意时刻调用。
//!
//! ── 时间从哪来 ─────────────────────────────────────────────
//! `now` 由调用方传入（毫秒时间戳），本模块**不读时钟**：这样测试能精确
//! 控制边界，调用方也能在同一次选路里对多个账号用同一个时间点，避免
//! 「先算的账号时间早、后算的晚」导致的不一致。调用方传同一时钟源的
//! 毫秒值即可（例如 `chrono::Utc::now().timestamp_millis()`）。
//!
//! ── 内存 ────────────────────────────────────────────────────
//! 每个 id 的队列长度上限就是 `limit`（占满即拒，不会继续增长）；过期命中
//! 在该 id 下次被触碰时清掉。`limit == 0` 表示不限速，**一次都不记**
//! （见 [`RpmWindow::allow`]），所以「关了限速」不会留下任何痕迹。
//!
//! 硬约束（同 `upstream` 模块头）：release 是 `panic=abort`，这里不
//! unwrap / expect / panic；锁被投毒时用 `into_inner` 继续用（窗口数据
//! 是尽力而为的限速状态，不值得为它中断转发）。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// 窗口长度：60 秒。命中时间戳 `at` 满足 `now - at >= WINDOW_MS` 即滑出窗口。
const WINDOW_MS: i64 = 60_000;

/// 按 id（账号 / 密钥）统计的 60 秒滑动窗口。可跨线程共享（内部一把锁）。
#[derive(Debug, Default)]
pub struct RpmWindow {
    /// id → 最近 60 秒内的命中时间戳（毫秒，单调不减地追加）
    hits: Mutex<HashMap<String, VecDeque<i64>>>,
}

impl RpmWindow {
    pub fn new() -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// 尝试占用一个名额：能发返回 true（并记一次命中），否则返回 false。
    ///
    /// `limit == 0` 表示**不限速**：直接放行且**不记录**命中 —— 既不占内存，
    /// 也不会在把限额从 0 调大后突然被一批「幽灵命中」挡住。
    ///
    /// 判定顺序：先按 `now` 淘汰过期命中，再看队列长度是否已达 `limit`。
    /// 「已达即拒」= 允许恰好 `limit` 次；被拒的那次**不记录**，所以不会
    /// 因为反复被拒而把队列撑长。
    pub fn allow(&self, id: &str, limit: u64, now: i64) -> bool {
        if limit == 0 {
            return true;
        }
        let mut table = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let queue = table.entry(id.to_string()).or_default();
        evict_expired(queue, now);
        if queue.len() as u64 >= limit {
            return false;
        }
        queue.push_back(now);
        true
    }

    /// 还要等多少毫秒才有名额：0 = 现在就能发，>0 = 至少还要等这么久。
    ///
    /// **只查不占**：不记录命中，也不改窗口内容（除了顺手清掉过期命中）。
    /// 已满时返回值取「最早那次命中滑出窗口」的剩余时间，最少 1ms ——
    /// 0 保留给「现在就能发」，调用方不必再区分「差一点点」和「真的能发」。
    /// `limit == 0`（不限速）恒为 0。
    pub fn wait_ms(&self, id: &str, limit: u64, now: i64) -> i64 {
        if limit == 0 {
            return 0;
        }
        let mut table = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let Some(queue) = table.get_mut(id) else {
            return 0;
        };
        evict_expired(queue, now);
        if (queue.len() as u64) < limit {
            return 0;
        }
        let Some(oldest) = queue.front().copied() else {
            return 0;
        };
        // 时钟回拨时 elapsed 为负：按 0 算，最多等满一个窗口
        let elapsed = (now - oldest).clamp(0, WINDOW_MS);
        (WINDOW_MS - elapsed).max(1)
    }
}

/// 淘汰滑出窗口的命中（队头最旧，弹出到第一个仍在窗口内为止）。
fn evict_expired(queue: &mut VecDeque<i64>, now: i64) {
    while queue.front().is_some_and(|at| now - *at >= WINDOW_MS) {
        queue.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn rpm_window_rolls_at_sixty_seconds() {
        let w = RpmWindow::new();
        assert!(w.allow("a", 2, 0));
        assert!(w.allow("a", 2, 1_000));
        assert!(!w.allow("a", 2, 2_000));
        assert!(w.allow("a", 2, 61_000));
    }

    #[test]
    fn rpm_concurrent_never_exceeds() {
        // 三个线程在同一时刻抢同一个账号的两个名额：只能过两个
        let w = Arc::new(RpmWindow::new());
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let w = Arc::clone(&w);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                w.allow("a", 2, 1_000)
            }));
        }
        let allowed = handles
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .filter(|ok| *ok)
            .count();
        assert_eq!(2, allowed);
    }

    #[test]
    fn rpm_unlimited_does_not_record() {
        let w = RpmWindow::new();
        for now in 0..1_000 {
            assert!(w.allow("a", 0, now));
        }
        assert_eq!(0, w.wait_ms("a", 0, 1_000));
        // limit == 0 = 不限速，且一次都不记 —— 否则这张表会无上限地涨
        assert!(w.hits.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[test]
    fn rpm_wait_ms_is_the_oldest_hit_remaining() {
        let w = RpmWindow::new();
        assert!(w.allow("a", 2, 1_000));
        assert!(w.allow("a", 2, 2_000));
        // 满了：要等最早那一次（1_000）滑出窗口，即 61_000，此刻还有 59_000ms
        assert_eq!(59_000, w.wait_ms("a", 2, 2_000));
        // 只差 1ms 就滑出：至少给 1，不返回 0（0 的语义是「现在就能发」）
        assert_eq!(1, w.wait_ms("a", 2, 60_999));
        // 过了窗口：名额回来了
        assert_eq!(0, w.wait_ms("a", 2, 61_000));
    }
}

//! 脱敏配置的**读侧接缝**（seam）。
//!
//! 出站脱敏要不要做、用哪份规则，由这里回答；调用方（[`super::mask_outbound`]）
//! 只认 [`MaskConfig`] 与 [`masker`] 两个入口。
//!
//! ## 开关从哪来
//!
//! 三项设置（`maskSecrets` / `maskPii` / `maskWords`）由 `server::config` 解析
//! 进运行期快照；[`active`] 把三档合并成一个 `enabled`（任一打开或词表非空），
//! [`masker`] 按同一份设置选择规则族。改完设置下一个请求就生效，不必重启。
//!
//! 关闭是**安全默认**：没有配置就没有理由改写用户请求体（改写会改变字节，
//! 进而影响上游缓存与排障对照）。关闭时 [`super::mask_outbound`] 连 JSON 都
//! 不解析，出站字节与改造前逐字节一致。
//!
//! ## fail-closed
//!
//! [`masker`] 返回 `None` 只表示「配置要求脱敏，却读不到配置」—— 调用方据此
//! 返回 503、**一个字节都不发**。当前配置读侧在未初始化 / 读锁中毒时会明确
//! 返回 `None`（见 `config::mask_settings_checked`），而不是拿一份空规则悄悄
//! 放行；两者都关且词表为空时它仍返回 `Some`（空规则集），因为「要不要脱敏」
//! 由 [`active`] 的 `enabled` 决定，不由 `None` 决定。
//!
//! ## 测试接缝
//!
//! 配置是进程级的，但测试需要「开启 + 指定规则」这种组合。`#[cfg(test)]`
//! 下用线程局部覆盖（[`testing`]）：每个测试线程有自己的覆盖值，互不串味；
//! 覆盖只在测试构建里存在，生产代码看不到这条路径。

use std::sync::Arc;

use crate::server::config::MaskSettings;

use super::RulesMasker;

/// 脱敏开关与脱敏器的快照（**同一次配置读取**的产物）。
///
/// 两件事必须来自同一份设置：只读「开不开」再单独读「用哪个规则」，两次
/// 读之间可能插进一次配置写入（开 → 关），于是门禁放行、规则却是另一份
/// 快照的 —— 这正是 fail-closed 要避免的窗口。
#[derive(Clone, Default)]
pub struct MaskConfig {
    pub enabled: bool,
    masker: Option<Arc<RulesMasker>>,
}

impl MaskConfig {
    /// 本次快照对应的脱敏器（`None` = 未开启，或配置读不到）。
    ///
    /// 读不到配置时 [`active`] 返回「开启 + 无脱敏器」，调用方据此 503。
    pub fn masker(&self) -> Option<Arc<RulesMasker>> {
        self.masker.clone()
    }
}

/// 当前的脱敏开关与脱敏器。
///
/// `enabled = maskSecrets || maskPii || !maskWords.is_empty()`：词表非空本身
/// 也算开启，否则用户填了词却发现没生效。
///
/// ── 读不到配置时必须 **fail-closed** ───────────────────────────
/// 配置读不到（未初始化 / 读锁中毒）时返回 `enabled: true` 且没有脱敏器，
/// 而不是回落「全关」：后者会让用户明明开着脱敏、却因为一次读锁故障把明文
/// 发给上游。开启后 [`super::mask_outbound`] 会取脱敏器，拿不到就返回 503、
/// **一个字节都不发**。
///
/// ── 为什么关闭时不构造脱敏器 ─────────────────────────────────
/// 关闭是默认态，热路径不该为它付一次规则缓存查找的代价。`enabled` 为
/// false 时调用方直接放行，`masker` 留空。
pub fn active() -> MaskConfig {
    #[cfg(test)]
    {
        if testing::unreadable_snapshot() {
            return MaskConfig {
                enabled: true,
                masker: None,
            };
        }
        if let Some(config) = testing::snapshot() {
            return config;
        }
    }
    match crate::server::config::mask_settings_checked() {
        Some(settings) => {
            let enabled = settings.enabled();
            let masker = enabled.then(|| masker_for(settings));
            MaskConfig { enabled, masker }
        }
        // 读不到配置：门禁按开启处理，但没有脱敏器 → 调用方 503（fail-closed）
        None => MaskConfig {
            enabled: true,
            masker: None,
        },
    }
}

/// 按设置构造（并缓存）脱敏器。
///
/// 缓存以整份设置为指纹：改开关或词表都会重建；没改则只做一次 Arc 克隆。
fn masker_for(settings: MaskSettings) -> Arc<RulesMasker> {
    static CACHE: std::sync::Mutex<Option<(MaskSettings, Arc<RulesMasker>)>> =
        std::sync::Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached, masker)) = cache.as_ref() {
        if *cached == settings {
            return masker.clone();
        }
    }
    let masker = Arc::new(RulesMasker::with_rules(
        settings.secrets,
        settings.pii,
        &settings.words,
    ));
    *cache = Some((settings, masker.clone()));
    masker
}

/// 测试专用覆盖（仅 `#[cfg(test)]` 构建可见）。
#[cfg(test)]
pub(crate) mod testing {
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    use super::{MaskConfig, RulesMasker};

    thread_local! {
        static OVERRIDE: RefCell<Option<MaskConfig>> = const { RefCell::new(None) };
        /// 「配置读不到」的覆盖：与 `OVERRIDE` 分开，因为它表达的是
        /// `mask_settings_checked()` 返回 `None`，而不是某一组设置。
        static UNREADABLE: Cell<bool> = const { Cell::new(false) };
    }

    /// 覆盖值的生命周期守卫：离开作用域即恢复默认，避免测试之间互相污染。
    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            OVERRIDE.with(|slot| {
                *slot.borrow_mut() = None;
            });
        }
    }

    /// 当前线程的覆盖值（`None` = 没覆盖，走生产默认）。
    pub(super) fn snapshot() -> Option<MaskConfig> {
        OVERRIDE.with(|slot| slot.borrow().clone())
    }

    /// 当前线程是否在模拟「配置读不到」。
    pub(super) fn unreadable_snapshot() -> bool {
        UNREADABLE.with(|flag| flag.get())
    }

    /// 模拟配置读不到（未初始化 / 读锁中毒）：`active()` 必须按开启处理，
    /// 且不带脱敏器，于是 `mask_outbound` fail-closed。
    pub(crate) fn unreadable() -> UnreadableGuard {
        UNREADABLE.with(|flag| flag.set(true));
        UnreadableGuard
    }

    /// 「配置读不到」覆盖的生命周期守卫。
    pub(crate) struct UnreadableGuard;

    impl Drop for UnreadableGuard {
        fn drop(&mut self) {
            UNREADABLE.with(|flag| flag.set(false));
        }
    }

    /// 关闭脱敏（显式覆盖，便于在「生产默认也是关闭」时仍然锁定语义）。
    pub(crate) fn disabled() -> Guard {
        set(MaskConfig {
            enabled: false,
            masker: None,
        })
    }

    /// 开启脱敏但没有可用脱敏器（fail-closed 用例）。
    pub(crate) fn enabled_without_masker() -> Guard {
        set(MaskConfig {
            enabled: true,
            masker: None,
        })
    }

    /// 开启脱敏并指定脱敏器。
    pub(crate) fn enabled_with(masker: Arc<RulesMasker>) -> Guard {
        set(MaskConfig {
            enabled: true,
            masker: Some(masker),
        })
    }

    fn set(config: MaskConfig) -> Guard {
        OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(config);
        });
        Guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配置读不到时门禁必须按开启处理，且不给出脱敏器 —— 两者合起来才是
    /// fail-closed：`mask_outbound` 会因此返回 503，而不是静默放行明文。
    #[test]
    fn active_fails_closed_when_the_config_snapshot_is_unreadable() {
        let _guard = testing::unreadable();
        let config = active();
        assert!(config.enabled, "读不到配置时必须按开启处理");
        assert!(config.masker().is_none(), "读不到配置时不能给出脱敏器");
    }
}

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

/// 脱敏开关的快照。
#[derive(Clone, Debug, Default)]
pub struct MaskConfig {
    pub enabled: bool,
}

/// 当前的脱敏开关。
///
/// `enabled = maskSecrets || maskPii || !maskWords.is_empty()`：词表非空本身
/// 也算开启，否则用户填了词却发现没生效。
pub fn active() -> MaskConfig {
    #[cfg(test)]
    {
        if let Some((config, _)) = testing::snapshot() {
            return config;
        }
    }
    MaskConfig {
        enabled: crate::server::config::mask_settings().enabled(),
    }
}

/// 当前的脱敏器（`None` = 配置要求脱敏但读不到配置）。
///
/// 按设置选择规则族（密钥 / 邮箱 / 自定义词，见
/// [`RulesMasker::with_rules`]）。同一份设置的脱敏器会被缓存：规则编译
/// （尤其自定义词的正则）不该落在每个请求的热路径上；设置一改，指纹不匹配
/// 就重建。
pub fn masker() -> Option<Arc<RulesMasker>> {
    #[cfg(test)]
    {
        if let Some((_, masker)) = testing::snapshot() {
            return masker;
        }
    }
    let settings = crate::server::config::mask_settings_checked()?;
    static CACHE: std::sync::Mutex<Option<(MaskSettings, Arc<RulesMasker>)>> =
        std::sync::Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached, masker)) = cache.as_ref() {
        if *cached == settings {
            return Some(masker.clone());
        }
    }
    let masker = Arc::new(RulesMasker::with_rules(
        settings.secrets,
        settings.pii,
        &settings.words,
    ));
    *cache = Some((settings, masker.clone()));
    Some(masker)
}

/// 测试专用覆盖（仅 `#[cfg(test)]` 构建可见）。
#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;
    use std::sync::Arc;

    use super::{MaskConfig, RulesMasker};

    type Override = (MaskConfig, Option<Arc<RulesMasker>>);

    thread_local! {
        static OVERRIDE: RefCell<Option<Override>> = const { RefCell::new(None) };
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
    pub(super) fn snapshot() -> Option<Override> {
        OVERRIDE.with(|slot| slot.borrow().clone())
    }

    /// 关闭脱敏（显式覆盖，便于在「生产默认也是关闭」时仍然锁定语义）。
    pub(crate) fn disabled() -> Guard {
        set(MaskConfig { enabled: false }, None)
    }

    /// 开启脱敏但没有可用脱敏器（fail-closed 用例）。
    pub(crate) fn enabled_without_masker() -> Guard {
        set(MaskConfig { enabled: true }, None)
    }

    /// 开启脱敏并指定脱敏器。
    pub(crate) fn enabled_with(masker: Arc<RulesMasker>) -> Guard {
        set(MaskConfig { enabled: true }, Some(masker))
    }

    fn set(config: MaskConfig, masker: Option<Arc<RulesMasker>>) -> Guard {
        OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some((config, masker));
        });
        Guard
    }
}

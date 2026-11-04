//! 脱敏配置的**读侧接缝**（seam）。
//!
//! 出站脱敏要不要做、用哪份规则，由这里回答；调用方（[`super::mask_outbound`]）
//! 只认 [`MaskConfig`] 与 [`masker`] 两个入口。
//!
//! ## 现在与将来
//!
//! 本次（Task 5.2）只落**出站门禁**：`enabled` 恒为 `false`，[`masker`] 恒给
//! 一份默认规则的 [`RulesMasker`] —— 于是生产行为与改造前逐字节一致（关闭时
//! 连 JSON 都不解析）。
//!
//! Task 5.4 会把这两个函数换成读真实设置（`maskSecrets` / `maskPii` /
//! `maskWords`）：把开关与词表接进来即可，**调用方不需要改**。
//! 因此这里刻意保持「无参数、进程级」的形态：设置是全局的，调用点不该各自
//! 决定读哪一份。
//!
//! ## 测试接缝
//!
//! 配置是进程级的，但测试需要「开启 + 无脱敏器」这种组合。`#[cfg(test)]`
//! 下用线程局部覆盖（[`testing`]）：每个测试线程有自己的覆盖值，互不串味；
//! 覆盖只在测试构建里存在，生产代码看不到这条路径。

use std::sync::Arc;

use super::RulesMasker;

/// 脱敏开关的快照。
///
/// 目前只有「开 / 关」一位；Task 5.4 会按设置拆成密钥 / 个人信息 / 自定义词
/// 三档（届时新增字段，[`active`] 负责合并成最终值）。
#[derive(Clone, Debug, Default)]
pub struct MaskConfig {
    pub enabled: bool,
}

/// 当前的脱敏开关。
///
/// Task 5.4 之前恒为关闭 —— 关闭是**安全默认**：没有配置就没有理由改写用户
/// 请求体（改写会改变字节，进而影响上游缓存与排障对照）。
pub fn active() -> MaskConfig {
    #[cfg(test)]
    {
        if let Some((config, _)) = testing::snapshot() {
            return config;
        }
    }
    MaskConfig { enabled: false }
}

/// 当前的脱敏器（`None` = 配置要求脱敏但拿不到规则）。
///
/// 现在恒给默认规则；Task 5.4 会按设置构造（自定义词表、是否含 PII 等），
/// 并在规则编译失败等异常情况下返回 `None` —— 调用方据此 fail-closed。
pub fn masker() -> Option<Arc<RulesMasker>> {
    #[cfg(test)]
    {
        if let Some((_, masker)) = testing::snapshot() {
            return masker;
        }
    }
    static MASKER: std::sync::OnceLock<Arc<RulesMasker>> = std::sync::OnceLock::new();
    Some(
        MASKER
            .get_or_init(|| Arc::new(RulesMasker::default()))
            .clone(),
    )
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

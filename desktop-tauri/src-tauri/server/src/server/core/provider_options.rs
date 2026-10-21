//! 每 provider 的行为开关（对标 magpie 的 `auth.options[]`）。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 全局开关（`sanitizeBlacklistFingerprints` / `promptMode` 等）对**所有家**
//! 同时生效。但不同家对同一策略的反应并不一致：WorkBuddy 的审核按逐字匹配、
//! 脱敏必须开；某家本来不做内容审核，脱敏只是白改字节。
//!
//! ── 存储形态 ──────────────────────────────────────────────
//! 配置里的 `providerOptions` 键：
//! ```json
//! "providerOptions": {
//!   "workbuddy": { "sanitizeBlacklistFingerprints": false }
//! }
//! ```
//! **键不存在 = 跟随全局默认**（`is_enabled` 的 `default` 参数）—— 这是
//! 升级兼容的硬要求：旧配置读出来必须与改造前逐字一致。
//!
//! ── 写侧为什么用 `update_raw_field` 而不是 `save_raw` ─────────────
//! `save_raw` 只写库、**不刷新内存快照**，而读侧 `with_raw` 读的是内存快照。
//! 用 `save_raw` 会导致「设了不生效、必须重启」。`update_raw_field` 走
//! `update()` 闭包，写完同步刷新内存。
//!
//! ── 键名必须与全局键逐字一致 ────────────────────────────────
//! [`KNOWN_KEYS`] 里的名字直接引用 `config::types` 的全局键常量，不是自造
//! 短名。拼错会让「per-provider 覆盖」永远匹配不到全局开关。
//!
//! ── 未初始化时的读侧 ──────────────────────────────────────
//! `config::with_raw` 在配置快照还没装进内存时返回 `None`（启动极早期，
//! 与 `current()` 回落空配置不同）。本模块把它当作「没设置过」→ 调用方
//! 走 `default`，于是那个窗口里的行为与改造前逐字一致。

use serde_json::{Map, Value};

use crate::server::config;
use crate::server::logging;

/// 允许的开关名 —— **直接引用全局键常量**，保证与全局开关同名。
pub const KNOWN_KEYS: [&str; 2] = [
    crate::server::config::KEY_SANITIZE_FINGERPRINTS,
    crate::server::config::KEY_STRIP_NEWLINE_KEEPALIVE,
];

fn config_key() -> &'static str {
    "providerOptions"
}

/// provider 是否可识别（内置注册表或自定义家）。
fn is_known_provider(provider_id: &str) -> bool {
    crate::server::core::providers::PROVIDERS
        .iter()
        .any(|meta| meta.id == provider_id)
        || crate::server::core::custom_providers::is_custom_provider_id(provider_id)
}

/// 读一个 provider 的某个开关。`None` = 没设置过（调用方用 default）。
pub fn get(provider_id: &str, key: &str) -> Option<bool> {
    if !KNOWN_KEYS.contains(&key) || !is_known_provider(provider_id) {
        return None;
    }
    config::with_raw(|raw| {
        raw.get(config_key())
            .and_then(|options| options.get(provider_id))
            .and_then(|options| options.get(key))
            .and_then(Value::as_bool)
    })
    .flatten()
}

/// 读开关，未设置时用 `default`。
pub fn is_enabled(provider_id: &str, key: &str, default: bool) -> bool {
    get(provider_id, key).unwrap_or(default)
}

/// 写一个 provider 的开关（写完内存快照同步刷新）。
///
/// 返回值只反映**落库**成不成功；内存快照无论成败都已更新（`update_raw_field`
/// 走 `update()`，它先改内存再落库 —— 见 `config::save_raw` 的「为什么成功与否
/// 都要让内存快照生效」）。落库失败只打日志、不报错，与 `api::sanitize` 的
/// `set_sanitize_fingerprints` 调用点同一取向：那是「重启后还在不在」的问题，
/// 不该让一次设置请求失败 —— 否则面板上刚点的那一下会变成错误，而本次运行
/// 内行为其实已经改了。
pub fn set(provider_id: &str, key: &str, value: bool) -> Result<(), String> {
    if !KNOWN_KEYS.contains(&key) {
        return Err(format!(
            "未知的 provider 开关 `{key}`（允许: {}）",
            KNOWN_KEYS.join(" / ")
        ));
    }
    if provider_id.trim().is_empty() {
        return Err("provider id 不能为空".to_string());
    }
    let mut options = config::with_raw(|raw| raw.get(config_key()).cloned())
        .flatten()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(options_map) = options.as_object_mut() else {
        return Err("providerOptions 不是对象".to_string());
    };
    let entry = options_map
        .entry(provider_id.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(entry) = entry.as_object_mut() else {
        return Err(format!("providerOptions.{provider_id} 不是对象"));
    };
    entry.insert(key.to_string(), Value::Bool(value));
    let saved = config::update_raw_field(config_key(), options);
    // 落库失败也照样返回 Ok：内存快照已经改了（见函数头），本次运行内生效。
    // 文案里带上成败 —— 两句分开打（一句失败一句成功）排查时分不清哪半是真的。
    logging::log(
        "[Config]",
        &format!(
            "provider 开关 {provider_id}.{key} = {value}{}",
            if saved {
                ""
            } else {
                "（落库失败，本次运行内仍生效）"
            }
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// 这四条都碰**进程级**配置快照（`config::CONFIG`），而 `config::update`
    /// 是「读整份 → 改 → 写整份」的非原子读改写：两条用例并行时，一条的写
    /// 可能被另一条「读走的旧底稿」整体盖掉。所以本模块动全局的用例串行跑
    /// （与 `config::auto_continue_tests` 的 `CONFIG_LOCK` 同一手法）。
    static CONFIG_LOCK: Mutex<()> = Mutex::new(());

    fn hold_config() -> MutexGuard<'static, ()> {
        CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 用例结束后把 `providerOptions` 恢复原样，别把「workbuddy 关着」留给
    /// 后面的用例（尤其留给任何读 `config::current()` 的断言）。
    ///
    /// 恢复写在 `Drop` 上：用例中途 `panic` 也会执行（测试跑的是 unwind
    /// profile，`panic=abort` 只作用于生产构建）。
    struct RestoreOptions {
        previous: Option<Value>,
    }

    impl RestoreOptions {
        fn capture() -> Self {
            Self {
                previous: config::with_raw(|raw| raw.get(config_key()).cloned()).flatten(),
            }
        }
    }

    impl Drop for RestoreOptions {
        fn drop(&mut self) {
            // 原先没有这个键 → 恢复成空对象（读侧语义与「没有」逐字相同：
            // 查不到任何 provider 条目，一律走 default）
            let value = self
                .previous
                .clone()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let _ = config::update_raw_field(config_key(), value);
        }
    }

    #[test]
    fn set_then_get_returns_new_value() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        // 回归锁定：写后同进程读必须立刻生效。
        // 反例（v1）：用 config::save_raw 只写库、不刷新内存快照，
        // 于是 get 读到旧值，必须重启才生效。
        set("workbuddy", "sanitizeBlacklistFingerprints", false).expect("写入应当成功");
        assert!(
            !is_enabled("workbuddy", "sanitizeBlacklistFingerprints", true),
            "写后必须立刻读到新值"
        );
    }

    #[test]
    fn per_provider_option_is_independent() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        set("workbuddy", "sanitizeBlacklistFingerprints", false).unwrap();
        set("qoder", "sanitizeBlacklistFingerprints", true).unwrap();
        assert!(!is_enabled("workbuddy", "sanitizeBlacklistFingerprints", true));
        assert!(is_enabled("qoder", "sanitizeBlacklistFingerprints", true));
        // 未设置的 provider 走 default
        assert!(is_enabled("trae", "sanitizeBlacklistFingerprints", true));
    }

    #[test]
    fn unknown_provider_option_is_ignored() {
        // 已删的自定义家在配置里还留着开关：读的时候忽略，不 panic。
        assert!(
            is_enabled("custom-gone", "sanitizeBlacklistFingerprints", true),
            "未知 provider 必须走 default"
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(set("workbuddy", "nonexistentKey", true).is_err());
    }
}

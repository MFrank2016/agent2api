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
///
/// ── 哪些家真读哪些键（读侧能力矩阵）──────────────────────
/// 本表是内置家与自定义家**共用**的一张白名单：写侧只校验「这个名字认识」，
/// 不区分家。但**读取点**分两条路径，能力不同：
///
/// | 键 | 内置家（`PROVIDERS`） | 自定义家（`custom-*`） |
/// | --- | --- | --- |
/// | `sanitizeBlacklistFingerprints` | ✓ 读（`upstream::payload`） | ✓ 读（同一条 send_body） |
/// | `stripLeadingThink` | ✓ 读（`provider_loop::frame_policy_of`） | ✓ 读（`custom::forward::frame_policy_of_custom`） |
/// | `stripNewlineKeepalive` | ✓ 读（同 `frame_policy_of`，与能力位相与） | ✗ **不读**（见下） |
///
/// 自定义家不读 `stripNewlineKeepalive` 是**有意的保守取舍**：丢帧的依据只来自
/// WorkBuddy / AutoClaw 两家的实测，不能替用户自配的上游猜（`FramePolicy::
/// with_rewrite_and_think` 恒把 `strip_newline_chunks` 置 false）。所以给自定义家
/// 配这个键会**存着但不生效** —— 本表仍允许写入（拒绝反而会让面板在两个家之间
/// 表现不一致），限制写在这里而不是静默。
///
/// 内置家里真正声明了「丢帧」能力位的只有 WorkBuddy / AutoClaw（`sse_strip_
/// newline_chunks`）：对其它内置家配这个键同样不生效，但**不是**本表能拦的
/// 事（那是能力位，不是开关）。
pub const KNOWN_KEYS: [&str; 3] = [
    crate::server::config::KEY_SANITIZE_FINGERPRINTS,
    crate::server::config::KEY_STRIP_NEWLINE_KEEPALIVE,
    crate::server::config::KEY_STRIP_LEADING_THINK,
];

fn config_key() -> &'static str {
    "providerOptions"
}

/// 测试用的进程级配置锁：**所有写 `providerOptions` 的用例共用这一把**
/// （本模块的 `tests` 与 `providers::custom::forward` 的用例）。
///
/// 为什么不能各用各的：`config::update` 是「读整份 → 改 → 写整份」的**非原子**
/// 读改写，两把互不相干的锁各自以为「串行了」，实际仍在互相盖掉对方的写入 ——
/// 症状是「换个跑法就有一条用例读到自己没写过的值」。锁放在模块层而不是
/// `tests` 里，是因为兄弟模块的用例要用同一个 static。
#[cfg(test)]
pub(crate) static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
///
/// ── 两道闸（都是「宁可选错，不要写坏」）──────────────────
///   - **配置快照未装入时拒绝写入**：`with_raw` 的返回值是 `Option<Option<T>>`，
///     **外层** `None` 才说明 `config::init` 还没跑（内层 `None` 只是「还没有
///     `providerOptions` 这个键」，全新安装的正常形态，必须能写）。外层为
///     `None` 时若照旧往下走，`update()` 里的 `current()` 会回落成
///     `build(Map::new())`（空底稿），随后 `save_raw` 把「空底稿里没有的键」
///     **整份删掉** —— 丢的不是这一家的开关，是用户所有配置。当前不可达
///     （`init` 是 `bootstrap` 的第一件事，`set` 也还没有调用方），但守一道闸
///     比事后排查便宜。
///   - **provider id 必须是认得的家**：拼错的 id 会写进一条永远读不到的死记录
///     （读侧 `is_known_provider` 会把它当残留忽略），而 `Ok` 让调用方以为写成了。
///     认不出的 id 在写侧就拒绝，比事后在配置里捞死记录便宜。
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
    if !is_known_provider(provider_id) {
        return Err(format!("未知的 provider `{provider_id}`"));
    }
    // 写侧必须分清 `with_raw` 的两层 `None`（`Option<Option<Value>>`）：
    //   - 外层 `None` = 配置快照还没装（`config::init` 未跑）→ 拒绝写入。
    //     此时往下走的话 `update()` 里的 `current()` 会回落成
    //     `build(Map::new())`（空底稿），随后 `save_raw` 把「空底稿里没有的键」
    //     **整份删掉** —— 丢的是用户所有配置，不只是这一家的开关。
    //   - 内层 `None` = 快照在、但还没有 `providerOptions` 这个键（全新安装的
    //     正常形态）→ 从空对象开始，这是必须能走通的路。
    // 用 `flatten()` 会把两层压成一层，于是「首次写入」也会被当成未初始化
    // 拒绝掉 —— 那正是本条闸最容易写歪的地方。
    let Some(mut options) = config::with_raw(|raw| raw.get(config_key()).cloned()) else {
        return Err("配置尚未初始化，无法写入 provider 开关".to_string());
    };
    let mut options = options.unwrap_or_else(|| Value::Object(Map::new()));
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
    use crate::server::config::KEY_SANITIZE_FINGERPRINTS;
    use std::sync::MutexGuard;

    /// 这四条都碰**进程级**配置快照（`config::CONFIG`），而 `config::update`
    /// 是「读整份 → 改 → 写整份」的非原子读改写：两条用例并行时，一条的写
    /// 可能被另一条「读走的旧底稿」整体盖掉。所以本模块动全局的用例串行跑
    /// （与 `config::auto_continue_tests` 的 `CONFIG_LOCK` 同一手法）。
    /// 锁本身在模块层（[`super::CONFIG_LOCK`]）：写同一个键的兄弟模块要共用它。
    fn hold_config() -> MutexGuard<'static, ()> {
        CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 单元测试进程从不调 `config::init`，所以快照可能是空的（`with_raw` 返回
    /// `None`）。`set()` 现在把「未初始化」当硬错误（写进去会丢整份配置，见
    /// 那里的两道闸），于是用例必须先让快照存在。
    ///
    /// 只在**真的没有**快照时补一个空的 `providerOptions`：已有快照时什么都不
    /// 做 —— 否则会盖掉别的模块（如 `http::cors_tests`）刚写进去的键，那正是
    /// 这个仓库已知的全局状态污染。
    fn ensure_snapshot() {
        if config::with_raw(|_| ()).is_some() {
            return;
        }
        let _ = config::update_raw_field(config_key(), Value::Object(Map::new()));
    }

    /// 绕开 `set()` 的 provider 校验，直接往配置里落一条开关 —— 模拟
    /// 「自定义家已经删了、配置里还留着它那条」。
    ///
    /// 不在这里断言落库成功：单元测试进程没有库，`update_raw_field` 只报「写盘
    /// 成不成功」，内存快照无论如何都已刷新（见 `config::save_raw` 的说明）。
    /// 落没落进去由调用方用 [`raw_option`] 回读验证。
    fn write_raw_option(provider_id: &str, key: &str, value: bool) {
        let mut options = config::with_raw(|raw| raw.get(config_key()).cloned())
            .flatten()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let Some(map) = options.as_object_mut() else {
            panic!("providerOptions 必须是对象");
        };
        let entry = map
            .entry(provider_id.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(entry) = entry.as_object_mut() else {
            panic!("providerOptions.{provider_id} 必须是对象");
        };
        entry.insert(key.to_string(), Value::Bool(value));
        let _ = config::update_raw_field(config_key(), options);
    }

    /// 直接从配置快照里取值，**不带** provider / key 的合法性校验 ——
    /// 用来证明「残留确实写进去了」，也是 [`get`] 那道校验的对照组。
    fn raw_option(provider_id: &str, key: &str) -> Option<bool> {
        config::with_raw(|raw| {
            raw.get(config_key())
                .and_then(|options| options.get(provider_id))
                .and_then(|options| options.get(key))
                .and_then(Value::as_bool)
        })
        .flatten()
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
        ensure_snapshot();
        // 回归锁定：写后同进程读必须立刻生效。
        // 反例（v1）：用 config::save_raw 只写库、不刷新内存快照，
        // 于是 get 读到旧值，必须重启才生效。
        set("workbuddy", KEY_SANITIZE_FINGERPRINTS, false).expect("写入应当成功");
        assert_eq!(
            get("workbuddy", KEY_SANITIZE_FINGERPRINTS),
            Some(false),
            "写后必须立刻读到新值"
        );
        assert!(
            !is_enabled("workbuddy", KEY_SANITIZE_FINGERPRINTS, true),
            "写后必须立刻读到新值"
        );
    }

    #[test]
    fn per_provider_option_is_independent() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        ensure_snapshot();
        set("workbuddy", KEY_SANITIZE_FINGERPRINTS, false).unwrap();
        set("qoder", KEY_SANITIZE_FINGERPRINTS, true).unwrap();
        // 两边都断言 `Some(..)`：只断 `is_enabled` 的话，`true` 与「没写进去、
        // 走 default(true)」分不开 —— 那正是本条要排除的空转形态。
        assert_eq!(get("workbuddy", KEY_SANITIZE_FINGERPRINTS), Some(false));
        assert_eq!(get("qoder", KEY_SANITIZE_FINGERPRINTS), Some(true));
        // 未设置的 provider 走 default
        assert_eq!(get("trae", KEY_SANITIZE_FINGERPRINTS), None);
        assert!(
            is_enabled("trae", KEY_SANITIZE_FINGERPRINTS, true),
            "未设置的 provider 必须走 default"
        );
        assert!(
            !is_enabled("trae", KEY_SANITIZE_FINGERPRINTS, false),
            "default 必须被真正采用，而不是被别的值盖住"
        );
    }

    #[test]
    fn unknown_provider_option_is_ignored() {
        // 回归锁定：已删的自定义家在配置里**真的留下**一条开关时，读侧必须忽略它。
        // 只断言 default 而不写入，等于空转 —— 把 is_known_provider 删掉也照样绿。
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        ensure_snapshot();
        // 绕过 is_known_provider 直接落一条残留（模拟「家删了、配置还在」）
        write_raw_option("custom-gone", KEY_SANITIZE_FINGERPRINTS, false);
        // 先证明残留**确实**在快照里：否则下面两条断言都是空转
        assert_eq!(
            raw_option("custom-gone", KEY_SANITIZE_FINGERPRINTS),
            Some(false),
            "前提：残留值必须真的写进了配置快照"
        );
        assert_eq!(
            get("custom-gone", KEY_SANITIZE_FINGERPRINTS),
            None,
            "未知 provider 的残留值必须读不出来"
        );
        assert!(
            is_enabled("custom-gone", KEY_SANITIZE_FINGERPRINTS, true),
            "未知 provider 必须走 default，不继承残留值"
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(set("workbuddy", "nonexistentKey", true).is_err());
    }

    /// `stripLeadingThink` 必须是**登记过的** per-provider 键：没登记的话，
    /// `provider_loop::frame_policy_of` 传进来的这个键永远读不到覆盖值，
    /// 逐家开关会静默失效（写侧也会被拒）。
    #[test]
    fn the_think_tag_key_is_a_known_per_provider_key() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        ensure_snapshot();
        let key = crate::server::config::KEY_STRIP_LEADING_THINK;
        set("qoder", key, true).expect("登记过的键必须能写");
        assert_eq!(get("qoder", key), Some(true), "写后必须立刻读到");
        // 未设置的 provider 跟随调用方给的 default（默认态 false）
        assert_eq!(get("workbuddy", key), None);
        assert!(!is_enabled("workbuddy", key, false));
        assert!(is_enabled("workbuddy", key, true), "default 必须被真正采用");
    }

    /// 拼错的 provider id 不能静默写成一条死记录：读侧会把它当残留忽略，
    /// 而 `Ok` 会让调用方以为设置生效了。
    #[test]
    fn unknown_provider_is_rejected_on_write() {
        let _guard = hold_config();
        let _restore = RestoreOptions::capture();
        ensure_snapshot();
        assert!(
            set("custom-gone", KEY_SANITIZE_FINGERPRINTS, false).is_err(),
            "未知 provider 必须被写侧拒绝"
        );
        // 拒绝必须是「一个字节都不写」，而不是先写后报错
        assert_eq!(
            raw_option("custom-gone", KEY_SANITIZE_FINGERPRINTS),
            None,
            "被拒绝的写入不许在配置里留下任何痕迹"
        );
    }
}

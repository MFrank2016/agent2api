//! 停用路线的稳定身份；保留存量账号，不提供执行体或添加入口。
//!
//! `kiro` 于 2026-11 移出停用表但**保留在 scoped runtime 表**里：插件仓已
//! 提供实现，网关侧只需放行；它仍使用独立 FULL-auth 边界与 strict 错误投影
//! （见 `plugin_host::strict_errors`），否则上游错误正文可能把凭证带出来。

pub const BUILTIN_SUBSCRIPTION_IDS: [&str; 5] = [
    "claude-subscription",
    "codex-subscription",
    "chatgpt-subscription",
    "copilot-subscription",
    "gemini-subscription",
];

/// 使用独立 FULL-auth 边界的稳定身份（**不随停用状态变化**）。
const SCOPED_RUNTIME_IDS: [&str; 13] = [
    "cursor",
    "kiro",
    "devin",
    "factory",
    "zed",
    "grok",
    "minimax-code",
    "minimax-code-global",
    "qwenwork",
    "qwenwork-enterprise",
    "kimi-work",
    "opendesign",
    "nous-portal",
];

/// 停用路线：不提供执行体或添加入口（`kiro` 已移出）。
const SUSPENDED_RUNTIME_IDS: [&str; 12] = [
    "cursor",
    "devin",
    "factory",
    "zed",
    "grok",
    "minimax-code",
    "minimax-code-global",
    "qwenwork",
    "qwenwork-enterprise",
    "kimi-work",
    "opendesign",
    "nous-portal",
];

pub(crate) fn is_runtime_identity(id: &str) -> bool {
    BUILTIN_SUBSCRIPTION_IDS.contains(&id) || SCOPED_RUNTIME_IDS.contains(&id)
}

pub fn is_suspended(id: &str) -> bool {
    BUILTIN_SUBSCRIPTION_IDS.contains(&id) || SUSPENDED_RUNTIME_IDS.contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiro_is_no_longer_suspended() {
        assert!(!is_suspended("kiro"), "kiro 已实现插件，必须可添加");
    }

    #[test]
    fn other_suspended_ids_are_unchanged() {
        for id in [
            "cursor",
            "devin",
            "factory",
            "zed",
            "grok",
            "opendesign",
            "nous-portal",
        ] {
            assert!(is_suspended(id), "{id} 仍应保持停用");
        }
    }

    #[test]
    fn kiro_stays_a_scoped_runtime_after_unsuspend() {
        assert!(
            is_runtime_identity("kiro"),
            "kiro 必须保留 FULL-auth 边界与 strict 错误投影"
        );
        assert!(!is_suspended("kiro"), "kiro 已可添加");
    }
}

//! 停用路线的稳定身份；保留存量账号，不提供执行体或添加入口。
//!
//! `kiro` 于 2026-11 移出本表：插件仓已提供实现（`providers/kiro/`），
//! 网关侧只需放行，不引入任何内置转发代码。

pub const BUILTIN_SUBSCRIPTION_IDS: [&str; 5] = [
    "claude-subscription",
    "codex-subscription",
    "chatgpt-subscription",
    "copilot-subscription",
    "gemini-subscription",
];

const PLUGIN_RUNTIME_IDS: [&str; 12] = [
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
    BUILTIN_SUBSCRIPTION_IDS.contains(&id) || PLUGIN_RUNTIME_IDS.contains(&id)
}

pub fn is_suspended(id: &str) -> bool {
    is_runtime_identity(id)
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
}

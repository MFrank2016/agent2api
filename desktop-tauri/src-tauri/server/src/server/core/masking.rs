//! 出站脱敏（masking）：把请求里的密钥 / 邮箱 / 自定义词替换成占位符，回复再还原。
//!
//! 与 `sanitize` 的分工：`sanitize` 是**出站指纹脱敏**（改写发送副本，回复不还原，
//! 用于规避上游对固定文案的识别）；本模块是**可逆脱敏**（请求侧换成占位符，
//! 回复侧按同一份映射还原），面向「不希望明文离开网关」的场景。
//!
//! ## 会话（session）设计
//!
//! 映射（占位符 → 原文）必须按请求隔离：同一进程里同时跑多个请求时，
//! 把映射挂在进程级 `RulesMasker` 上会互相串（A 请求的回复被 B 请求的映射还原）。
//! 因此生产路径一律走 [`RulesMasker::session`]：每次请求拿一个 [`MaskSession`]，
//! 映射只活在这次请求里，请求结束随 session 一起释放。
//!
//! [`RulesMasker`] 自身也实现了 [`Masker`]（便于单测 / 单次调用），但它内部的
//! 映射表是**有上限**的（超限按插入顺序淘汰），且不应在请求路径上共享使用。
//!
//! ## 占位符
//!
//! 形如 `[[A2A_MASK_<kind>_<n>]]`，`n` 进程内全局递增（跨会话也不重复），
//! 因此占位符不会与普通文本或其他会话的占位符混淆；还原时按前缀精确匹配，
//! 不认识的占位符原样保留。

mod rules;

pub use rules::{MaskSession, RulesMasker};

use serde_json::Value;

/// 占位符前缀。`restore` 只认这个前缀，避免误伤普通方括号文本。
pub const PLACEHOLDER_PREFIX: &str = "[[A2A_MASK_";

/// 一次命中的元数据：**只记类型与占位符，不记原文**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskHit {
    pub kind: &'static str,
    pub placeholder: String,
}

/// 一次 mask 的命中汇总（按占位符去重：同一个值只报一次）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaskReport {
    pub hits: Vec<MaskHit>,
}

impl MaskReport {
    /// 合并另一份报告，按占位符去重。
    pub fn absorb(&mut self, other: Self) {
        for hit in other.hits {
            if !self
                .hits
                .iter()
                .any(|seen| seen.placeholder == hit.placeholder)
            {
                self.hits.push(hit);
            }
        }
    }

    /// 记录一次命中（同一占位符只记一次）。
    fn note(&mut self, kind: &'static str, placeholder: &str) {
        if self.hits.iter().any(|seen| seen.placeholder == placeholder) {
            return;
        }
        self.hits.push(MaskHit {
            kind,
            placeholder: placeholder.to_string(),
        });
    }
}

/// 可逆脱敏器。`restore` 依赖 mask 阶段建立的映射，因此同一份映射必须
/// 贯穿「请求 mask → 回复 restore」的全过程（见模块头的 session 设计）。
pub trait Masker {
    /// 递归脱敏 JSON 的所有字符串值（对象键不动，tool call 的 arguments 字符串也会被脱敏）。
    fn mask_json(&self, value: &mut Value) -> MaskReport;
    /// 脱敏一段文本，返回新文本与命中元数据。
    fn mask_text(&self, text: &str) -> (String, MaskReport);
    /// 按本脱敏器的映射还原占位符；不认识的占位符原样保留。
    fn restore(&self, text: &str) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 基础能力：API Key 与邮箱都要被替换，且能原样还原。
    #[test]
    fn masks_api_key_and_email() {
        let masker = RulesMasker::default();
        let (text, report) = masker.mask_text("key sk-abc123 email a@b.com");
        assert!(!text.contains("sk-abc123"));
        assert!(!text.contains("a@b.com"));
        assert_eq!(report.hits.len(), 2);
        assert_eq!(masker.restore(&text), "key sk-abc123 email a@b.com");
    }

    /// 嵌套结构里的字符串值、以及 tool call 的 `arguments` JSON 字符串都要脱敏；
    /// 对象键不能被动。
    #[test]
    fn mask_json_masks_nested_strings_and_tool_arguments() {
        let masker = RulesMasker::default();
        let mut body = json!({
            "model": "example-model",
            "messages": [
                { "role": "user", "content": "mail me at a@b.com" },
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "type": "function",
                        "function": {
                            "name": "run",
                            "arguments": "{\"token\":\"sk-abcdefgh\",\"note\":\"a@b.com\"}"
                        }
                    }]
                }
            ]
        });

        let report = masker.mask_json(&mut body);
        let serialized = body.to_string();
        assert!(!serialized.contains("a@b.com"), "{serialized}");
        assert!(!serialized.contains("sk-abcdefgh"), "{serialized}");
        assert!(serialized.contains(PLACEHOLDER_PREFIX), "{serialized}");
        // 对象键与普通字符串保持原样。
        assert_eq!(body["model"], "example-model");
        assert!(body["messages"][0].get("content").is_some());
        assert_eq!(report.hits.len(), 2);

        // 还原后回到原文。
        let restored = masker.restore(&serialized);
        assert!(restored.contains("a@b.com"), "{restored}");
        assert!(restored.contains("sk-abcdefgh"), "{restored}");
    }

    /// 同一个值重复出现时复用同一占位符，还原时全部还原。
    #[test]
    fn restore_roundtrips_repeated_values() {
        let masker = RulesMasker::default();
        let (text, report) = masker.mask_text("a@b.com and a@b.com again");
        let placeholder = report.hits[0].placeholder.clone();
        assert_eq!(report.hits.len(), 1);
        assert_eq!(text.matches(&placeholder).count(), 2);
        assert_eq!(masker.restore(&text), "a@b.com and a@b.com again");
    }

    /// 报告里不能出现原文（`MaskHit` 只有类型与占位符）。
    #[test]
    fn no_original_text_in_report() {
        let masker = RulesMasker::default();
        let (_text, report) = masker.mask_text("sk-abcdefgh a@b.com");
        let debug = format!("{report:?}");
        assert!(!debug.contains("sk-abcdefgh"), "{debug}");
        assert!(!debug.contains("a@b.com"), "{debug}");
        for hit in &report.hits {
            assert!(!hit.placeholder.contains("sk-abcdefgh"));
            assert!(!hit.placeholder.contains("a@b.com"));
        }
    }

    /// 两个会话的映射互不可见：A 会话的占位符在 B 会话里还原不出 A 的原文。
    #[test]
    fn session_does_not_leak_mappings() {
        let masker = RulesMasker::default();
        let first = masker.session();
        let second = masker.session();

        let (text_a, _) = first.mask_text("alice@example.com");
        let (text_b, _) = second.mask_text("bob@example.com");

        assert_eq!(first.restore(&text_a), "alice@example.com");
        assert_eq!(second.restore(&text_b), "bob@example.com");
        // B 不认识 A 的占位符：既不能还原成 alice，也不该被改动。
        assert_eq!(second.restore(&text_a), text_a);
        assert_ne!(second.restore(&text_a), "alice@example.com");
    }

    /// 自定义词表按字面匹配（正则元字符不生效）。
    #[test]
    fn masks_custom_words_literally() {
        let masker = RulesMasker::with_words(&["Project Falcon".to_string(), "a.b".to_string()]);
        let (text, report) = masker.mask_text("Project Falcon and a.b and axb");
        assert!(!text.contains("Project Falcon"), "{text}");
        assert!(!text.contains("a.b"), "{text}");
        assert!(text.contains("axb"), "{text}");
        assert_eq!(report.hits.len(), 2);
        assert_eq!(masker.restore(&text), "Project Falcon and a.b and axb");
    }

    /// GitHub / Slack 前缀令牌也要命中。
    #[test]
    fn masks_github_and_slack_tokens() {
        let masker = RulesMasker::default();
        let (text, report) =
            masker.mask_text("ghp_abcdefghij github_pat_abcdefghij xoxb-1234567890");
        assert!(!text.contains("ghp_abcdefghij"), "{text}");
        assert!(!text.contains("github_pat_abcdefghij"), "{text}");
        assert!(!text.contains("xoxb-1234567890"), "{text}");
        assert_eq!(report.hits.len(), 3);
    }

    /// 不认识的占位符原样保留，不能吞掉后面的文本。
    #[test]
    fn restore_leaves_unknown_placeholders_alone() {
        let masker = RulesMasker::default();
        let text = "before [[A2A_MASK_email_999999]] after [[OTHER]]";
        assert_eq!(masker.restore(text), text);
    }

    /// 同起点重叠时取更长的匹配（fail-closed）：整段邮箱被盖掉，不留域名尾巴。
    #[test]
    fn overlapping_matches_prefer_the_longer_span() {
        let masker = RulesMasker::default();
        let (text, report) = masker.mask_text("sk-abcdefgh@example.com");
        assert!(!text.contains("example.com"), "{text}");
        assert!(!text.contains("sk-abcdefgh"), "{text}");
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].kind, "email");
        assert_eq!(masker.restore(&text), "sk-abcdefgh@example.com");
    }

    /// 部分重叠的 span 不能丢掉尾巴：邮箱先盖住 `a@b.sk` 后，密钥剩下的
    /// `-abcdefgh` 必须单独盖掉，否则明文会漏出网关（fail-closed）。
    #[test]
    fn partially_overlapping_spans_mask_the_tail() {
        let masker = RulesMasker::default();
        let (text, report) = masker.mask_text("a@b.sk-abcdefgh");
        assert!(!text.contains("abcdefgh"), "{text}");
        assert!(!text.contains("sk-abcdefgh"), "{text}");
        assert!(report.hits.len() >= 2, "{report:?}");
        assert_eq!(masker.restore(&text), "a@b.sk-abcdefgh");
    }

    /// 超长自定义词跳过而不是 panic（配置来源不可信）。
    #[test]
    fn oversized_custom_words_are_skipped_not_fatal() {
        let long_word = "x".repeat(10_000);
        let masker = RulesMasker::with_words(&[long_word.clone()]);
        let (text, report) = masker.mask_text(&format!("a {long_word} b"));
        assert!(
            text.contains(&long_word),
            "oversized word should be left alone"
        );
        assert!(report.hits.is_empty(), "{report:?}");
    }

    /// 对象键即使长得像邮箱也不能被脱敏（只动值）。
    #[test]
    fn mask_json_leaves_object_keys_alone() {
        let masker = RulesMasker::default();
        let mut body = json!({ "a@b.com": "plain" });
        let report = masker.mask_json(&mut body);
        assert_eq!(body["a@b.com"], "plain");
        assert!(report.hits.is_empty(), "{report:?}");
    }
}

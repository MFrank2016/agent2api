//! 剥离响应**开头**的思考标签块（对标 magpie 的 `think-tags` 插件 `strip` 模式）。
//!
//! ── 与 Qoder 那台机器的区别（不要混淆）──────────────────────
//! `providers::qoder::stream` 的机器把**正文中间**的 `<think>` 转成 reasoning
//! delta（`THINK_TAGS` 四组标签、任意位置）。本模块只处理**开头**那一块：
//! 上游把思考内容整个塞在正文最前面、而客户端不认标签时，把它剥掉。
//! 两者语义不同，不要互相替代。
//!
//! ── 为什么要跨帧状态 ──────────────────────────────────────
//! SSE 帧边界与标签边界无关：`<thi` 与 `nk>` 可能落在两帧里。逐帧剥离会
//! 把半截标签当普通正文留下。所以用 [`LeadingThinkFilter`] 累积到标签闭合。
//!
//! ── 为什么「没有标签」要立刻放行 ────────────────────────────
//! 首帧若不是标签开头（也不是任何标签的**前缀**），就置 `done` 并原样返回
//! —— 不能为了等可能的标签而缓冲正文（那会给所有正常响应加延迟）。

use crate::server::core::providers::qoder::protocol::THINK_TAGS;

/// 剥离文本**开头**的 think 块。返回 (清理后文本, 是否命中)。
///
/// 「命中」= 确实剥掉了一个**闭合**的标签块，而不是「判定结束」。
/// 标签没闭合时按「无法判定」处理：原文原样返回（见 [`LeadingThinkFilter::finish`]）。
pub fn strip_leading(text: &str) -> (String, bool) {
    let mut filter = LeadingThinkFilter::default();
    let mut out = filter.push(text);
    out.push_str(&filter.finish());
    (out, filter.stripped())
}

/// 跨帧的开头标签过滤器。
#[derive(Debug, Default)]
pub struct LeadingThinkFilter {
    /// 已判定完毕（没有开头标签 / 标签已剥完）—— 之后原样透传
    done: bool,
    /// 判定期间的缓冲（只在开头可能是标签时非空）
    buffer: String,
    /// 是否真的剥掉了一个闭合的标签块（`done` 也可能因为「不是标签」而为真）
    stripped: bool,
}

impl LeadingThinkFilter {
    /// 是否已经判定完毕（之后 `push` 一律原样透传）。
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 是否真的剥掉了一个闭合的标签块。
    pub fn stripped(&self) -> bool {
        self.stripped
    }

    /// 喂一段正文，返回可以下发的部分。
    pub fn push(&mut self, chunk: &str) -> String {
        if self.done {
            return chunk.to_string();
        }
        self.buffer.push_str(chunk);
        // 先按去掉前导空白的形态判定；命中时连那段空白一起丢（它是标签前的
        // 分隔噪声），没命中则原样吐出（见模块头与测试）。
        let trimmed = self.buffer.trim_start();
        if trimmed.is_empty() {
            // 只有空白：还判不出来，等下一帧（正常响应第一帧不会只是空白）
            return String::new();
        }
        for (open, close) in THINK_TAGS {
            if let Some(rest) = trimmed.strip_prefix(open) {
                let Some(close_at) = rest.find(close) else {
                    // 标签还没闭合：继续缓冲（等下一帧）
                    return String::new();
                };
                self.stripped = true;
                self.done = true;
                let rest = rest[close_at + close.len()..].to_string();
                self.buffer.clear();
                return rest;
            }
            // 半截标签（`<thi` 是 `<think>` 的前缀）：必须继续缓冲，
            // 否则下一帧的 `nk>` 会被当成正文透传出去
            if open.starts_with(trimmed) {
                return String::new();
            }
        }
        // 首部不是任何已知标签（也不是它们的前缀）→ 判定完毕，原样吐出
        self.done = true;
        std::mem::take(&mut self.buffer)
    }

    /// 流结束 / 文本末尾：把还没判定完的缓冲原样交出。
    ///
    /// 标签没闭合 = 无法判定（上游被切断，或正文真的以 `<thi` 开头）。
    /// 这里**不剥**、也不丢：宁可多下发几个字节，也不静默吃掉正文。
    /// 已经判定完毕时返回空串（调用方只在结尾调一次）。
    pub fn finish(&mut self) -> String {
        self.done = true;
        std::mem::take(&mut self.buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_only_affects_leading_block() {
        let (out, hit) = strip_leading("<think>internal</think>real answer");
        assert!(hit);
        assert_eq!(out, "real answer");
        // 正文中间的标签不动（那是用户内容）
        let (out, hit) = strip_leading("see <think>y</think> here");
        assert!(!hit, "没剥任何标签时 hit 必须是 false");
        assert_eq!(out, "see <think>y</think> here");
    }

    #[test]
    fn strip_handles_all_four_tag_pairs() {
        // Qoder 的 THINK_TAGS 有 4 组（qoder/protocol.rs:41），不是只有 <think>
        for (open, close) in [
            ("<thinking>", "</thinking>"),
            ("<think>", "</think>"),
            ("<reasoning>", "</reasoning>"),
            ("<thought>", "</thought>"),
        ] {
            let text = format!("{open}x{close}body");
            let (out, hit) = strip_leading(&text);
            assert!(hit, "标签对 {open} 必须被识别");
            assert_eq!(out, "body");
        }
    }

    #[test]
    fn strip_handles_split_across_frames() {
        // 跨帧：`<thi` + `nk>x</think>` + `body`
        let mut filter = LeadingThinkFilter::default();
        let mut out = String::new();
        for chunk in ["<thi", "nk>x</think>", "body"] {
            out.push_str(&filter.push(chunk));
        }
        assert_eq!(out, "body", "剥完标签后正文必须完整且无残留");
        assert!(filter.stripped());
    }

    #[test]
    fn filter_passes_through_when_no_leading_tag() {
        let mut filter = LeadingThinkFilter::default();
        // 第一帧就不是标签开头 → 立刻进入 done，后续原样透传（零额外延迟）
        assert_eq!(filter.push("hello"), "hello");
        assert!(filter.is_done(), "首帧不是标签就必须立刻判定完毕");
        assert!(!filter.stripped());
        assert_eq!(filter.push(" world"), " world");
    }

    /// 标签前的空白与标签一起丢；没有标签时空白原样保留。
    #[test]
    fn whitespace_before_the_tag_is_dropped_with_it() {
        let (out, hit) = strip_leading("  \n<think>x</think>body");
        assert!(hit);
        assert_eq!(out, "body", "标签前的空白属于标签块的分隔噪声");
        let (out, hit) = strip_leading("  hello");
        assert!(!hit);
        assert_eq!(out, "  hello", "没有标签时空白一个字节都不动");
    }

    /// 没闭合的标签：判定不了，结尾原样交出（宁可多几个字节也不吃正文）。
    #[test]
    fn unterminated_tag_is_flushed_verbatim() {
        let mut filter = LeadingThinkFilter::default();
        assert_eq!(filter.push("<thi"), "");
        assert!(!filter.is_done(), "半截标签不能提前判定");
        assert_eq!(filter.finish(), "<thi");
        assert!(!filter.stripped());
    }
}

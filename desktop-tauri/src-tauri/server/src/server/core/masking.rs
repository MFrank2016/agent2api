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
//! [`MaskSession`] **拥有**规则（`Arc<[CompiledRule]>` 克隆），不带生命周期
//! 参数 —— 于是它能跟着请求跨 `await` 存活（请求结构体无法携带借用）。出站
//! 路径的入口是 [`mask_outbound`]：构造请求时调用一次，把「脱敏后的字节 +
//! 会话」一起放进传输层（`TransportRequest`），回复侧再用同一份会话还原。
//!
//! ## 出站门禁（fail-closed）
//!
//! 开关由 [`config::active`] 决定：关闭时 [`mask_outbound`] 原样放行（连 JSON
//! 都不解析，字节与改造前一致）；开启却做不到（脱敏器不可用、请求体不是
//! JSON）时返回错误，调用方必须**一个字节都不发**。宁可让这条请求 503，
//! 也不能把明文漏给上游。
//!
//! ## 占位符
//!
//! 形如 `[[A2A_MASK_<kind>_<n>]]`，`n` 进程内全局递增（跨会话也不重复），
//! 因此占位符不会与普通文本或其他会话的占位符混淆；还原时按前缀精确匹配，
//! 不认识的占位符原样保留。

pub(crate) mod config;
mod rules;
pub use config::MaskConfig;
pub use rules::{MaskSession, RulesMasker};

use serde_json::Value;

/// 占位符前缀。`restore` 只认这个前缀，避免误伤普通方括号文本。
pub const PLACEHOLDER_PREFIX: &str = "[[A2A_MASK_";

/// 占位符后缀：与前缀一起界定一个完整的占位符。
pub const PLACEHOLDER_SUFFIX: &str = "]]";

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

    /// 按**类型**汇总成 `(kind, 次数)`（供请求日志的 hit 元数据用）。
    ///
    /// 计数单位是**占位符**而不是出现次数：同一个值复用同一个占位符（`note`
    /// 已按占位符去重），所以「3 处相同的邮箱」只算 1 次 —— 与
    /// `TelemetrySnapshot::mask_hits` 的「命中了几类、各几个值」口径一致。
    /// 顺序按类型首次出现的顺序，读侧不需要再排。
    pub fn counts(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for hit in &self.hits {
            match counts.iter_mut().find(|(kind, _)| kind == hit.kind) {
                Some((_, count)) => *count += 1,
                None => counts.push((hit.kind.to_string(), 1)),
            }
        }
        counts
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

/// 出站门禁：按配置决定要不要脱敏，并给出**本次请求的会话**。
///
/// 返回 `Ok(None)` 表示「未开启脱敏」—— 调用方原样发送，字节与改造前完全
/// 一致（连 JSON 都不解析，避免无谓的序列化漂移）。
///
/// 返回 `Ok(Some((masked, session, report)))` 表示已脱敏：`masked` 是新的
/// 请求体，`session` 持有本次映射（回复侧用它还原，Task 5.3），`report` 是
/// 本次命中元数据（**只有类型与占位符，没有原文**；调用方据此记
/// `mask_hits`）。
///
/// 错误一律 **fail-closed**：配置要求脱敏却做不到（没有脱敏器、请求体不是
/// JSON、序列化失败）时返回错误文案，调用方必须**一个字节都不发**。宁可让
/// 这一条请求 503，也不能把明文漏给上游。
pub fn mask_outbound(payload: &str) -> Result<Option<(String, MaskSession, MaskReport)>, String> {
    if !config::active().enabled {
        return Ok(None);
    }
    let Some(masker) = config::masker() else {
        return Err("脱敏已开启但脱敏器不可用，未发送任何请求".to_string());
    };
    let mut value: Value = serde_json::from_str(payload)
        .map_err(|_| "脱敏已开启但请求体不是合法 JSON，未发送任何请求".to_string())?;
    let session = masker.session();
    let report = session.mask_json(&mut value);
    let serialized = serde_json::to_string(&value)
        .map_err(|_| "脱敏后请求体序列化失败，未发送任何请求".to_string())?;
    Ok(Some((serialized, session, report)))
}

/// 跨分片还原的字节缓冲（流式回复用）。
///
/// ── 为什么需要它 ──────────────────────────────────────────────
/// 上游按 TCP 分片吐字节，SSE 的 `data:` 事件本身也可能被切开；一个占位符
/// `[[A2A_MASK_…]]` 完全可能横跨两个分片。逐片 `restore` 会把它当成普通文本
/// 原样下发（客户端看到占位符而不是原文），因此这里按**字节**缓冲：
///
///   - 完整占位符立即还原；
///   - 结尾可能是占位符前缀的片段（如 `[[A2A_M`）先压住，等下一片补齐；
///   - 完整前缀已经出现但还没有 `]]` 时，从前缀起整段压住；
///   - 只有 [`PlaceholderBuffer::finish`] 才把压住的字节原样吐出 —— 它本来就
///     不是占位符（上游被切断 / 普通方括号文本），不能吞掉。
///
/// ── fail-closed：不能安全放进原始 JSON 的原文保留占位符 ──────────
/// 原始字节这条路径没有 JSON 转义能力：原文若含 `"` / `\` / 控制字符，直接替换
/// 会破坏 `data: {…}` 外壳（下游解析器整帧丢弃，客户端拿到 200 却少内容）。
/// 这类值在这里**保持占位符**（不发明文、也不破坏帧），非流式出口会在 JSON
/// 解析之后由 [`Restorer::restore_json`] 正确还原。
///
/// ── 为什么按字节而不是 `String` ───────────────────────────────
/// 分片边界不保证落在 UTF-8 字符边界（中文正文很常见）。按字节扫描占位符、
/// 其余字节原样搬运，就不会把半截汉字解码成 U+FFFD，也不会因为一片非法
/// UTF-8 而 panic。
///
/// 压住的字节最多是一个前缀的长度（约 11 字节）+ 一个未闭合的占位符，
/// **不会**缓冲整段回复：客户端看到的只是这一小段延迟。
#[derive(Default)]
pub struct PlaceholderBuffer {
    /// 上一片结尾处还没判定完的字节（可能是半截前缀 / 未闭合占位符）
    tail: Vec<u8>,
}

impl PlaceholderBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 吃一片上游字节，返回这一片可以下发的字节（可能是空）。
    ///
    /// `restore` 通常是 `|text| session.restore(text)`；完整占位符按它还原，
    /// 不认识的占位符由 `restore` 原样保留。
    pub fn push(&mut self, chunk: &[u8], restore: &dyn Fn(&str) -> String) -> Vec<u8> {
        self.tail.extend_from_slice(chunk);
        let emit_len = split_point(&self.tail);
        // 先把要下发的部分摘出来，再留下尾巴：`split_off` 返回后半段
        let tail = self.tail.split_off(emit_len);
        let emit = std::mem::replace(&mut self.tail, tail);
        restore_placeholders(&emit, restore)
    }

    /// 流结束：把压住的字节原样下发（它们不是完整的占位符）。
    pub fn finish(self, restore: &dyn Fn(&str) -> String) -> Vec<u8> {
        restore_placeholders(&self.tail, restore)
    }
}

/// 回复还原器：会话 + 跨分片缓冲。
///
/// `None` 会话表示未开启脱敏：`push` 返回 `None`，调用方**原样透传**这一片
/// 字节（与接入前逐字节一致），`finish` 返回空。
pub struct Restorer {
    session: Option<MaskSession>,
    buffer: PlaceholderBuffer,
}

impl Restorer {
    pub fn new(session: Option<MaskSession>) -> Self {
        Self {
            session,
            buffer: PlaceholderBuffer::new(),
        }
    }

    /// 吃一片上游字节，返回要下发的字节；`None` = 未脱敏，原样透传。
    pub fn push(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        let Self { session, buffer } = self;
        let session = session.as_ref()?;
        Some(buffer.push(chunk, &|text| session.restore(text)))
    }

    /// 流结束：冲刷压住的半截前缀；未脱敏时没有可冲刷的字节。
    pub fn finish(&mut self) -> Vec<u8> {
        let Self { session, buffer } = self;
        let Some(session) = session.as_ref() else {
            return Vec::new();
        };
        // `PlaceholderBuffer::finish` 按值消费；这里换一个空的出来（之后
        // 本对象不会再被 push —— 调用方只在 EOF 时调一次）
        let buffer = std::mem::take(buffer);
        buffer.finish(&|text| session.restore(text))
    }

    /// 还原一份已经解析成 JSON 的响应体（非流式出口**唯一**的还原点）。
    ///
    /// ── 为什么在 Value 上还原、而不是原始字节 ────────────────────
    /// 聚合器把 `content` / `tool_calls.function.arguments` 拼回一个字符串之后，
    /// 占位符重新变完整 —— 无论它原来是被 TCP 分片切开，还是被上游拆进两个
    /// `delta` 事件（后者在原始字节里夹着 `"}]}\n\ndata: {…` 外壳，已经不连续）。
    /// 更重要的是：这里由 serde 负责转义，原文含 `"` / `\` / 控制字符也不会
    /// 破坏 JSON（原始字节路径对这些值是 fail-closed 保留占位符）。
    ///
    /// 只改字符串值（对象键与 `mask_json` 同口径不动）；未脱敏时什么都不做。
    pub fn restore_json(&self, value: &mut Value) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        restore_value(value, session);
    }
}

/// 递归还原 JSON 里的全部字符串值（对象键不动）。
fn restore_value(value: &mut Value, session: &MaskSession) {
    match value {
        Value::String(text) => {
            let restored = session.restore(text);
            if restored != *text {
                *text = restored;
            }
        }
        Value::Array(items) => {
            for item in items {
                restore_value(item, session);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                restore_value(item, session);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// 这一片里「要下发」与「先压住」的分界点（见 [`PlaceholderBuffer`]）。
///
/// 返回 `bytes.len()` 表示全部可以下发。两条规则（先命中先用）：
///   1. 最后一个完整前缀之后还没有 `]]` → 从前缀起整段压住；
///   2. 结尾是前缀的一段**真前缀**（如 `[[A2A_M`）→ 压住这段。
fn split_point(bytes: &[u8]) -> usize {
    let prefix = PLACEHOLDER_PREFIX.as_bytes();
    if let Some(start) = rfind_bytes(bytes, prefix) {
        let after = &bytes[start + prefix.len()..];
        if find_bytes(after, PLACEHOLDER_SUFFIX.as_bytes()).is_none() {
            return start;
        }
    }
    for keep in (1..prefix.len()).rev() {
        if bytes.ends_with(&prefix[..keep]) {
            return bytes.len() - keep;
        }
    }
    bytes.len()
}

/// 还原一段字节里的全部完整占位符；其余字节**原样搬运**。
///
/// 只对占位符本身做 UTF-8 解码（前缀 / 后缀都是 ASCII），因此非法 UTF-8 的
/// 正文不会被改动，也不会 panic。原文不能安全放进原始 JSON 时保留占位符
/// （见 [`plaintext_fits_raw_json`]）。
fn restore_placeholders(bytes: &[u8], restore: &dyn Fn(&str) -> String) -> Vec<u8> {
    let prefix = PLACEHOLDER_PREFIX.as_bytes();
    let suffix = PLACEHOLDER_SUFFIX.as_bytes();
    // 快路径：没有前缀就一个字节都不动（绝大多数分片走这里）
    if find_bytes(bytes, prefix).is_none() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(index) = find_bytes(rest, prefix) {
        out.extend_from_slice(&rest[..index]);
        let after = &rest[index..];
        let Some(end) = find_bytes(&after[prefix.len()..], suffix) else {
            // 不完整（只可能出现在 `finish`）：保留前缀，从它之后继续找，
            // 不能吞掉后面的正文
            out.extend_from_slice(prefix);
            rest = &after[prefix.len()..];
            continue;
        };
        let token_end = prefix.len() + end + suffix.len();
        let token = &after[..token_end];
        match std::str::from_utf8(token) {
            // fail-closed：原文若含 `"` / `\` / 控制字符，塞进原始 `data: {…}`
            // 字节会破坏 JSON 外壳（下游解析器整帧丢弃，客户端 200 少内容）。
            // 这种值**不下发明文**，保留占位符 —— 非流式出口会在 JSON 解析
            // 之后用 `Restorer::restore_json` 正确还原（那里由序列化器负责转义）。
            Ok(text) => match restore(text) {
                restored if plaintext_fits_raw_json(&restored) => {
                    out.extend_from_slice(restored.as_bytes())
                }
                _ => out.extend_from_slice(token),
            },
            // 占位符内部出现非法 UTF-8：原样保留，不做任何猜测
            Err(_) => out.extend_from_slice(token),
        }
        rest = &after[token_end..];
    }
    out.extend_from_slice(rest);
    out
}

/// 还原出来的原文能不能直接放进原始 SSE / JSON 字节里。
///
/// `"` 会提前闭合 JSON 字符串、`\` 会变成非法转义、控制字符（含换行）会截断
/// 或拆坏 `data:` 行 —— 三者都会让下游解析器整帧失败。这类值在**原始字节**
/// 这条路径上保持占位符（fail-closed），交给解析后的 `restore_json` 还原。
fn plaintext_fits_raw_json(text: &str) -> bool {
    !text
        .bytes()
        .any(|byte| byte == b'"' || byte == b'\\' || byte < 0x20)
}

/// 字节串里第一次出现 `needle` 的位置（`needle` 非空）。
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 字节串里最后一次出现 `needle` 的位置（`needle` 非空）。
fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
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

    /// 只开密钥脱敏：邮箱原样上行（三档互相独立）。
    #[test]
    fn secrets_only_does_not_mask_emails() {
        let masker = RulesMasker::with_rules(true, false, &[]);
        let (text, report) = masker.mask_text("key sk-abc123 mail a@b.com");
        assert!(!text.contains("sk-abc123"), "{text}");
        assert!(text.contains("a@b.com"), "只开密钥时邮箱必须原样: {text}");
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].kind, "api_key");
    }

    /// 只开个人信息脱敏：密钥原样上行。
    #[test]
    fn pii_only_does_not_mask_tokens() {
        let masker = RulesMasker::with_rules(false, true, &[]);
        let (text, report) = masker.mask_text("key sk-abc123 mail a@b.com");
        assert!(
            text.contains("sk-abc123"),
            "只开 PII 时密钥必须原样: {text}"
        );
        assert!(!text.contains("a@b.com"), "{text}");
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].kind, "email");
    }

    /// 只给自定义词：内置规则一条都不跑，词按字面盖掉。
    #[test]
    fn words_only_masks_custom_words() {
        let masker = RulesMasker::with_rules(false, false, &["Project Falcon".to_string()]);
        let (text, report) = masker.mask_text("Project Falcon sk-abc123 a@b.com");
        assert!(!text.contains("Project Falcon"), "{text}");
        assert!(
            text.contains("sk-abc123"),
            "没开密钥时 token 必须原样: {text}"
        );
        assert!(text.contains("a@b.com"), "没开 PII 时邮箱必须原样: {text}");
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].kind, "word");
        assert_eq!(masker.restore(&text), "Project Falcon sk-abc123 a@b.com");
    }

    /// 报告按 kind 汇总成（类型, 次数），不出现原文。
    #[test]
    fn report_counts_kinds_without_originals() {
        let masker = RulesMasker::default();
        let (_text, report) = masker.mask_text("sk-abc123 a@b.com sk-abc123");
        let counts = report.counts();
        assert_eq!(
            counts,
            vec![("api_key".to_string(), 1), ("email".to_string(), 1)],
            "同一个值复用同一个占位符，只算一次"
        );
        let debug = format!("{counts:?}");
        assert!(!debug.contains("sk-abc123"), "{debug}");
        assert!(!debug.contains("a@b.com"), "{debug}");
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

    // ── 出站门禁（mask_outbound）────────────────────────────────────

    /// 关闭脱敏：原样放行，调用方拿不到会话（= 回复侧不做还原）。
    #[test]
    fn masking_disabled_is_byte_identical() {
        let _guard = config::testing::disabled();
        let payload = r#"{"model":"m","messages":[{"role":"user","content":"a@b.com"}]}"#;
        let result = mask_outbound(payload);
        assert!(matches!(result, Ok(None)), "{:?}", result.err());
        // 关闭时连解析都不做：字节不变。
        assert_eq!(
            payload,
            r#"{"model":"m","messages":[{"role":"user","content":"a@b.com"}]}"#
        );
    }

    /// 开启但没有可用脱敏器：fail-closed，绝不把明文发出去。
    #[test]
    fn masking_enabled_without_masker_sends_nothing() {
        let _guard = config::testing::enabled_without_masker();
        match mask_outbound(r#"{"messages":[]}"#) {
            Err(error) => assert!(error.contains("未发送任何请求"), "{error}"),
            Ok(_) => panic!("没有脱敏器时必须报错而不是放行"),
        }
    }

    /// 开启：对象键不变、字符串值脱敏，且返回的会话能还原原文。
    #[test]
    fn masking_enabled_masks_json_values() {
        let masker = std::sync::Arc::new(RulesMasker::default());
        let _guard = config::testing::enabled_with(masker);
        let payload =
            r#"{"model":"example-model","messages":[{"role":"user","content":"mail a@b.com"}]}"#;
        let (masked, session, _report) = mask_outbound(payload)
            .expect("脱敏器可用时不应失败")
            .expect("开启脱敏时应返回掩码后的请求体");
        assert!(!masked.contains("a@b.com"), "{masked}");
        assert!(masked.contains(PLACEHOLDER_PREFIX), "{masked}");
        // 键与普通值不变。
        let value: Value = serde_json::from_str(&masked).expect("掩码后仍是合法 JSON");
        assert_eq!(value["model"], "example-model");
        assert!(value["messages"][0].get("content").is_some());
        // 会话可还原（Task 5.3 的回复还原依赖这一份映射）。
        let restored = session.restore(&masked);
        assert!(restored.contains("a@b.com"), "{restored}");
    }

    /// 出站门禁把命中元数据一并交回（调用方据此记 `mask_hits`，不记原文）。
    #[test]
    fn mask_outbound_returns_hit_metadata() {
        let masker = std::sync::Arc::new(RulesMasker::with_rules(true, false, &[]));
        let _guard = config::testing::enabled_with(masker);
        let payload = r#"{"messages":[{"role":"user","content":"sk-abc123 a@b.com"}]}"#;
        let (masked, _session, report) = mask_outbound(payload)
            .expect("脱敏器可用时不应失败")
            .expect("开启脱敏时应返回掩码后的请求体");
        assert!(!masked.contains("sk-abc123"), "{masked}");
        assert!(masked.contains("a@b.com"), "只开密钥时邮箱原样: {masked}");
        assert_eq!(report.counts(), vec![("api_key".to_string(), 1)]);
    }

    /// 只开 PII：出站门禁只盖邮箱，密钥原样上行。
    #[test]
    fn mask_outbound_pii_only_leaves_tokens_alone() {
        let masker = std::sync::Arc::new(RulesMasker::with_rules(false, true, &[]));
        let _guard = config::testing::enabled_with(masker);
        let payload = r#"{"messages":[{"role":"user","content":"sk-abc123 a@b.com"}]}"#;
        let (masked, _session, report) = mask_outbound(payload)
            .expect("脱敏器可用时不应失败")
            .expect("开启脱敏时应返回掩码后的请求体");
        assert!(
            masked.contains("sk-abc123"),
            "只开 PII 时密钥原样: {masked}"
        );
        assert!(!masked.contains("a@b.com"), "{masked}");
        assert_eq!(report.counts(), vec![("email".to_string(), 1)]);
    }

    /// 只给自定义词：出站门禁按字面盖词，内置规则不跑。
    #[test]
    fn mask_outbound_words_only_masks_custom_words() {
        let masker = std::sync::Arc::new(RulesMasker::with_rules(
            false,
            false,
            &["Project Falcon".to_string()],
        ));
        let _guard = config::testing::enabled_with(masker);
        let payload = r#"{"messages":[{"role":"user","content":"Project Falcon sk-abc123"}]}"#;
        let (masked, _session, report) = mask_outbound(payload)
            .expect("脱敏器可用时不应失败")
            .expect("开启脱敏时应返回掩码后的请求体");
        assert!(!masked.contains("Project Falcon"), "{masked}");
        assert!(
            masked.contains("sk-abc123"),
            "没开密钥时 token 原样: {masked}"
        );
        assert_eq!(report.counts(), vec![("word".to_string(), 1)]);
    }

    /// 开启但请求体不是 JSON：fail-closed，返回错误而不是原样发送。
    #[test]
    fn masking_enabled_rejects_non_json_payload() {
        let masker = std::sync::Arc::new(RulesMasker::default());
        let _guard = config::testing::enabled_with(masker);
        assert!(mask_outbound("not json").is_err());
    }

    // ── 回复还原（PlaceholderBuffer / Restorer，Task 5.3）────────────

    /// 建一个会话并脱敏一段文本，返回（会话, 脱敏后的文本, 占位符）
    fn masked(text: &str) -> (MaskSession, String, String) {
        let session = RulesMasker::default().session();
        let (masked, report) = session.mask_text(text);
        let placeholder = report
            .hits
            .first()
            .map(|hit| hit.placeholder.clone())
            .unwrap_or_default();
        (session, masked, placeholder)
    }

    /// 占位符横跨两个分片（`[[A2A_M` + `ASK_api_key_1]]`）时必须还原成原文，
    /// 不能把占位符当普通文本下发给客户端。
    #[test]
    fn sse_placeholder_across_chunks() {
        let (session, masked, placeholder) = masked("key sk-abc123");
        assert!(!placeholder.is_empty(), "样例应当命中一条规则: {masked}");
        let split = masked
            .find(&placeholder)
            .map(|index| index + 7) // `[[A2A_M` 之后
            .unwrap_or(masked.len() / 2);
        let (head, tail) = masked.split_at(split);

        let mut buffer = PlaceholderBuffer::new();
        let restore = |text: &str| session.restore(text);
        let mut out = buffer.push(head.as_bytes(), &restore);
        out.extend(buffer.push(tail.as_bytes(), &restore));
        out.extend(buffer.finish(&restore));

        let restored = String::from_utf8(out).expect("样例都是 UTF-8");
        assert_eq!(restored, "key sk-abc123");
        assert!(!restored.contains(PLACEHOLDER_PREFIX), "{restored}");
    }

    /// 占位符在任何字节边界被切开都要能还原（分片边界不保证落在占位符外）。
    #[test]
    fn placeholder_is_restored_at_every_chunk_boundary() {
        let (session, masked, placeholder) = masked("mail a@b.com now");
        let start = masked.find(&placeholder).expect("样例应当含占位符");
        let end = start + placeholder.len();
        for split in start..=end {
            let (head, tail) = masked.split_at(split);
            let mut buffer = PlaceholderBuffer::new();
            let restore = |text: &str| session.restore(text);
            let mut out = buffer.push(head.as_bytes(), &restore);
            out.extend(buffer.push(tail.as_bytes(), &restore));
            out.extend(buffer.finish(&restore));
            let restored = String::from_utf8(out).expect("样例都是 UTF-8");
            assert_eq!(restored, "mail a@b.com now", "在字节 {split} 处切开时失败");
        }
    }

    /// 流结束时压着的半截前缀必须原样吐出 —— 它可能只是普通方括号文本，
    /// 吞掉就是丢正文。
    #[test]
    fn sse_partial_prefix_at_end_is_not_emitted() {
        let session = RulesMasker::default().session();
        let restore = |text: &str| session.restore(text);
        let mut buffer = PlaceholderBuffer::new();
        let mut out = buffer.push(b"before [[A2A_M", &restore);
        assert!(
            out.is_empty() || out == b"before ",
            "半截前缀不能先发出去: {out:?}"
        );
        out.extend(buffer.finish(&restore));
        assert_eq!(
            String::from_utf8(out).expect("样例都是 UTF-8"),
            "before [[A2A_M"
        );
    }

    /// 非法 UTF-8 原样透传（不 panic、不改字节）。
    #[test]
    fn invalid_utf8_passes_through() {
        let session = RulesMasker::default().session();
        let restore = |text: &str| session.restore(text);
        let chunk = b"data: \xff\xfe broken\n\n";
        let mut buffer = PlaceholderBuffer::new();
        let mut out = buffer.push(chunk, &restore);
        out.extend(buffer.finish(&restore));
        assert_eq!(out, chunk);
    }

    /// 未开启脱敏（没有会话）：`Restorer` 明确表示「原样透传」，不做任何缓冲。
    #[test]
    fn disabled_masking_is_byte_identical() {
        let chunk = b"data: {\"x\":\"[[A2A_MASK_email_1]]\"}\n\n";
        let mut restorer = Restorer::new(None);
        assert!(restorer.push(chunk).is_none());
        assert!(restorer.finish().is_empty());
    }

    /// 原文含 `"` / `\` / 控制字符时，**原始字节**路径必须保留占位符
    /// （fail-closed）：直接替换会破坏 `data: {…}` 的 JSON 外壳。
    /// 解析后的 JSON 路径（`restore_json`）由序列化器转义，能完整还原。
    #[test]
    fn unsafe_plaintext_keeps_the_placeholder_on_raw_bytes() {
        let masker = RulesMasker::with_words(&["he said \"hi\"".to_string()]);
        let session = masker.session();
        let (masked, report) = session.mask_text("say he said \"hi\" now");
        assert_eq!(report.hits.len(), 1, "{report:?}");

        let restore = |text: &str| session.restore(text);
        let mut buffer = PlaceholderBuffer::new();
        let mut out = buffer.push(masked.as_bytes(), &restore);
        out.extend(buffer.finish(&restore));
        assert_eq!(
            String::from_utf8(out).expect("样例都是 UTF-8"),
            masked,
            "原始字节路径不能把带引号的原文塞进 JSON"
        );

        // 解析后还原：由 serde 负责转义，原文完整回来
        let mut value = serde_json::json!({ "content": masked });
        Restorer::new(Some(session)).restore_json(&mut value);
        assert_eq!(value["content"], "say he said \"hi\" now");
    }

    /// 占位符内部是非法 UTF-8：原样透传（不 panic、不改字节）。
    #[test]
    fn invalid_utf8_inside_placeholder_passes_through() {
        let session = RulesMasker::default().session();
        let restore = |text: &str| session.restore(text);
        let chunk = b"[[A2A_MASK_\xff]]";
        let mut buffer = PlaceholderBuffer::new();
        let mut out = buffer.push(chunk, &restore);
        out.extend(buffer.finish(&restore));
        assert_eq!(out, chunk);
    }

    /// 流结束时压着的半截前缀要由 `finish` 吐出（经 `Restorer` 的接线路径）。
    #[test]
    fn restorer_finish_emits_held_prefix() {
        let session = RulesMasker::default().session();
        let mut restorer = Restorer::new(Some(session));
        let emitted = restorer.push(b"data: tail [[A2A_M").unwrap_or_default();
        assert_eq!(emitted, b"data: tail ");
        assert_eq!(restorer.finish(), b"[[A2A_M");
    }
}

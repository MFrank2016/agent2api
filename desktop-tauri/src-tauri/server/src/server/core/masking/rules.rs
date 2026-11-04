//! 默认脱敏规则与占位符映射实现（详见父模块 `masking` 的模块头）。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use regex::Regex;
use serde_json::Value;

use super::{MaskReport, Masker, PLACEHOLDER_PREFIX, PLACEHOLDER_SUFFIX};

/// `RulesMasker` 自身（非 session）映射表的上限。它只用于单测 / 单次调用；
/// 生产请求路径一律用 [`RulesMasker::session`]，映射随请求释放，见父模块模块头。
const MAX_SHARED_ENTRIES: usize = 1024;

/// 进程内全局递增的占位符序号：跨会话也不重复，避免「同号不同值」的歧义。
static NEXT_PLACEHOLDER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct CompiledRule {
    kind: &'static str,
    regex: Regex,
}

impl CompiledRule {
    fn new(kind: &'static str, pattern: &str) -> Self {
        Self {
            kind,
            regex: Regex::new(pattern).expect("内置脱敏正则必须可编译"),
        }
    }
}

/// 内置规则（进程内只编译一次）。
///
/// 顺序即优先级：同起点重叠时先取更长的匹配（fail-closed），长度也相同时
/// 靠前的规则优先。自定义词放在最后，避免它先吃掉内置规则的匹配。
fn default_rules() -> &'static [CompiledRule] {
    static RULES: OnceLock<Vec<CompiledRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        vec![
            // 真实 OpenAI 风格 Key 是 `sk-` + 48 位，但任务用例里的样例是
            // `sk-abc123`（6 位），故下限取 6；脱敏宁可多盖一点，也不漏。
            CompiledRule::new("api_key", r"\bsk-[A-Za-z0-9_-]{6,}"),
            CompiledRule::new("api_key", r"\bghp_[A-Za-z0-9]{8,}"),
            CompiledRule::new("api_key", r"\bgithub_pat_[A-Za-z0-9_]{8,}"),
            CompiledRule::new("api_key", r"\bxoxb-[A-Za-z0-9-]{8,}"),
            CompiledRule::new(
                "email",
                r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
            ),
        ]
    })
}

/// 单个自定义词的最大长度：超长词直接跳过。自定义词来自配置，不能在请求
/// 路径上因为正则编译失败而 panic（宁可少盖一个词，也不带走整个进程）。
const MAX_WORD_CHARS: usize = 4096;

/// 自定义词表：按字面匹配（`regex::escape`），不做大小写折叠。
fn word_rules(words: &[String]) -> Vec<CompiledRule> {
    words
        .iter()
        .filter(|word| !word.is_empty() && word.chars().count() <= MAX_WORD_CHARS)
        .filter_map(|word| {
            Regex::new(&regex::escape(word))
                .ok()
                .map(|regex| CompiledRule {
                    kind: "word",
                    regex,
                })
        })
        .collect()
}

#[derive(Default)]
struct State {
    /// 原文 → 占位符（同一个值复用同一个占位符）。
    forward: HashMap<String, String>,
    /// 占位符 → 原文（restore 用）。
    reverse: HashMap<String, String>,
    /// 插入顺序，仅在有上限时用于淘汰。
    order: VecDeque<String>,
    limit: Option<usize>,
}

impl State {
    fn bounded(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }

    fn placeholder_for(&mut self, kind: &'static str, original: &str) -> String {
        if let Some(existing) = self.forward.get(original) {
            return existing.clone();
        }
        let id = NEXT_PLACEHOLDER.fetch_add(1, Ordering::Relaxed);
        let placeholder = format!("{PLACEHOLDER_PREFIX}{kind}_{id}{PLACEHOLDER_SUFFIX}");
        self.forward
            .insert(original.to_string(), placeholder.clone());
        self.reverse
            .insert(placeholder.clone(), original.to_string());
        if let Some(limit) = self.limit {
            self.order.push_back(original.to_string());
            while self.order.len() > limit {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                if let Some(removed) = self.forward.remove(&oldest) {
                    self.reverse.remove(&removed);
                }
            }
        }
        placeholder
    }
}

struct SpanMatch<'a> {
    start: usize,
    end: usize,
    priority: usize,
    kind: &'static str,
    value: &'a str,
}

/// 单遍扫描原文收集所有规则的匹配，按位置排序后一次性替换。
///
/// 不逐条规则地在「已替换文本」上继续跑：那样后一条规则可能命中前一条
/// 生成的占位符，且命中顺序会随规则顺序漂移。这里所有 span 都取自原文，
/// 互不污染。
fn mask_text_with(rules: &[CompiledRule], text: &str, state: &mut State) -> (String, MaskReport) {
    if text.is_empty() || rules.is_empty() {
        return (text.to_string(), MaskReport::default());
    }
    let mut spans = Vec::new();
    for (priority, rule) in rules.iter().enumerate() {
        for found in rule.regex.find_iter(text) {
            spans.push(SpanMatch {
                start: found.start(),
                end: found.end(),
                priority,
                kind: rule.kind,
                value: found.as_str(),
            });
        }
    }
    if spans.is_empty() {
        return (text.to_string(), MaskReport::default());
    }
    // 起点靠前优先；同起点时**长匹配优先**（fail-closed：宁可把整段
    // `sk-xxx@example.com` 当邮箱全盖掉，也不留下 `@example.com` 尾巴），
    // 再按规则优先级。
    spans.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then(right.end.cmp(&left.end))
            .then(left.priority.cmp(&right.priority))
    });

    let mut out = String::with_capacity(text.len());
    let mut report = MaskReport::default();
    let mut cursor = 0;
    for span in spans {
        if span.end <= cursor {
            // 已被前面更靠前 / 更长的 span 完整覆盖。
            continue;
        }
        if span.start < cursor {
            // 部分重叠：起点已被覆盖，但尾巴还在后面。只盖住未覆盖的那段，
            // 否则 `a@b.sk-abcdefgh` 这种「邮箱先吃掉 `a@b.sk`」的场景会把
            // 密钥剩下的 `-abcdefgh` 漏出去（必须 fail-closed）。
            // 占位符映射到精确的尾巴原文，restore 仍能逐段拼回。
            let tail = &text[cursor..span.end];
            let placeholder = state.placeholder_for(span.kind, tail);
            report.note(span.kind, &placeholder);
            out.push_str(&placeholder);
            cursor = span.end;
            continue;
        }
        out.push_str(&text[cursor..span.start]);
        let placeholder = state.placeholder_for(span.kind, span.value);
        report.note(span.kind, &placeholder);
        out.push_str(&placeholder);
        cursor = span.end;
    }
    out.push_str(&text[cursor..]);
    (out, report)
}

fn mask_value(value: &mut Value, rules: &[CompiledRule], state: &mut State, in_media: bool) -> MaskReport {
    match value {
        Value::String(text) => {
            // ── 图片 / 多模态载荷不脱敏（2026-11 修）─────────────────
            // base64 里出现 `sk-` 这样的子串是**概率事件**（字符集重叠），
            // 替换它会把图片数据改坏（上游解码失败 / 画面损坏），而且这类
            // 载荷本来就不是「用户写下的秘密文本」。判据只收「明显是二进制」
            // 的形状，普通正文（含空格、标点、较短）照常脱敏。
            if in_media || looks_like_binary_payload(text) {
                return MaskReport::default();
            }
            let (next, report) = mask_text_with(rules, text, state);
            *text = next;
            report
        }
        Value::Array(items) => {
            let mut report = MaskReport::default();
            for item in items {
                report.absorb(mask_value(item, rules, state, in_media));
            }
            report
        }
        Value::Object(map) => {
            // 只遍历值：对象键（字段名）不是用户内容，不能脱敏。
            // 图片载荷的常见外壳（OpenAI / Anthropic / Gemini 三种形状）由键名
            // 认出来，整棵子树跳过；其余对象照常递归。
            let mut report = MaskReport::default();
            for (key, item) in map.iter_mut() {
                let media = in_media || is_media_payload_key(key);
                report.absorb(mask_value(item, rules, state, media));
            }
            report
        }
        _ => MaskReport::default(),
    }
}

/// 图片 / 多模态载荷的外壳键：这些键下的字符串是二进制（base64）或媒体 URL，
/// 不是用户正文。三种主流形状：
///   - OpenAI：`image_url`（其下 `url`）
///   - Anthropic：`source`（其下 `data` / `url`）
///   - Gemini：`inline_data`（其下 `data`）
fn is_media_payload_key(key: &str) -> bool {
    matches!(key, "inline_data" | "image_url" | "source")
}

/// 字符串看起来像二进制载荷（base64 / data URL）。
///
/// 保守判定，只跳过「一眼不是自然语言」的值：
///   - `data:` 开头（data URL，base64 里可能夹换行，前缀足以认定）；
///   - 长度 > 128 且**全部**由 base64 字符集组成（没有空格 / 标点 / 换行）。
///
/// 普通正文（句子、代码、短标识符）不会同时满足这两条。
fn looks_like_binary_payload(text: &str) -> bool {
    if text.starts_with("data:") {
        return true;
    }
    text.len() > 128
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'-' | b'_'))
}

/// 单遍还原：只替换本映射表认识的占位符，不认识的（含残缺前缀）原样保留。
///
/// 不用 `str::replace` 逐个替换：若原文本身含 `[[A2A_MASK_` 形状的文本，
/// 多轮替换可能把「还原出来的原文」再当成占位符处理。
fn restore_with(state: &State, text: &str) -> String {
    if !text.contains(PLACEHOLDER_PREFIX) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(PLACEHOLDER_PREFIX) {
        out.push_str(&rest[..index]);
        let after = &rest[index..];
        if let Some(end) = after.find(PLACEHOLDER_SUFFIX) {
            let token = &after[..end + PLACEHOLDER_SUFFIX.len()];
            if let Some(original) = state.reverse.get(token) {
                out.push_str(original);
                rest = &after[token.len()..];
                continue;
            }
        }
        // 不认识 / 不完整：保留前缀，从它之后继续找，不能吞掉正文。
        out.push_str(PLACEHOLDER_PREFIX);
        rest = &after[PLACEHOLDER_PREFIX.len()..];
    }
    out.push_str(rest);
    out
}

fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 规则脱敏器：持有编译后的规则。
///
/// 它自身实现 [`Masker`] 只是为了单测 / 单次调用；内部映射表有上限
/// （超限按插入顺序淘汰），**请求路径必须用 [`RulesMasker::session`]**，
/// 否则映射会跨请求共享（见父模块模块头）。
///
/// 规则用 `Arc<[CompiledRule]>` 持有：会话只克隆这个 Arc（引用计数 +1），
/// 于是 [`MaskSession`] 是 `'static` 的 —— 它能随请求一起跨 `await` 存活，
/// 不必挂在借用上（请求结构体无法携带生命周期参数）。
pub struct RulesMasker {
    rules: Arc<[CompiledRule]>,
    state: Mutex<State>,
}

impl Default for RulesMasker {
    fn default() -> Self {
        Self {
            rules: default_rules().to_vec().into(),
            state: Mutex::new(State::bounded(MAX_SHARED_ENTRIES)),
        }
    }
}

impl RulesMasker {
    /// 按设置选择规则族：`tokens` 控制密钥 / 令牌，`emails` 控制邮箱，
    /// `words` 是自定义词（字面匹配；优先级最低，同起点同长度时让位内置规则）。
    ///
    /// 三档互相独立是**用户可见的语义**：只开密钥脱敏时，邮箱必须原样上行
    /// （企业场景常见「留邮箱、遮 Key」）。两个开关都关且词表为空时得到一份
    /// 空规则集 —— 不脱敏任何东西，但调用方仍能拿到一个可用的脱敏器
    /// （开关本身由 `core::masking::config::active` 把关）。
    pub fn with_rules(tokens: bool, emails: bool, words: &[String]) -> Self {
        let mut rules: Vec<CompiledRule> = default_rules()
            .iter()
            .filter(|rule| match rule.kind {
                "api_key" => tokens,
                "email" => emails,
                _ => true,
            })
            .cloned()
            .collect();
        rules.extend(word_rules(words));
        Self {
            rules: rules.into(),
            state: Mutex::new(State::bounded(MAX_SHARED_ENTRIES)),
        }
    }

    /// 默认规则 + 自定义词表（两族内置规则都开）。
    pub fn with_words(words: &[String]) -> Self {
        Self::with_rules(true, true, words)
    }

    /// 每次请求拿一个：映射只属于这次请求，请求结束随 session 释放。
    ///
    /// 返回的会话拥有规则（`Arc` 克隆），不带生命周期参数 —— 见
    /// [`RulesMasker`] 的说明。
    pub fn session(&self) -> MaskSession {
        MaskSession {
            rules: Arc::clone(&self.rules),
            state: Mutex::new(State::default()),
        }
    }
}

/// 一次请求的脱敏会话：mask 与 restore 共用同一份映射。
///
/// 拥有规则（`Arc` 克隆），因此可以独立于 [`RulesMasker`] 存活 —— 出站
/// 请求把会话放进传输层、回复侧再取出来还原（见父模块模块头）。
pub struct MaskSession {
    rules: Arc<[CompiledRule]>,
    state: Mutex<State>,
}

impl Masker for RulesMasker {
    fn mask_json(&self, value: &mut Value) -> MaskReport {
        let mut state = lock_state(&self.state);
        mask_value(value, &self.rules, &mut state, false)
    }

    fn mask_text(&self, text: &str) -> (String, MaskReport) {
        let mut state = lock_state(&self.state);
        mask_text_with(&self.rules, text, &mut state)
    }

    fn restore(&self, text: &str) -> String {
        let state = lock_state(&self.state);
        restore_with(&state, text)
    }
}

impl Masker for MaskSession {
    fn mask_json(&self, value: &mut Value) -> MaskReport {
        let mut state = lock_state(&self.state);
        mask_value(value, &self.rules, &mut state, false)
    }

    fn mask_text(&self, text: &str) -> (String, MaskReport) {
        let mut state = lock_state(&self.state);
        mask_text_with(&self.rules, text, &mut state)
    }

    fn restore(&self, text: &str) -> String {
        let state = lock_state(&self.state);
        restore_with(&state, text)
    }
}

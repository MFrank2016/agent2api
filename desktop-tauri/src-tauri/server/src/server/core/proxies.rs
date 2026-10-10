//! 出网代理：账号级配置归一 / 解析 / 展示描述。
//!
//! 对照 Node 版 src/workbuddy-proxy.mjs 的「账号代理」部分（归一/解析/描述）。
//! 本模块只做**纯逻辑**：把账号里存的 proxy 配置解析成可用的连接信息。
//! Clash Verge 配置的真实读取（三个候选目录 + 两个 YAML + 3 秒 TTL 缓存）
//! 在 `core::clash`；真实出网（按出口缓存 reqwest Client）在 `core::egress`。
//! 三者拆分是因为它们的变化原因不同：本模块随**账号数据格式**变，
//! clash.rs 随**Clash 版本**变，egress.rs 随**HTTP 客户端实现**变。
//!
//! 账号记录里的 proxy 字段形态（null 表示无代理）：
//!   { source: 'clash',  listenerUid: '__mixed__' | '<Clash 节点名>' }
//!   { source: 'custom', protocol: 'http' | 'socks5', host, port, username?, password? }
//!   { source: 'pool',   proxyId: '<代理池条目 id>' }
//!
//! `pool` 是「网络代理」页那批命名代理的**引用**（见 `core::proxy_pool`）：
//! 解析时去池里取条目、再按条目自己的 clash/custom 形态解析（端口实时读取、
//! 条目被删/被禁用的失败文案在 `proxy_pool::resolve_reference`）。入口放在
//! 这里而不是让每个调用方各查一次池：账号的展示描述、会话解析、出口测试
//! 三条链走的都是本模块，加一个分支就全部生效。
//! 注意 `proxy_pool::resolve_item` 反过来调本模块 —— 两条路径**不递归**：
//! 池条目的 source 只可能是 clash / custom（归一里挡掉了嵌套引用）。

use serde_json::{json, Map, Value};

// Clash 常量与可选项从 core::clash 透出：账号模块与路由层一直从本模块引用
// 这些名字（clash_proxy_options / CLASH_MIXED_UID），保持这条路径能少改调用方。
// CLASH_UNAVAILABLE 的权威定义在 clash.rs，需要它的调用方从那里取 ——
// 这里不再二次导出，免得同一个常量有两条引用路径。
pub use crate::server::core::clash::{clash_proxy_options, CLASH_MIXED_UID};

const MAX_HOST_LENGTH: usize = 255;
const MAX_USER_LENGTH: usize = 200;
const MAX_LABEL_LENGTH: usize = 100;
const MAX_GROUP_LENGTH: usize = 60;
const MAX_ROTATION_MEMBERS: usize = 64;

/// 代理配置错误（对应 Node 版 ProxyConfigError，状态码固定 400）。
///
/// 单独一个类型而不是塞进 GatewayError：账号路由要按它给出 400，
/// 而 GatewayError 的默认语义是 500（上游/内部错误）。
#[derive(Clone, Debug)]
pub struct ProxyConfigError {
    pub message: String,
    pub status_code: i32,
}

impl ProxyConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), status_code: 400 }
    }
}

impl std::fmt::Display for ProxyConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

// ─── 归一与解析 ─────────────────────────────────────────────

/// 去空白并截断长度（对照 Node 版 `cleanString`：非字符串一律当空串）
fn clean_string(value: Option<&Value>, max: usize) -> String {
    let Some(Value::String(text)) = value else {
        return String::new();
    };
    let trimmed = text.trim();
    trimmed.chars().take(max).collect()
}

/// Node 的 `Number(value)` 语义下「是 1..65535 的整数」
fn valid_port(value: Option<&Value>) -> Option<u16> {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64()?,
        // 与 Node 的 Number('8080') 一致：数字字符串也接受
        Some(Value::String(text)) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !number.is_finite() || number.fract() != 0.0 || !(1.0..=65535.0).contains(&number) {
        return None;
    }
    Some(number as u16)
}

/// 归一账号代理配置（来自 API 的原始输入）。
///
/// 返回 `Ok(None)`（无代理）或 `Ok(Some(标准形态))`；非法输入抛 ProxyConfigError
/// （→ HTTP 400）。`source` 缺省时按字段推断，便于手工编辑账号记录
/// （库里的 `data` 列，或导出文件的 JSON）。
pub fn normalize_account_proxy(input: &Value) -> Result<Option<Value>, ProxyConfigError> {
    if input.is_null() || input.as_str() == Some("") {
        return Ok(None);
    }
    let Some(object) = input.as_object() else {
        return Err(ProxyConfigError::new("代理配置必须是对象或 null"));
    };

    let explicit_source = clean_string(object.get("source"), 20).to_lowercase();
    let has_listener = object
        .get("listenerUid")
        .and_then(Value::as_str)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false);
    let has_host = object
        .get("host")
        .and_then(Value::as_str)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false);
    let source = if !explicit_source.is_empty() {
        explicit_source
    } else if has_listener {
        "clash".to_string()
    } else if has_host {
        "custom".to_string()
    } else {
        String::new()
    };
    if source.is_empty() {
        return Err(ProxyConfigError::new("代理配置缺少 source（clash / custom）"));
    }

    if source == "clash" {
        let listener_uid = clean_string(object.get("listenerUid"), MAX_LABEL_LENGTH);
        if listener_uid.is_empty() {
            return Err(ProxyConfigError::new("缺少 Clash 监听器 uid"));
        }
        return Ok(Some(json!({ "source": "clash", "listenerUid": listener_uid })));
    }
    // 代理池引用：只存条目 id（形状校验；条目是否存在 / 是否禁用留给**解析**时
    // 报错 —— 与本模块的纯逻辑定位一致，也不让「保存账号」依赖代理池的读库）
    if source == "pool" {
        let proxy_id = clean_string(object.get("proxyId"), MAX_LABEL_LENGTH);
        if proxy_id.is_empty() {
            return Err(ProxyConfigError::new("缺少代理池条目 id"));
        }
        return Ok(Some(json!({ "source": "pool", "proxyId": proxy_id })));
    }
    if source == "pool-rotate" {
        let group = clean_string(object.get("group"), MAX_GROUP_LENGTH);
        let proxy_ids = match object.get("proxyIds") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut ids: Vec<String> = Vec::new();
                for item in items {
                    let id = clean_string(Some(item), MAX_LABEL_LENGTH);
                    if id.is_empty() {
                        return Err(ProxyConfigError::new("轮询出口的 proxyIds 含空项"));
                    }
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                if ids.len() > MAX_ROTATION_MEMBERS {
                    return Err(ProxyConfigError::new(format!("轮询出口成员过多（最多 {MAX_ROTATION_MEMBERS}）")));
                }
                ids
            }
            Some(_) => return Err(ProxyConfigError::new("轮询出口的 proxyIds 必须是数组")),
        };
        if group.is_empty() && proxy_ids.is_empty() {
            return Err(ProxyConfigError::new("轮询出口缺少成员（group 或 proxyIds）"));
        }
        let strategy = {
            let raw = clean_string(object.get("strategy"), 20).to_lowercase();
            if raw.is_empty() { Strategy::RoundRobin }
            else { Strategy::parse(&raw).ok_or_else(|| ProxyConfigError::new("轮询策略必须是 round-robin / random / least-latency"))? }
        };
        let on_error = {
            let raw = clean_string(object.get("onError"), 10).to_lowercase();
            if raw.is_empty() { OnError::Next }
            else { OnError::parse(&raw).ok_or_else(|| ProxyConfigError::new("onError 必须是 next 或 none"))? }
        };
        return Ok(Some(json!({
            "source": "pool-rotate",
            "group": group,
            "proxyIds": proxy_ids,
            "strategy": strategy.as_str(),
            "onError": on_error.as_str(),
        })));
    }
    if source != "custom" {
        return Err(ProxyConfigError::new(format!("不支持的代理来源: {source}")));
    }

    let protocol = {
        let cleaned = clean_string(object.get("protocol"), 10).to_lowercase();
        if cleaned.is_empty() { "http".to_string() } else { cleaned }
    };
    if protocol != "http" && protocol != "socks5" {
        return Err(ProxyConfigError::new("代理协议只支持 http 或 socks5"));
    }
    let host = clean_string(object.get("host"), MAX_HOST_LENGTH);
    if host.is_empty() {
        return Err(ProxyConfigError::new("缺少代理主机地址"));
    }
    let Some(port) = valid_port(object.get("port")) else {
        return Err(ProxyConfigError::new("代理端口必须是 1-65535 的整数"));
    };
    let label = clean_string(object.get("label"), MAX_LABEL_LENGTH);
    // label 缺失时按 `协议://主机:端口` 生成，前端直接展示这个串
    let label = if label.is_empty() {
        format!("{protocol}://{host}:{port}")
    } else {
        label
    };

    let mut normalized = Map::new();
    normalized.insert("source".to_string(), Value::String("custom".to_string()));
    normalized.insert("protocol".to_string(), Value::String(protocol));
    normalized.insert("host".to_string(), Value::String(host));
    normalized.insert("port".to_string(), Value::from(port));
    normalized.insert(
        "username".to_string(),
        Value::String(clean_string(object.get("username"), MAX_USER_LENGTH)),
    );
    normalized.insert(
        "password".to_string(),
        Value::String(clean_string(object.get("password"), MAX_USER_LENGTH)),
    );
    normalized.insert(
        "noReuse".to_string(),
        Value::Bool(object.get("noReuse").and_then(Value::as_bool).unwrap_or(false)),
    );
    normalized.insert("label".to_string(), Value::String(label));
    Ok(Some(Value::Object(normalized)))
}

/// 解析结果：成功时给出可用的出口，失败时 `error` 说明原因
/// （调用方据此回退直连并记日志，对应 Node 版 `{ error }` 分支）。
///
/// `port` 是 `Option<u16>`：Node 的 custom 分支直接写 `Number(config.port)`，
/// 手工编辑出的非法端口会变成 `NaN` → JSON `null`。这里如实保留那个 null，
/// 而不是伪造一个 0 —— 前端拿到 null 才知道「这条记录本来就坏」。
#[derive(Clone, Debug)]
pub struct ResolvedProxy {
    pub source: String,
    pub protocol: String,
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    pub password: String,
    pub label: String,
    /// 每请求换出口 IP：不复用连接池（代理出口每次新建隧道）。默认 false。
    pub no_reuse: bool,
}

impl ResolvedProxy {
    /// 端口的 JSON 形态（非法/缺失 → null，与 Node 的 NaN → null 一致）
    pub fn port_json(&self) -> Value {
        self.port.map(Value::from).unwrap_or(Value::Null)
    }

    /// 从 JSON 形态还原（会话里的 `proxy` 字段就是这个形态，
    /// 由 account_store 的 `session_from_record` 用 `to_json` 的成功分支写出）。
    ///
    /// 三态：`Ok(None)` = 没有代理（直连）；`Ok(Some(...))` = 可用出口；
    /// `Err(原因)` = **配了代理但数据坏了**（缺主机 / 端口非法）。
    /// 第三态必须与「没配代理」分开：手工编辑账号记录（库里 `data` 列的
    /// `proxy` 字段，或导出文件）写出 `port: "abc"` 时，Node 会带着 `NaN`
    /// 端口去建 ProxyAgent 并失败（出口测试显示「❌ 无法连接」）—— 若这里
    /// 静默按直连处理，用户会看到「✅ 出口可用」，那是在骗人。
    /// 出网侧的调用方遇到 Err 时按直连兜底（可用性优先），
    /// 但**出口测试**要把原因如实报出来。
    pub fn from_json(value: &Value) -> Result<Option<Self>, String> {
        if value.is_null() {
            return Ok(None);
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let host = text("host");
        if host.is_empty() {
            return Err("代理配置缺少主机地址".to_string());
        }
        let Some(port) = valid_port(value.get("port")) else {
            return Err("代理端口必须是 1-65535 的整数".to_string());
        };
        let protocol = {
            let protocol = text("protocol");
            if protocol.is_empty() { "http".to_string() } else { protocol }
        };
        Ok(Some(ResolvedProxy {
            source: text("source"),
            protocol,
            host,
            port: Some(port),
            username: text("username"),
            password: text("password"),
            label: text("label"),
            no_reuse: value.get("noReuse").and_then(Value::as_bool).unwrap_or(false),
        }))
    }
}

/// 轮询策略（账号出口为 `pool-rotate` 时生效）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy { RoundRobin, Random, LeastLatency }

impl Strategy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "round-robin" => Some(Self::RoundRobin),
            "random" => Some(Self::Random),
            "least-latency" => Some(Self::LeastLatency),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self { Self::RoundRobin => "round-robin", Self::Random => "random", Self::LeastLatency => "least-latency" }
    }
}

/// 连接失败时是否换下一个出口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError { Next, None }

impl OnError {
    pub fn parse(value: &str) -> Option<Self> {
        match value { "next" => Some(Self::Next), "none" => Some(Self::None), _ => None }
    }
    pub fn as_str(self) -> &'static str {
        match self { Self::Next => "next", Self::None => "none" }
    }
}

/// 轮询计划：成员 + 策略 + 失败语义 + 进程内状态键。
#[derive(Clone, Debug)]
pub struct RotationPlan {
    pub members: Vec<ResolvedProxy>,
    pub strategy: Strategy,
    pub on_error: OnError,
    /// 游标 / EWMA 的状态键（成员端点排序拼接，见 `core::egress_rotation`）。
    pub group_key: String,
}

/// 一次请求可用的出网出口（`Direct` ≡ 无代理）。
#[derive(Clone, Debug)]
pub enum AccountEgress { Direct, Single(ResolvedProxy), Rotate(RotationPlan) }

/// 出口 → JSON（与 `account_store::proxy_json` 同形）。
pub(crate) fn proxy_to_json(proxy: &ResolvedProxy) -> Value {
    json!({
        "source": proxy.source, "protocol": proxy.protocol, "host": proxy.host,
        "port": proxy.port, "username": proxy.username, "password": proxy.password, "label": proxy.label,
        "noReuse": proxy.no_reuse,
    })
}

impl AccountEgress {
    /// 从会话/计划里的 JSON 还原；畸形数据一律回退 `Direct`（可用性优先）。
    pub fn from_json(value: &Value) -> Self {
        if value.as_object().and_then(|o| o.get("source")).and_then(Value::as_str) == Some("pool-rotate") {
            let members: Vec<ResolvedProxy> = value
                .get("members").and_then(Value::as_array)
                .map(|items| items.iter().filter_map(|it| ResolvedProxy::from_json(it).ok().flatten()).collect())
                .unwrap_or_default();
            if members.is_empty() { return Self::Direct; }
            let strategy = value.get("strategy").and_then(Value::as_str).and_then(Strategy::parse).unwrap_or(Strategy::RoundRobin);
            let on_error = value.get("onError").and_then(Value::as_str).and_then(OnError::parse).unwrap_or(OnError::Next);
            let group_key = value.get("groupKey").and_then(Value::as_str).unwrap_or("").to_string();
            return Self::Rotate(RotationPlan { members, strategy, on_error, group_key });
        }
        match ResolvedProxy::from_json(value) {
            Ok(Some(proxy)) => Self::Single(proxy),
            _ => Self::Direct,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Direct => Value::Null,
            Self::Single(proxy) => proxy_to_json(proxy),
            Self::Rotate(plan) => json!({
                "source": "pool-rotate",
                "members": plan.members.iter().map(proxy_to_json).collect::<Vec<_>>(),
                "strategy": plan.strategy.as_str(),
                "onError": plan.on_error.as_str(),
                "groupKey": plan.group_key,
            }),
        }
    }

    pub fn primary(&self) -> Option<&ResolvedProxy> {
        match self {
            Self::Direct => None,
            Self::Single(proxy) => Some(proxy),
            Self::Rotate(plan) => plan.members.first(),
        }
    }
}

/// 会话里的出口（`session.proxy`）。
///
/// 账号存储组装的会话已经带上了 `proxy`（解析成功）或 `proxyError`（解析失败，
/// 此时 `proxy` 为 null）—— 所以拿到 None 就表示「直连」：没配代理、
/// 配了但解析失败（如引用了已被删掉的 Clash 监听器）、以及配了但数据坏了，
/// 这三种情况在出网层是同一个动作。第三种会额外记一条日志提醒。
pub fn session_proxy(session: &Value) -> Option<ResolvedProxy> {
    match ResolvedProxy::from_json(session.get("proxy").unwrap_or(&Value::Null)) {
        Ok(proxy) => proxy,
        Err(reason) => {
            crate::server::logging::log(
                "[Upstream]",
                &format!("⚠️ 账号代理不可用（{reason}），本次回退直连"),
            );
            None
        }
    }
}

/// JS 真值判定（`Boolean(x)`）：null/false/0/"" 为假，其余（含数组/对象）为真。
/// 用于复刻 Node 的 `config.label || ...` 这类短路取值。
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS 模板串里插值的等效渲染：`a` 缺失是 `undefined`、显式 null 是 `null`、
/// 其余按 `String(a)` 的字面量（对象/数组在 JS 里是 `[object Object]` /
/// 逗号拼接，这里用 JSON 文本近似 —— 这些形态只可能来自手工改坏的账号记录，
/// 近似的目的是「不要把值吞掉」，而不是逐字复刻 JS 的 toString）。
/// 只用于复刻 Node 拼接 label / 报错文案的行为（见 `resolve_account_proxy`）。
fn js_interpolation(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Object(_)) => "[object Object]".to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// 解析失败的原因（与成功结果互斥）
#[derive(Clone, Debug)]
pub enum ProxyResolution {
    Resolved(ResolvedProxy),
    Failed(String),
}

impl ProxyResolution {
    pub fn error(&self) -> Option<&str> {
        match self {
            ProxyResolution::Resolved(_) => None,
            ProxyResolution::Failed(message) => Some(message.as_str()),
        }
    }

    pub fn resolved(&self) -> Option<&ResolvedProxy> {
        match self {
            ProxyResolution::Resolved(proxy) => Some(proxy),
            ProxyResolution::Failed(_) => None,
        }
    }

    /// 转成 Node 版 resolveAccountProxy 的返回形态（JSON，含 error 分支）。
    /// 账号公开形态走 `describe_account_proxy`（它另带 config/label 兜底），
    /// 这条与它逐字段对应，保留供排障时直接比对解析结果。
    #[allow(dead_code)]
    pub fn to_json(&self) -> Value {
        match self {
            ProxyResolution::Resolved(proxy) => json!({
                "source": proxy.source,
                "protocol": proxy.protocol,
                "host": proxy.host,
                "port": proxy.port_json(),
                "username": proxy.username,
                "password": proxy.password,
                "label": proxy.label,
            }),
            ProxyResolution::Failed(message) => json!({ "error": message }),
        }
    }
}

/// 把账号里存的代理配置解析成可用的连接信息。
///
/// 逐字段复刻 Node 版 `resolveAccountProxy`，**包括它对脏数据的宽容**：
///   - protocol 只认 `socks5`，其余（含 undefined）都当 http
///   - host 原样透出（Node 不做字符串化，非字符串会变成 undefined → 丢失）
///   - port 走 `Number()`：非数字得 NaN → JSON null（不是 0）
///   - label 缺省时按 JS 模板串拼接，所以 `undefined://bad:undefined` 这种
///     输出是**预期行为** —— 它恰好告诉用户这条记录本身就坏（前端显示为
///     「出口 undefined://…」比显示一个编造的默认值更容易排障）
///
/// clash 类型从 Clash Verge 快照实时取端口（快照由 `core::clash` 维护，
/// 含 3 秒 TTL 缓存）；取不到时返回 Failed，调用方回退直连并提示。
pub fn resolve_account_proxy(config: Option<&Value>) -> Option<ProxyResolution> {
    let config = config?;
    if config.is_null() {
        return None;
    }
    // 非对象（字符串/数字/数组）在 JS 里取 `.source` 得 undefined，
    // 于是落到「不支持的代理来源: undefined」—— 手工编辑账号记录
    // 写错形状时就是这条文案，照抄不改成更「友好」的提示
    let Some(object) = config.as_object() else {
        return Some(ProxyResolution::Failed("不支持的代理来源: undefined".to_string()));
    };
    // source 缺失时同样是 undefined（Node 是 `config.source` 直接进模板串）。
    // 这条文案会显示在账号列表的「代理异常」气泡里，所以必须逐字一致 ——
    // 给空串会让用户看到「不支持的代理来源: 」这种像是程序坏了的提示
    let source = object
        .get("source")
        .map(|value| js_interpolation(Some(value)))
        .unwrap_or_else(|| "undefined".to_string());

    // 代理池引用：去池里取条目、按条目自己的 clash / custom 形态解析。
    // 失败文案（不存在 / 已禁用 / Clash 监听器没了）由 proxy_pool 给出，
    // 这里只做转手 —— 账号列表的「代理异常」气泡、转发时的回退日志都读它。
    if source == "pool" {
        let proxy_id = object.get("proxyId").and_then(Value::as_str).unwrap_or("");
        if proxy_id.is_empty() {
            return Some(ProxyResolution::Failed("代理引用缺少条目 id".to_string()));
        }
        return Some(
            match crate::server::core::proxy_pool::resolve_reference(proxy_id) {
                Ok(proxy) => ProxyResolution::Resolved(proxy),
                Err(reason) => ProxyResolution::Failed(reason),
            },
        );
    }

    if source == "custom" {
        let protocol = if object.get("protocol").and_then(Value::as_str) == Some("socks5") {
            "socks5"
        } else {
            "http"
        };
        // host 原样透出：非字符串时 Node 会把数字/布尔照透给 JSON，但那种值
        // 无法用作主机名，这里统一给空串 —— 调用方（session_proxy）见到空 host
        // 就回退直连并记日志，比带着 `host: 123` 去连一个好
        let host = object.get("host").and_then(Value::as_str).unwrap_or("").to_string();
        let port = valid_port(object.get("port"));
        // label 缺省时按 JS 模板串拼接（未设置的值渲染成 undefined）；
        // 显式给了真值就用它（Node 是 `config.label || \`...\``，数字/布尔这类
        // 真值也会被原样采用，这里用 js_interpolation 取同形态的文本）
        let label = match object.get("label") {
            Some(value) if js_truthy(value) => js_interpolation(Some(value)),
            _ => format!(
                "{}://{}:{}",
                js_interpolation(object.get("protocol")),
                js_interpolation(object.get("host")),
                js_interpolation(object.get("port")),
            ),
        };
        return Some(ProxyResolution::Resolved(ResolvedProxy {
            source: "custom".to_string(),
            protocol: protocol.to_string(),
            host,
            port,
            username: object
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            password: object
                .get("password")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            label,
            no_reuse: object.get("noReuse").and_then(Value::as_bool).unwrap_or(false),
        }));
    }
    if source != "clash" {
        return Some(ProxyResolution::Failed(format!("不支持的代理来源: {source}")));
    }

    let snapshot = crate::server::core::clash::clash_snapshot();
    if !snapshot.available {
        let detail = snapshot
            .error
            .as_ref()
            .map(|error| format!("（{error}）"))
            .unwrap_or_default();
        return Some(ProxyResolution::Failed(format!("Clash Verge 配置不可用{detail}")));
    }
    let listener_uid = object.get("listenerUid").and_then(Value::as_str).unwrap_or("");
    if listener_uid == CLASH_MIXED_UID {
        let Some(port) = snapshot.mixed_port else {
            return Some(ProxyResolution::Failed("Clash Verge 未启用混合端口".to_string()));
        };
        return Some(ProxyResolution::Resolved(ResolvedProxy {
            source: "clash".to_string(),
            protocol: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port: Some(port),
            username: String::new(),
            password: String::new(),
            label: format!("Clash 混合端口 {port}"),
            no_reuse: false,
        }));
    }
    let Some(listener) = snapshot.listeners.iter().find(|item| item.uid == listener_uid) else {
        return Some(ProxyResolution::Failed(format!(
            "Clash Verge 中找不到监听器「{listener_uid}」（可能已在 Clash 中删除）"
        )));
    };
    if !listener.enabled {
        return Some(ProxyResolution::Failed(format!(
            "Clash Verge 监听器「{}」已禁用",
            listener.name
        )));
    }
    Some(ProxyResolution::Resolved(ResolvedProxy {
        source: "clash".to_string(),
        protocol: "http".to_string(),
        host: "127.0.0.1".to_string(),
        port: Some(listener.port),
        username: String::new(),
        password: String::new(),
        label: format!("{}（:{}）", listener.name, listener.port),
        no_reuse: false,
    }))
}

/// 出口解析结果：成功给可用出口，失败给原因（转发层回退直连）。
#[derive(Clone, Debug)]
pub enum EgressResolution { Resolved(AccountEgress), Failed(String) }

/// 轮询计划的进程内状态键：成员端点串**排序**后拼接（`protocol://host:port`）。
///
/// 排序是为了让「同一组成员、不同书写顺序」的账号落到同一个键 —— 否则它们
/// 各自持有一份游标 / EWMA，round-robin 与延迟择优都会各转各的。
fn rotation_group_key(members: &[ResolvedProxy]) -> String {
    let mut keys: Vec<String> = members
        .iter()
        .map(|m| {
            format!(
                "{}://{}:{}",
                m.protocol,
                m.host,
                m.port.map(|p| p.to_string()).unwrap_or_default()
            )
        })
        .collect();
    keys.sort();
    keys.join("|")
}

/// 把账号里存的代理配置解析成统一出口（Direct / Single / Rotate）。
pub fn resolve_account_egress(config: Option<&Value>) -> EgressResolution {
    let Some(config) = config else { return EgressResolution::Resolved(AccountEgress::Direct) };
    if config.is_null() { return EgressResolution::Resolved(AccountEgress::Direct); }
    let Some(object) = config.as_object() else {
        return EgressResolution::Failed("不支持的代理来源: undefined".to_string());
    };
    if object.get("source").and_then(Value::as_str) == Some("pool-rotate") {
        let group = object.get("group").and_then(Value::as_str).unwrap_or("");
        let proxy_ids: Vec<String> = object
            .get("proxyIds").and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let members = crate::server::core::proxy_pool::members_for(group, &proxy_ids);
        if members.is_empty() {
            return EgressResolution::Failed("轮询组没有可用出口".to_string());
        }
        let strategy = object.get("strategy").and_then(Value::as_str).and_then(Strategy::parse).unwrap_or(Strategy::RoundRobin);
        let on_error = object.get("onError").and_then(Value::as_str).and_then(OnError::parse).unwrap_or(OnError::Next);
        let group_key = rotation_group_key(&members);
        return EgressResolution::Resolved(AccountEgress::Rotate(RotationPlan { members, strategy, on_error, group_key }));
    }
    match resolve_account_proxy(Some(config)) {
        None => EgressResolution::Resolved(AccountEgress::Direct),
        Some(ProxyResolution::Resolved(proxy)) => EgressResolution::Resolved(AccountEgress::Single(proxy)),
        Some(ProxyResolution::Failed(message)) => EgressResolution::Failed(message),
    }
}

/// 账号代理的展示描述（公开形态，不带单独字段的密码 —— 与 Node 版一致，
/// 密码只在 config 里原样带回）。
///
/// 无代理返回 Null；解析失败时 `label` 为「解析失败」并附 `error`
/// （前端据此提示「转发时会回退直连」）。
pub fn describe_account_proxy(config: Option<&Value>) -> Value {
    let Some(config) = config.filter(|value| !value.is_null()) else {
        return Value::Null;
    };
    // 轮询组（pool-rotate）走独立的出口解析：`resolve_account_proxy` 不认识
    // 这个 source，会报「不支持的代理来源」造成账号列表里的假告警。转发路径
    // （`resolve_account_egress`）本来就支持它，这里用同一套解析保证两端一致。
    if config.get("source").and_then(Value::as_str) == Some("pool-rotate") {
        return match resolve_account_egress(Some(config)) {
            EgressResolution::Resolved(AccountEgress::Rotate(plan)) => json!({
                "source": "pool-rotate",
                "label": format!("轮询组（{} 个出口）", plan.members.len()),
                "error": Value::Null,
                "config": config,
            }),
            EgressResolution::Failed(message) => json!({
                "source": "pool-rotate",
                "label": "解析失败",
                "error": message,
                "config": config,
            }),
            // resolve_account_egress 对 pool-rotate 配置只会给出 Rotate；
            // Direct/Single 在此不可达，防御性兜底。
            _ => Value::Null,
        };
    }
    let Some(resolution) = resolve_account_proxy(Some(config)) else {
        return Value::Null;
    };
    let source = config
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match &resolution {
        ProxyResolution::Failed(message) => json!({
            "source": source,
            "label": "解析失败",
            "error": message,
            "config": config,
        }),
        ProxyResolution::Resolved(proxy) => json!({
            // source 用**账号里配的那个**（pool / clash / custom）：pool 引用
            // 解析成功后底层是 clash / custom，但界面上要如实显示「引用了
            // 代理池的某条」—— 前端据此在代理列上标出来。手工改坏的记录
            // （source 缺失）才回落到解析结果的 source
            "source": if source.is_empty() { proxy.source.clone() } else { source.clone() },
            "protocol": proxy.protocol,
            "host": proxy.host,
            "port": proxy.port,
            "label": proxy.label,
            "error": Value::Null,
            "config": config,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_egress_round_trips_a_rotation_plan() {
        let plan = RotationPlan {
            members: vec![ResolvedProxy {
                source: "pool".into(), protocol: "http".into(), host: "1.2.3.4".into(),
                port: Some(8080), username: "u".into(), password: "p".into(), label: "a".into(),
                no_reuse: false,
            }],
            strategy: Strategy::LeastLatency,
            on_error: OnError::Next,
            group_key: "g".into(),
        };
        let json = AccountEgress::Rotate(plan).to_json();
        match AccountEgress::from_json(&json) {
            AccountEgress::Rotate(back) => {
                assert_eq!(back.strategy, Strategy::LeastLatency);
                assert_eq!(back.on_error, OnError::Next);
                assert_eq!(back.members.len(), 1);
                assert_eq!(back.members[0].host, "1.2.3.4");
            }
            other => panic!("expected rotate, got {other:?}"),
        }
    }

    #[test]
    fn rotation_group_key_is_order_independent() {
        let member = |host: &str, port: u16| ResolvedProxy {
            source: "pool".into(), protocol: "http".into(), host: host.into(),
            port: Some(port), username: String::new(), password: String::new(), label: String::new(),
            no_reuse: false,
        };
        // 同一组成员、两种书写顺序 → 同一个键（否则游标 / EWMA 不共享）
        let forward = rotation_group_key(&[member("1.2.3.4", 8080), member("5.6.7.8", 9090)]);
        let reversed = rotation_group_key(&[member("5.6.7.8", 9090), member("1.2.3.4", 8080)]);
        assert_eq!(forward, reversed);
        assert_eq!(forward, "http://1.2.3.4:8080|http://5.6.7.8:9090");
    }

    #[test]
    fn account_egress_from_json_falls_back_to_direct_on_garbage() {
        assert!(matches!(AccountEgress::from_json(&serde_json::json!({"source":"pool-rotate","members":[]})), AccountEgress::Direct));
        assert!(matches!(AccountEgress::from_json(&serde_json::json!(42)), AccountEgress::Direct));
    }

    #[test]
    fn normalize_pool_rotate_accepts_and_rejects() {
        let ok = normalize_account_proxy(&serde_json::json!({
            "source":"pool-rotate","group":"kilo","strategy":"least-latency","onError":"none"
        })).unwrap().unwrap();
        assert_eq!(ok["strategy"], "least-latency");
        assert_eq!(ok["onError"], "none");

        // 空成员
        assert!(normalize_account_proxy(&serde_json::json!({"source":"pool-rotate"})).is_err());
        // 非法策略
        assert!(normalize_account_proxy(&serde_json::json!({"source":"pool-rotate","group":"x","strategy":"fastest"})).is_err());
        // proxyIds 非数组
        assert!(normalize_account_proxy(&serde_json::json!({"source":"pool-rotate","proxyIds":"a"})).is_err());
    }

    #[test]
    fn resolve_egress_passes_through_single_and_reports_empty_rotation() {
        // 单出口沿用既有解析
        let single = resolve_account_egress(Some(&serde_json::json!({"source":"custom","protocol":"http","host":"1.2.3.4","port":8080})));
        assert!(matches!(single, EgressResolution::Resolved(AccountEgress::Single(_))));

        // 无配置 → Direct
        assert!(matches!(resolve_account_egress(None), EgressResolution::Resolved(AccountEgress::Direct)));

        // 轮询但池里无成员 → Failed（测试库为空）
        let rot = resolve_account_egress(Some(&serde_json::json!({"source":"pool-rotate","group":"__nope__"})));
        assert!(matches!(rot, EgressResolution::Failed(_)));
    }

    #[test]
    fn describe_account_proxy_special_cases_pool_rotate() {
        // 轮询组在池里无成员（测试库默认空）→ 走 Failed 分支：文案是
        // 「轮询组没有可用出口」，而不是旧路径的「不支持的代理来源: pool-rotate」。
        let failed = describe_account_proxy(Some(&serde_json::json!({
            "source": "pool-rotate", "group": "__nope__"
        })));
        assert_eq!(failed["source"], "pool-rotate");
        assert_eq!(failed["label"], "解析失败");
        assert_eq!(failed["error"], "轮询组没有可用出口");
        assert_ne!(failed["error"], "不支持的代理来源: pool-rotate");

        // 回归护栏：普通 custom 配置仍走既有 resolve_account_proxy 路径。
        let custom = describe_account_proxy(Some(&serde_json::json!({
            "source": "custom", "protocol": "http", "host": "1.2.3.4", "port": 8080
        })));
        assert_eq!(custom["source"], "custom");
        assert_eq!(custom["error"], Value::Null);
        assert_eq!(custom["label"], "http://1.2.3.4:8080");
    }
}

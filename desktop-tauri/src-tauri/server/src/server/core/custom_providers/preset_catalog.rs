//! 预置提供商目录（preset）：新建自定义提供商表单的**编译期**模板表。
//!
//! ── 这张表是什么、不是什么 ──────────────────────────────────
//! 预置只是**表单的预填模板**：选中某一家时把名称 / 协议 / Base URL（以及
//! 该家的上游特判、余额查询方案、图标与提示文案）填进表单，用户仍可逐项改；
//! 提交后走普通 `POST /api/custom-providers`，落库的是一条 `custom-` 前缀的
//! 普通自定义提供商。因此本模块**不读配置、不落库、不参与转发** —— 它只在
//! 「创建」那一刻被查一次（`find`），把模板值派生进请求体。
//!
//! ── 与前端清单的关系 ───────────────────────────────────────
//! 这张表的唯一事实来源原先是 `desktop-tauri/ui/preset-providers.js`（24 张
//! 卡）。服务端接管后由 `GET /api/presets` 出口，前端那份退化成读接口的薄
//! 壳；两边条目、顺序、字段取值必须逐项一致（顺序即卡片网格顺序，用户对
//! 它有肌肉记忆，**不要重排**）。
//!
//! ── 与 JS 形状的差异（照抄时别照抄错）───────────────────────
//! 逐项迁移时做了三处**形状**调整，取值不变：
//!
//!  1. `protocol` + `baseUrl` 两个平铺字段合成 `protocols: [{protocol, baseUrl}]`
//!     —— 一家以后可能同时支持多种协议，列表是给后续条目留的位置；当前每家
//!     只有一条，创建时取第一条。
//!  2. 新增 `kind`（`vendor` / `relay` / `local`）：JS 没有这个字段，它只是
//!     展示分类，不参与转发判定。
//!  3. `quirks` 从「JS 里的嵌套对象」改成**类型化的 [`PresetQuirks`]**：提供商
//!     记录本身是**平铺**的（`urlSuffix` / `headers` / `anthropicToolType` 是
//!     顶层键，见 `custom_providers` 模块头的「条目形状」），嵌套 blob 会被
//!     `item_of` 丢掉。类型化后既能逐字段 `apply_to_record` 写进记录，也能原样
//!     序列化给前端（`headers` 仍是 JSON 对象）。**`clientEmulation` 不在这里**
//!     —— 它和 JS 一样是 `PresetDef` 的顶层字段，前端读的是 `preset.clientEmulation`。
//!
//! 其余字段照 JS 原样迁移：`icon`（`assets/providers/` 下的文件名）、`hint`
//! （Base URL 栏下的备注，中文原文；前端仍走 `wbI18n.t` 翻译）、`placeholder`
//! （自建面板的输入框占位）、`no_key`（JS `account.noAuth`）、`balance`
//! （JS `balance.scheme`）。
//!
//! ── quirks 各字段的取值语义 ────────────────────────────────
//! 取值照抄 JS 表（那边有详细出处注释），**不要改值**：
//! `urlSuffix` 追加到出站 URL 的查询串；`headers` 合并到默认头；`anthropicToolType`
//! 给 anthropic 上游的工具补 `type`；`clientEmulation` 按官方 CLI 形状补齐请求。
//!
//! ── 字段口径 ───────────────────────────────────────────────
//! * `protocols`：`protocol` 取值与 [`super::PROTOCOLS`] 逐字一致；
//!   `base_url` 按本网关的拼接语义给（chat 拼 `{base}/chat/completions`，
//!   anthropic 拼 `{base}/v1/messages`）。**sub2api / newapi 的 `base_url` 是
//!   空串**：自建面板没有固定站点，由用户填（`placeholder` 是提示语）。
//! * `no_key`：表单里「该上游无需鉴权」默认勾选（OpenCode Zen 免费档、本机 Ollama）。
//! * `catalog` / `website` / `keys_url` / `header_hints`：JS 里没有对应字段，
//!   当前一律 `None` / 空；预留给「远程模型目录 / 官网 / 取 Key 页 / 头部提示」
//!   这类后续要展示的元信息。

use serde_json::{Map, Value};

/// 预置卡片的分类（仅展示用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PresetKind {
    /// 官方厂商端点。
    Vendor,
    /// 聚合面板 / 订阅制中转。
    Relay,
    /// 本机服务（Ollama）。
    Local,
}

/// 一条预填协议：协议名 + 该协议下的 Base URL。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetProtocol {
    pub protocol: &'static str,
    pub base_url: &'static str,
}

/// 该家的上游特判（**平铺**写进提供商记录，不是嵌套 blob）。
///
/// 字段与记录里的键一一对应：`url_suffix` → `urlSuffix`、`headers` → `headers`、
/// `anthropic_tool_type` → `anthropicToolType`。`clientEmulation` **不在**这里，
/// 它是 [`PresetDef::client_emulation`]（与 JS 的顶层字段一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetQuirks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_suffix: Option<&'static str>,
    /// 静态额外头；序列化成 JSON 对象（`{"k": "v"}`），与 JS 的 `headers` 同形。
    #[serde(serialize_with = "serialize_headers")]
    pub headers: &'static [(&'static str, &'static str)],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_tool_type: Option<&'static str>,
}

impl PresetQuirks {
    /// 把本特判写进提供商记录的**顶层键**（`custom_providers::create` 的入参形状）。
    ///
    /// 只写「有值」的键：空 `headers` 不落键（读侧 `item_of` 把缺省当无特判）。
    /// 调用方传入的 `object` 通常就是创建请求体。
    pub fn apply_to_record(&self, object: &mut Map<String, Value>) {
        if let Some(suffix) = self.url_suffix {
            object.insert("urlSuffix".to_string(), Value::String(suffix.to_string()));
        }
        if !self.headers.is_empty() {
            let mut headers = Map::new();
            for (key, value) in self.headers {
                headers.insert((*key).to_string(), Value::String((*value).to_string()));
            }
            object.insert("headers".to_string(), Value::Object(headers));
        }
        if let Some(tool_type) = self.anthropic_tool_type {
            object.insert(
                "anthropicToolType".to_string(),
                Value::String(tool_type.to_string()),
            );
        }
    }
}

/// `PresetQuirks::headers` 的序列化：切片按 JSON 对象输出（`[["k","v"]]` 不是
/// 前端与记录契约的形状）。
fn serialize_headers<S>(
    headers: &&'static [(&'static str, &'static str)],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut map = serializer.serialize_map(Some(headers.len()))?;
    for (key, value) in *headers {
        map.serialize_entry(key, value)?;
    }
    map.end()
}

/// 一张预置卡片。
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetDef {
    /// 稳定标识（= JS 的 `key`）：`presetId` 的取值、`presetKey` 落库值。
    pub id: &'static str,
    /// 卡片展示名（预填进表单的名称，可改）。
    pub name: &'static str,
    pub kind: PresetKind,
    /// 预填协议候选；创建时取第一条。
    pub protocols: &'static [PresetProtocol],
    /// 该上游无需鉴权（表单里「无需 API Key」默认勾选）。
    pub no_key: bool,
    /// 图标文件名（`ui/assets/providers/` 下；没有收录的家为 `None`，前端用首字母徽章）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<&'static str>,
    /// 表单 Base URL 栏下的备注文案（中文原文，前端走 `wbI18n.t`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
    /// Base URL 输入框的占位提示（自建面板没有固定地址时用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<&'static str>,
    /// 客户端形态伪装（`"opencode"` = OpenCode Zen 免费档）。与 JS 同：**顶层字段**。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_emulation: Option<&'static str>,
    /// 远程模型目录地址（预留，当前全为 `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<&'static str>,
    /// 官网（预留，当前全为 `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<&'static str>,
    /// 取 Key 的页面（预留，当前全为 `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys_url: Option<&'static str>,
    /// 预留的头部提示（当前全为空）。
    pub header_hints: &'static [(&'static str, &'static str)],
    /// 该家的上游特判（平铺写进记录，见 [`PresetQuirks`]）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quirks: Option<PresetQuirks>,
    /// 余额查询方案（= JS `balance.scheme`）；`None` = 不查余额。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<&'static str>,
}

impl PresetDef {
    /// 把这张卡的**全部**上游特判写进提供商记录：先 [`PresetQuirks::apply_to_record`]
    /// 写 `urlSuffix` / `headers` / `anthropicToolType`，再写顶层 `clientEmulation`。
    ///
    /// 创建提供商（Task 3.3 的 `presetId` 派生）应该调这个，而不是把 `quirks`
    /// 整个塞进请求体 —— 记录契约是平铺的，嵌套 blob 会被读侧丢掉。
    pub fn apply_quirks_to_record(&self, object: &mut Map<String, Value>) {
        if let Some(quirks) = self.quirks {
            quirks.apply_to_record(object);
        }
        if let Some(emulation) = self.client_emulation {
            object.insert(
                "clientEmulation".to_string(),
                Value::String(emulation.to_string()),
            );
        }
    }
}

/// 全部预置卡片，顺序即前端网格顺序（照抄 JS 表，勿重排）。
pub fn all() -> &'static [PresetDef] {
    CATALOG
}

/// 按 id 取一张卡片；未收录的 id 返回 `None`（调用方回落到手动新建）。
pub fn find(id: &str) -> Option<&'static PresetDef> {
    all().iter().find(|preset| preset.id == id)
}

// ── 特判常量（同一组头被多家复用，写一处免得改漏）─────────────────

/// Anthropic-Beta：启用 interleaved-thinking 等 beta 特性（与 JS 表同值）。
const ANTHROPIC_BETA_HEADERS: &[(&str, &str)] = &[(
    "Anthropic-Beta",
    "claude-code-20250219,interleaved-thinking-2025-05-14",
)];

/// OpenRouter 的推荐标识头（缺了不影响转发，照抄保持同款行为）。
const OPENROUTER_HEADERS: &[(&str, &str)] = &[
    ("HTTP-Referer", "https://endpoint-proxy.local"),
    ("X-Title", "Endpoint Proxy"),
];

/// Cline 客户端伪装头（抄自后端 Cline 适配器的 DEFAULT_HEADERS）。
///
/// `X-CLIENT-TYPE: cline-sdk` 是硬门槛（缺了一律 403）；**不要抄别的网关的
/// 客户端值** —— 上游认的是 Cline 客户端。
const CLINE_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "Cline/3.0.62"),
    ("HTTP-Referer", "https://cline.bot"),
    ("X-Title", "Cline"),
    ("X-IS-MULTIROOT", "false"),
    ("X-CLIENT-TYPE", "cline-sdk"),
    ("X-CLIENT-VERSION", "3.0.62"),
    ("X-PLATFORM", "terminal"),
    ("X-PLATFORM-VERSION", "3.0.62"),
    ("X-CORE-VERSION", "0.0.70"),
];

const ANTHROPIC_QUIRKS: PresetQuirks = PresetQuirks {
    url_suffix: None,
    headers: ANTHROPIC_BETA_HEADERS,
    anthropic_tool_type: None,
};

const OPENROUTER_QUIRKS: PresetQuirks = PresetQuirks {
    url_suffix: None,
    headers: OPENROUTER_HEADERS,
    anthropic_tool_type: None,
};

const GLM_QUIRKS: PresetQuirks = PresetQuirks {
    url_suffix: Some("?beta=true"),
    headers: ANTHROPIC_BETA_HEADERS,
    anthropic_tool_type: None,
};

const MINIMAX_QUIRKS: PresetQuirks = PresetQuirks {
    url_suffix: Some("?beta=true"),
    headers: ANTHROPIC_BETA_HEADERS,
    anthropic_tool_type: Some("custom"),
};

const CLINE_QUIRKS: PresetQuirks = PresetQuirks {
    url_suffix: None,
    headers: CLINE_HEADERS,
    anthropic_tool_type: None,
};

// ── 协议简写（静态提升要求字段都是 const 表达式，别用 const fn 包一层）──

const CHAT: &str = super::PROTOCOL_CHAT_COMPLETIONS;
const ANTHROPIC: &str = super::PROTOCOL_ANTHROPIC;

/// 表内条目的默认形状：只有 id / name / kind / protocols 必填，其余字段
/// 缺省即「无」（需要 Key、无特判、不查余额）。变体用结构体更新语法表达
/// （`PresetDef { no_key: true, ..preset(...) }`），与 JS 表里「只有部分家
/// 写 `account` / `quirks` / `balance`」的形状对齐。
const fn preset(
    id: &'static str,
    name: &'static str,
    kind: PresetKind,
    protocols: &'static [PresetProtocol],
) -> PresetDef {
    PresetDef {
        id,
        name,
        kind,
        protocols,
        no_key: false,
        icon: None,
        hint: None,
        placeholder: None,
        client_emulation: None,
        catalog: None,
        website: None,
        keys_url: None,
        header_hints: &[],
        quirks: None,
        balance: None,
    }
}

static CATALOG: &[PresetDef] = &[
    preset(
        "openai",
        "OpenAI",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.openai.com/v1",
        }],
    )
    .with_icon("openai.png"),
    preset(
        "anthropic",
        "Anthropic",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: ANTHROPIC,
            base_url: "https://api.anthropic.com",
        }],
    )
    .with_icon("anthropic.png")
    .with_quirks(ANTHROPIC_QUIRKS),
    preset(
        "openrouter",
        "OpenRouter",
        PresetKind::Relay,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://openrouter.ai/api/v1",
        }],
    )
    .with_icon("openrouter.png")
    .with_hint("一个 Key 用遍多家模型，模型名用「厂商/模型」全称（如 deepseek/deepseek-chat）")
    .with_quirks(OPENROUTER_QUIRKS),
    preset(
        "huggingface",
        "HuggingFace",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://router.huggingface.co/v1",
        }],
    )
    .with_icon("huggingface.png")
    .with_hint("走 Inference Providers 路由，Key 在 hf.co/settings/tokens 创建"),
    preset(
        "nvidia",
        "NVIDIA",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://integrate.api.nvidia.com/v1",
        }],
    )
    .with_icon("nvidia.png")
    .with_hint("NVIDIA NIM 托管端点，Key 在 integrate.api.nvidia.com 申请"),
    preset(
        "deepseek",
        "DeepSeek",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.deepseek.com",
        }],
    )
    .with_icon("deepseek.png")
    .with_hint(
        "Anthropic 兼容端点在 https://api.deepseek.com/anthropic（协议换 anthropic 时填它）",
    )
    .with_balance(super::BALANCE_SCHEME_DEEPSEEK),
    preset(
        "glm",
        "GLM Coding",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: ANTHROPIC,
            base_url: "https://api.z.ai/api/anthropic",
        }],
    )
    .with_icon("glm.png")
    .with_hint("国际版 z.ai 的 Coding 套餐；OpenAI 兼容地址为 https://api.z.ai/api/coding/paas/v4")
    .with_quirks(GLM_QUIRKS)
    .with_balance(super::BALANCE_SCHEME_ZHIPU_ZAI),
    preset(
        "glm-cn",
        "GLM 中国",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        }],
    )
    .with_icon("glm-cn.png")
    .with_hint("智谱 bigmodel.cn 的 Coding 套餐，Key 在 open.bigmodel.cn/usercenter/apikeys")
    .with_balance(super::BALANCE_SCHEME_ZHIPU),
    preset(
        "minimax",
        "Minimax Coding",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: ANTHROPIC,
            base_url: "https://api.minimax.io/anthropic",
        }],
    )
    .with_icon("minimax.png")
    .with_hint("国际站 Coding 套餐；OpenAI 兼容地址为 https://api.minimax.io/v1")
    .with_quirks(MINIMAX_QUIRKS),
    preset(
        "minimax-cn",
        "Minimax 中国",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: ANTHROPIC,
            base_url: "https://api.minimaxi.com/anthropic",
        }],
    )
    .with_icon("minimax-cn.png")
    .with_hint("国内站（minimaxi.com）；OpenAI 兼容地址为 https://api.minimaxi.com/v1")
    .with_quirks(MINIMAX_QUIRKS),
    preset(
        "siliconflow",
        "SiliconFlow",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.siliconflow.com/v1",
        }],
    )
    .with_icon("siliconflow.png")
    .with_hint("国内站为 https://api.siliconflow.cn/v1"),
    preset(
        "volcengine-ark",
        "火山方舟",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://ark.cn-beijing.volces.com/api/coding/v3",
        }],
    )
    .with_icon("volcengine-ark.png")
    .with_hint("火山引擎 Coding 套餐；通用推理接入点为 https://ark.cn-beijing.volces.com/api/v3"),
    preset(
        "alicode",
        "Alibaba",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://coding.dashscope.aliyuncs.com/v1",
        }],
    )
    .with_icon("alicode.png")
    .with_hint("阿里云百炼 Coding 套餐（国内站）"),
    preset(
        "alicode-intl",
        "Alibaba Coding",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://coding-intl.dashscope.aliyuncs.com/v1",
        }],
    )
    .with_icon("alicode-intl.png")
    .with_hint("阿里云百炼 Coding 套餐（国际站）"),
    preset(
        "alims-intl",
        "Alibaba Studio",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        }],
    )
    .with_icon("alims-intl.png")
    .with_hint("阿里云百炼模型服务（国际站，OpenAI 兼容模式）"),
    preset(
        "alitp-intl",
        "Alibaba Token Plan",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        }],
    )
    .with_icon("alitp-intl.png")
    .with_hint("阿里云 Token Plan 套餐（国际站）"),
    preset(
        "xiaomi-tokenplan",
        "小米 MiMo (Token Plan)",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
        }],
    )
    .with_icon("xiaomi-tokenplan.png")
    .with_hint("小米 MiMo Token Plan 套餐（新加坡站）"),
    preset(
        "opencode-go",
        "OpenCode Go",
        PresetKind::Relay,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://opencode.ai/zen/go/v1",
        }],
    )
    .with_icon("opencode-go.png")
    .with_hint(
        "同地址换 anthropic 协议即走它的 Claude 兼容端点。转发必须填 OpenCode Go 的 API Key（订阅制；获取模型清单不需要 Key，但发请求上游会回 401「Missing API key」）",
    )
    .with_balance(super::BALANCE_SCHEME_OPENCODE),
    preset(
        "opencode-zen",
        "OpenCode Zen",
        PresetKind::Relay,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://opencode.ai/inference/v1",
        }],
    )
    .with_icon("opencode-zen.png")
    .with_hint(
        "免费模型不用填 Key（默认勾上「该上游无需鉴权」，网关自动用匿名凭证）；付费模型则填 Zen 的 API Key。免费档上游只认官方客户端形态，已默认开启「伪装 OpenCode 官方客户端」",
    )
    .with_client_emulation(super::CLIENT_EMULATION_OPENCODE)
    .with_no_key(),
    preset(
        "clinepass",
        "Cline Pass",
        PresetKind::Relay,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.cline.bot/api/v1",
        }],
    )
    // 图标复用 Agent 段那张 Cline 品牌图（与内置家 cline-free / cline-pass 同一张）。
    .with_icon("cline.png")
    .with_hint(
        "Cline Pass 的 API Key 在 app.cline.bot 获取（订阅制）；端点为 OpenAI 兼容，模型清单以 Cline 控制台为准，模型名在模型管理里按裸名添加（如 glm-5.2 / kimi-k2.7-code / deepseek-v4-pro）；上游按客户端形态校验，所需的客户端请求头已随卡预置",
    )
    .with_quirks(CLINE_QUIRKS),
    preset(
        "ollama-local",
        "Ollama（本地）",
        PresetKind::Local,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "http://localhost:11434/v1",
        }],
    )
    .with_icon("ollama-local.png")
    .with_hint("本机 Ollama 的 OpenAI 兼容端点，无需 API Key（默认已勾上「该上游无需鉴权」，鉴权头一个都不发）")
    .with_no_key(),
    preset(
        "sub2api",
        "Sub2API",
        PresetKind::Relay,
        // 自建面板没有固定站点：基址留空，由用户填。
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "",
        }],
    )
    .with_icon("sub2api.png")
    .with_placeholder("https://你的 Sub2API 站点地址（OpenAI 兼容填到 /v1）")
    .with_hint(
        "自建聚合面板：填你自己的站点地址。余额查询用该账号的 API Key 打 {base}/v1/usage，不需要额外配置",
    )
    .with_balance(super::BALANCE_SCHEME_SUB2API),
    preset(
        "newapi",
        "NewAPI",
        PresetKind::Relay,
        // 同上：自建面板，基址由用户填。
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "",
        }],
    )
    .with_icon("newapi.png")
    .with_placeholder("https://你的 NewAPI 站点地址（OpenAI 兼容填到 /v1）")
    .with_hint("自建聚合面板：填你自己的站点地址；余额查询要另填 NewAPI 的个人访问令牌（见下方「余额查询」一段）")
    .with_balance(super::BALANCE_SCHEME_NEW_API),
    preset(
        "sensenova",
        "商汤日日新",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.sensenova.cn/compatible-mode/v1",
        }],
    )
    .with_icon("sensenova.png")
    .with_hint("商汤日日新的 OpenAI 兼容端点；余额查询走控制台账号密码（见下方「余额查询」一段）")
    .with_balance(super::BALANCE_SCHEME_SENSENOVA),
    preset(
        "moonshot-cn",
        "Kimi / Moonshot 国内版",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.moonshot.cn/v1",
        }],
    )
    .with_icon("kimi.svg")
    .with_hint("Kimi / Moonshot 国内站 API Key，使用 moonshot.cn 按量接口；不是 Kimi Code 会员入口"),
    preset(
        "moonshot",
        "Kimi / Moonshot 国际版",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.moonshot.ai/v1",
        }],
    )
    .with_icon("kimi.svg")
    .with_hint("Kimi / Moonshot 国际站 API Key，使用 moonshot.ai 按量接口；不是 Kimi Code 会员入口"),
    preset(
        "stepfun-cn",
        "StepFun 国内版（按量）",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.stepfun.com/v1",
        }],
    )
    .with_icon("stepfun-color.svg")
    .with_hint("StepFun 国内站按量 API；请使用按量 API Key，不要填 Step Plan 套餐 Key"),
    preset(
        "stepfun",
        "StepFun 国际版（按量）",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.stepfun.ai/v1",
        }],
    )
    .with_icon("stepfun-color.svg")
    .with_hint("StepFun 国际站按量 API；请使用按量 API Key，不要填 Step Plan 套餐 Key"),
    preset(
        "longcat",
        "LongCat",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.longcat.chat/openai/v1",
        }],
    )
    .with_icon("longcat.svg")
    .with_hint("LongCat 的 OpenAI 兼容端点，使用 LongCat 平台的 API Key"),
    preset(
        "tencent-tokenhub-cn",
        "腾讯 TokenHub 国内版（按量）",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://tokenhub.tencentmaas.com/v1",
        }],
    )
    .with_icon("tencentcloud-color.svg")
    .with_hint("腾讯 TokenHub 国内站按量 API；使用 TokenHub API Key，不是 Token Plan 套餐或 WorkBuddy 凭证"),
    preset(
        "tencent-tokenhub",
        "腾讯 TokenHub 国际版（按量）",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://tokenhub-intl.tencentmaas.com/v1",
        }],
    )
    .with_icon("tencentcloud-color.svg")
    .with_hint("腾讯 TokenHub 国际站按量 API；使用 TokenHub API Key，不是 Token Plan 套餐或 WorkBuddy 凭证"),
    preset(
        "xai",
        "xAI",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.x.ai/v1",
        }],
    )
    .with_icon("xai.svg")
    .with_hint("xAI 的 OpenAI 兼容端点，使用 xAI 控制台的 API Key；不是 Grok 网页订阅"),
    preset(
        "mistral",
        "Mistral",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.mistral.ai/v1",
        }],
    )
    .with_icon("mistral-color.svg")
    .with_hint("Mistral 的 OpenAI 兼容端点，使用 Mistral 控制台的 API Key"),
    preset(
        "groq",
        "Groq",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.groq.com/openai/v1",
        }],
    )
    .with_icon("groq.svg")
    .with_hint("Groq 的 OpenAI 兼容端点，使用 GroqCloud 的 API Key"),
    preset(
        "nous",
        "Nous Portal",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://inference-api.nousresearch.com/v1",
        }],
    )
    .with_icon("nous-portal.svg")
    .with_hint("Nous Portal 的按量 API，使用 Portal 的 API Key；不是旧的订阅登录方式"),
    preset(
        "aihubmix",
        "AiHubMix",
        PresetKind::Relay,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://aihubmix.com/v1",
        }],
    )
    .with_icon("aihubmix-color.svg")
    .with_hint("AiHubMix 的 OpenAI 兼容端点，使用 AiHubMix 的 API Key；模型 ID 以平台清单为准"),
    preset(
        "lmstudio",
        "LM Studio（本地）",
        PresetKind::Local,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "http://localhost:1234/v1",
        }],
    )
    .with_icon("lmstudio.svg")
    .with_hint("先在网关所在机器启动 LM Studio 服务；localhost 指向网关所在机器，远程或容器部署请改为网关可访问的地址。默认无需鉴权；启用认证后请取消「该上游无需鉴权」并填写真实 API Key")
    .with_no_key(),
    preset(
        "xiaomi",
        "小米 MiMo（按量）",
        PresetKind::Vendor,
        &[PresetProtocol {
            protocol: CHAT,
            base_url: "https://api.xiaomimimo.com/v1",
        }],
    )
    .with_icon("xiaomi-tokenplan.png")
    .with_hint("小米 MiMo 按量 API；使用按量 API Key，不要混用 Token Plan 套餐 Key"),
];

/// 逐项设置可选字段的小工具。
///
/// 表是 `static`（编译期常量），可选字段只能靠 const fn 的链式调用逐项填 ——
/// 直接写 `PresetDef { .. }` 结构体更新在这里不可用（`..base` 要求 base 是
/// 运行期值）。每个 `with_*` 都是 `const fn`，返回值仍是常量。
impl PresetDef {
    const fn with_icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    const fn with_hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }

    const fn with_placeholder(mut self, placeholder: &'static str) -> Self {
        self.placeholder = Some(placeholder);
        self
    }

    const fn with_client_emulation(mut self, emulation: &'static str) -> Self {
        self.client_emulation = Some(emulation);
        self
    }

    const fn with_quirks(mut self, quirks: PresetQuirks) -> Self {
        self.quirks = Some(quirks);
        self
    }

    const fn with_balance(mut self, balance: &'static str) -> Self {
        self.balance = Some(balance);
        self
    }

    const fn with_no_key(mut self) -> Self {
        self.no_key = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_38_presets() {
        assert_eq!(all().len(), 38);
        assert!(find("openai").is_some());
        assert!(find("opencode-zen").unwrap().no_key);
    }

    /// 顺序即卡片网格顺序（前端有肌肉记忆），逐项照抄 JS 的 `key` 序列。
    #[test]
    fn preset_ids_and_order_match_js() {
        let ids: Vec<&str> = all().iter().map(|preset| preset.id).collect();
        assert_eq!(
            ids,
            vec![
                "openai",
                "anthropic",
                "openrouter",
                "huggingface",
                "nvidia",
                "deepseek",
                "glm",
                "glm-cn",
                "minimax",
                "minimax-cn",
                "siliconflow",
                "volcengine-ark",
                "alicode",
                "alicode-intl",
                "alims-intl",
                "alitp-intl",
                "xiaomi-tokenplan",
                "opencode-go",
                "opencode-zen",
                "clinepass",
                "ollama-local",
                "sub2api",
                "newapi",
                "sensenova",
                "moonshot-cn",
                "moonshot",
                "stepfun-cn",
                "stepfun",
                "longcat",
                "tencent-tokenhub-cn",
                "tencent-tokenhub",
                "xai",
                "mistral",
                "groq",
                "nous",
                "aihubmix",
                "lmstudio",
                "xiaomi",
            ]
        );
    }

    #[test]
    fn openai_and_anthropic_protocols() {
        let openai = find("openai").unwrap();
        assert_eq!(openai.protocols.len(), 1);
        assert_eq!(openai.protocols[0].protocol, "chat_completions");
        assert_eq!(openai.protocols[0].base_url, "https://api.openai.com/v1");

        let anthropic = find("anthropic").unwrap();
        assert_eq!(anthropic.protocols[0].protocol, "anthropic");
        assert_eq!(anthropic.protocols[0].base_url, "https://api.anthropic.com");
    }

    /// Zen 的两条预置事实：无需 Key、开着 OpenCode 客户端伪装。
    ///
    /// 伪装开关是**顶层** `clientEmulation`（前端读 `preset.clientEmulation`），
    /// 不是 `quirks` 里的字段 —— 见模块头「与 JS 形状的差异」。
    #[test]
    fn opencode_zen_is_no_key_and_emulates_client() {
        let zen = find("opencode-zen").unwrap();
        assert!(zen.no_key);
        assert_eq!(zen.client_emulation, Some("opencode"));
        assert!(zen.quirks.is_none());
    }

    #[test]
    fn local_preset_is_ollama_local() {
        let local = find("ollama-local").unwrap();
        assert_eq!(local.kind, PresetKind::Local);
        assert_eq!(local.protocols[0].base_url, "http://localhost:11434/v1");
        assert!(local.no_key);
    }

    #[test]
    fn find_unknown_returns_none() {
        assert!(find("no-such-preset").is_none());
        assert!(find("").is_none());
    }

    /// 自建面板没有固定站点：base_url 必须是空串（由用户填，见 `placeholder`）。
    #[test]
    fn self_hosted_presets_have_empty_base_url() {
        for id in ["sub2api", "newapi"] {
            let preset = find(id).unwrap();
            assert_eq!(preset.protocols[0].base_url, "");
            assert!(preset.placeholder.is_some(), "{id} 应有输入框占位提示");
        }
    }

    /// 图标 / 提示文案从 JS 迁移过来（Task 3.4 删掉 JS 后只能靠接口）。
    #[test]
    fn icons_and_hints_are_migrated() {
        assert_eq!(find("clinepass").unwrap().icon, Some("cline.png"));
        assert_eq!(find("openai").unwrap().icon, Some("openai.png"));
        assert_eq!(find("anthropic").unwrap().icon, Some("anthropic.png"));
        assert!(all().iter().all(|preset| preset.icon.is_some()));
        assert!(all()
            .iter()
            .any(|preset| preset.hint.is_some_and(|hint| !hint.is_empty())));
        assert_eq!(
            find("openrouter").unwrap().hint,
            Some("一个 Key 用遍多家模型，模型名用「厂商/模型」全称（如 deepseek/deepseek-chat）")
        );
    }

    /// 特判必须写成提供商记录的**平铺顶层键**（嵌套 blob 会被 `item_of` 丢掉）。
    #[test]
    fn quirks_apply_to_record_writes_flat_keys() {
        // Cline Pass：只有 headers（X-CLIENT-TYPE 是上游硬门槛）。
        let mut cline = Map::new();
        find("clinepass")
            .unwrap()
            .apply_quirks_to_record(&mut cline);
        assert_eq!(cline["headers"]["X-CLIENT-TYPE"], "cline-sdk");
        assert_eq!(cline["headers"]["User-Agent"], "Cline/3.0.62");
        assert!(cline.get("urlSuffix").is_none());

        // OpenCode Zen：伪装开关是顶层 clientEmulation。
        let mut zen = Map::new();
        find("opencode-zen")
            .unwrap()
            .apply_quirks_to_record(&mut zen);
        assert_eq!(zen["clientEmulation"], "opencode");
        assert!(zen.get("quirks").is_none());

        // GLM Coding：urlSuffix + Anthropic-Beta。
        let mut glm = Map::new();
        find("glm").unwrap().apply_quirks_to_record(&mut glm);
        assert_eq!(glm["urlSuffix"], "?beta=true");
        assert_eq!(
            glm["headers"]["Anthropic-Beta"],
            "claude-code-20250219,interleaved-thinking-2025-05-14"
        );
        assert!(glm.get("anthropicToolType").is_none());

        // Minimax：urlSuffix + Anthropic-Beta + anthropicToolType。
        let mut minimax = Map::new();
        find("minimax")
            .unwrap()
            .apply_quirks_to_record(&mut minimax);
        assert_eq!(minimax["urlSuffix"], "?beta=true");
        assert_eq!(minimax["anthropicToolType"], "custom");

        // 没有特判的家一个键都不写。
        let mut openai = Map::new();
        find("openai").unwrap().apply_quirks_to_record(&mut openai);
        assert!(openai.is_empty());
    }

    /// 序列化出口就是 `GET /api/presets` 的形状：camelCase、`headers` 是 JSON
    /// 对象、`clientEmulation` 在顶层、缺省的可选字段不出现（与 JS 同形）。
    #[test]
    fn serializes_as_camel_case_json() {
        let value = serde_json::to_value(find("glm").unwrap()).unwrap();
        assert_eq!(value["id"], "glm");
        assert_eq!(value["name"], "GLM Coding");
        assert_eq!(value["kind"], "vendor");
        assert_eq!(value["noKey"], false);
        assert_eq!(value["icon"], "glm.png");
        assert_eq!(value["protocols"][0]["protocol"], "anthropic");
        assert_eq!(
            value["protocols"][0]["baseUrl"],
            "https://api.z.ai/api/anthropic"
        );
        assert_eq!(value["balance"], "zhipu_zai");
        assert_eq!(value["quirks"]["urlSuffix"], "?beta=true");
        assert_eq!(
            value["quirks"]["headers"]["Anthropic-Beta"],
            "claude-code-20250219,interleaved-thinking-2025-05-14"
        );
        assert!(value.get("clientEmulation").is_none());

        let zen = serde_json::to_value(find("opencode-zen").unwrap()).unwrap();
        assert_eq!(zen["clientEmulation"], "opencode");
        assert!(zen.get("quirks").is_none());
    }
}

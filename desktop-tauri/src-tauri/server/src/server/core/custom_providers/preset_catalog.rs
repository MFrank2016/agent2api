//! 预置提供商目录（preset）：新建自定义提供商表单的**编译期**模板表。
//!
//! ── 这张表是什么、不是什么 ──────────────────────────────────
//! 预置只是**表单的预填模板**：选中某一家时把名称 / 协议 / Base URL（以及
//! 该家的上游特判、余额查询方案）填进表单，用户仍可逐项改；提交后走普通
//! `POST /api/custom-providers`，落库的是一条 `custom-` 前缀的普通自定义
//! 提供商。因此本模块**不读配置、不落库、不参与转发** —— 它只在「创建」
//! 那一刻被查一次（`find`），把模板值派生进请求体。
//!
//! ── 与前端清单的关系 ───────────────────────────────────────
//! 这张表的唯一事实来源原先是 `desktop-tauri/ui/preset-providers.js`（24 张
//! 卡）。服务端接管后由 `GET /api/presets` 出口，前端那份退化成读接口的
//! 薄壳；两边条目、顺序、字段取值必须逐项一致（顺序即卡片网格顺序，用户对
//! 它有肌肉记忆，**不要重排**）。
//!
//! ── quirks 为什么是 `Option<&'static Value>` ────────────────
//! 特判对象的形状是 JS 里那份嵌套 JSON（`urlSuffix` / `headers` /
//! `anthropicToolType` / `clientEmulation`），而 `serde_json::Value` 不是
//! const 可构造的，`PresetDef` 又要保持 `Copy`（表按 `&'static [PresetDef]`
//! 共享，不克隆）。取舍：
//!
//!   * 表整体放在 `OnceLock<Vec<PresetDef>>` 里，进程内只构造一次；
//!   * 带特判的 7 家在构造时用 [`leaked`] 把 `json!` 值 `Box::leak` 成
//!     `&'static Value` —— 每个对象几十字节、进程生命周期，泄漏量恒定且
//!     只发生一次，换来「字段类型就是 JSON 值」：序列化到 `/api/presets`
//!     的形状与 JS 原样一致，创建时也能直接取值展开进记录。
//!
//! 备选（每条一个 `OnceLock<Value>` 静态 + getter）要多写十几个符号、读起来
//! 更绕；备选（存 JSON 文本字符串）会让序列化出口变成「字符串里套 JSON」，
//! 前端还得再解析一次 —— 都不如一次性泄漏这 7 个小对象直接。
//!
//! ── 字段口径 ───────────────────────────────────────────────
//! * `kind`：`Vendor` 官方厂商 / `Relay` 聚合与订阅制中转 / `Local` 本机
//!   服务（当前只有 Ollama）。它只是展示分类，不参与转发判定。
//! * `protocols`：JS 的 `protocol` + `baseUrl` 折算成一条。目前每家只有一种
//!   预填协议（表单里可改）；`responses` 协议与多协议候选留给后续条目。
//! * `no_key`：JS `account.noAuth === true`，即表单里「该上游无需鉴权」默认
//!   勾选（OpenCode Zen 免费档、本机 Ollama）。
//! * `catalog` / `website` / `keys_url` / `header_hints`：JS 里没有对应字段，
//!   当前一律 `None` / 空；预留给「远程模型目录 / 官网 / 取 Key 页 / 头部
//!   提示」这类后续要展示的元信息。
//! * `quirks`：原样照抄 JS 的 `quirks` 对象（含 Anthropic-Beta、OpenRouter
//!   推荐头、Cline 客户端伪装头、MiniMax 的 `anthropicToolType`、OpenCode
//!   Zen 的 `clientEmulation`）。取值语义见 JS 文件头的长注释，**不要改值**。
//! * `balance`：JS `balance.scheme`，取值与 [`super::BALANCE_SCHEMES`] 逐字
//!   一致；`None` = 这家不查余额。

use std::sync::OnceLock;

use serde_json::{json, Value};

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
///
/// `protocol` 取值与 [`super::PROTOCOLS`] 逐字一致（`chat_completions` /
/// `responses` / `anthropic`），`base_url` 按本网关的拼接语义给（chat 拼
/// `{base}/chat/completions`，anthropic 拼 `{base}/v1/messages`）。
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetProtocol {
    pub protocol: &'static str,
    pub base_url: &'static str,
}

/// 一张预置卡片。
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetDef {
    /// 稳定标识（= JS 的 `key`）：`presetId` 的取值、图标名、`presetKey` 落库值。
    pub id: &'static str,
    /// 卡片展示名（预填进表单的名称，可改）。
    pub name: &'static str,
    pub kind: PresetKind,
    /// 预填协议候选；创建时取第一条。
    pub protocols: &'static [PresetProtocol],
    /// 该上游无需鉴权（表单里「无需 API Key」默认勾选）。
    pub no_key: bool,
    /// 远程模型目录地址（预留，当前全为 `None`）。
    pub catalog: Option<&'static str>,
    /// 官网（预留，当前全为 `None`）。
    pub website: Option<&'static str>,
    /// 取 Key 的页面（预留，当前全为 `None`）。
    pub keys_url: Option<&'static str>,
    /// 表单 Base URL 栏下的备注：该家另一端协议的地址 / 站点差异说明。
    pub header_hints: &'static [(&'static str, &'static str)],
    /// 该家的上游特判（形状同 JS 的 `quirks` 对象）。
    pub quirks: Option<&'static Value>,
    /// 余额查询方案（= JS `balance.scheme`）；`None` = 不查余额。
    pub balance: Option<&'static str>,
}

/// 全部预置卡片，顺序即前端网格顺序（照抄 JS 表，勿重排）。
pub fn all() -> &'static [PresetDef] {
    static CATALOG: OnceLock<Vec<PresetDef>> = OnceLock::new();
    CATALOG.get_or_init(build).as_slice()
}

/// 按 id 取一张卡片；未收录的 id 返回 `None`（调用方回落到手动新建）。
pub fn find(id: &str) -> Option<&'static PresetDef> {
    all().iter().find(|preset| preset.id == id)
}

/// 把 `json!` 值提升成 `&'static Value`（只在本模块的 `OnceLock` 初始化里
/// 调用，每张带特判的卡各一次，见模块头「quirks 为什么是 …」）。
fn leaked(value: Value) -> &'static Value {
    Box::leak(Box::new(value))
}

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
        catalog: None,
        website: None,
        keys_url: None,
        header_hints: &[],
        quirks: None,
        balance: None,
    }
}

fn build() -> Vec<PresetDef> {
    // Anthropic-Beta：启用 interleaved-thinking 等 beta 特性（与 JS 表同值）。
    const ANTHROPIC_BETA: &str = "claude-code-20250219,interleaved-thinking-2025-05-14";
    // `chat_completions` 协议名（与 `custom_providers::PROTOCOLS` 逐字一致）。
    const CHAT: &str = super::PROTOCOL_CHAT_COMPLETIONS;
    // `anthropic` 协议名（同上）。
    const ANTHROPIC: &str = super::PROTOCOL_ANTHROPIC;

    vec![
        preset(
            "openai",
            "OpenAI",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://api.openai.com/v1",
            }],
        ),
        PresetDef {
            quirks: Some(leaked(json!({
                "headers": { "Anthropic-Beta": ANTHROPIC_BETA },
            }))),
            ..preset(
                "anthropic",
                "Anthropic",
                PresetKind::Vendor,
                &[PresetProtocol {
                    protocol: ANTHROPIC,
                    base_url: "https://api.anthropic.com",
                }],
            )
        },
        PresetDef {
            quirks: Some(leaked(json!({
                "headers": {
                    "HTTP-Referer": "https://endpoint-proxy.local",
                    "X-Title": "Endpoint Proxy",
                },
            }))),
            ..preset(
                "openrouter",
                "OpenRouter",
                PresetKind::Relay,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "https://openrouter.ai/api/v1",
                }],
            )
        },
        preset(
            "huggingface",
            "HuggingFace",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://router.huggingface.co/v1",
            }],
        ),
        preset(
            "nvidia",
            "NVIDIA",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://integrate.api.nvidia.com/v1",
            }],
        ),
        PresetDef {
            balance: Some(super::BALANCE_SCHEME_DEEPSEEK),
            ..preset(
                "deepseek",
                "DeepSeek",
                PresetKind::Vendor,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "https://api.deepseek.com",
                }],
            )
        },
        PresetDef {
            quirks: Some(leaked(json!({
                "urlSuffix": "?beta=true",
                "headers": { "Anthropic-Beta": ANTHROPIC_BETA },
            }))),
            balance: Some(super::BALANCE_SCHEME_ZHIPU_ZAI),
            ..preset(
                "glm",
                "GLM Coding",
                PresetKind::Vendor,
                &[PresetProtocol {
                    protocol: ANTHROPIC,
                    base_url: "https://api.z.ai/api/anthropic",
                }],
            )
        },
        PresetDef {
            balance: Some(super::BALANCE_SCHEME_ZHIPU),
            ..preset(
                "glm-cn",
                "GLM 中国",
                PresetKind::Vendor,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
                }],
            )
        },
        PresetDef {
            quirks: Some(leaked(json!({
                "urlSuffix": "?beta=true",
                "headers": { "Anthropic-Beta": ANTHROPIC_BETA },
                "anthropicToolType": "custom",
            }))),
            ..preset(
                "minimax",
                "Minimax Coding",
                PresetKind::Vendor,
                &[PresetProtocol {
                    protocol: ANTHROPIC,
                    base_url: "https://api.minimax.io/anthropic",
                }],
            )
        },
        PresetDef {
            quirks: Some(leaked(json!({
                "urlSuffix": "?beta=true",
                "headers": { "Anthropic-Beta": ANTHROPIC_BETA },
                "anthropicToolType": "custom",
            }))),
            ..preset(
                "minimax-cn",
                "Minimax 中国",
                PresetKind::Vendor,
                &[PresetProtocol {
                    protocol: ANTHROPIC,
                    base_url: "https://api.minimaxi.com/anthropic",
                }],
            )
        },
        preset(
            "siliconflow",
            "SiliconFlow",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://api.siliconflow.com/v1",
            }],
        ),
        preset(
            "volcengine-ark",
            "火山方舟",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://ark.cn-beijing.volces.com/api/coding/v3",
            }],
        ),
        preset(
            "alicode",
            "Alibaba",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://coding.dashscope.aliyuncs.com/v1",
            }],
        ),
        preset(
            "alicode-intl",
            "Alibaba Coding",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://coding-intl.dashscope.aliyuncs.com/v1",
            }],
        ),
        preset(
            "alims-intl",
            "Alibaba Studio",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            }],
        ),
        preset(
            "alitp-intl",
            "Alibaba Token Plan",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            }],
        ),
        preset(
            "xiaomi-tokenplan",
            "小米 MiMo (Token Plan)",
            PresetKind::Vendor,
            &[PresetProtocol {
                protocol: CHAT,
                base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
            }],
        ),
        PresetDef {
            balance: Some(super::BALANCE_SCHEME_OPENCODE),
            ..preset(
                "opencode-go",
                "OpenCode Go",
                PresetKind::Relay,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "https://opencode.ai/zen/go/v1",
                }],
            )
        },
        PresetDef {
            no_key: true,
            quirks: Some(leaked(json!({ "clientEmulation": "opencode" }))),
            ..preset(
                "opencode-zen",
                "OpenCode Zen",
                PresetKind::Relay,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "https://opencode.ai/inference/v1",
                }],
            )
        },
        PresetDef {
            quirks: Some(leaked(json!({
                "headers": {
                    "User-Agent": "Cline/3.0.62",
                    "HTTP-Referer": "https://cline.bot",
                    "X-Title": "Cline",
                    "X-IS-MULTIROOT": "false",
                    "X-CLIENT-TYPE": "cline-sdk",
                    "X-CLIENT-VERSION": "3.0.62",
                    "X-PLATFORM": "terminal",
                    "X-PLATFORM-VERSION": "3.0.62",
                    "X-CORE-VERSION": "0.0.70",
                },
            }))),
            ..preset(
                "clinepass",
                "Cline Pass",
                PresetKind::Relay,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "https://api.cline.bot/api/v1",
                }],
            )
        },
        PresetDef {
            no_key: true,
            ..preset(
                "ollama-local",
                "Ollama（本地）",
                PresetKind::Local,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "http://localhost:11434/v1",
                }],
            )
        },
        PresetDef {
            balance: Some(super::BALANCE_SCHEME_SUB2API),
            ..preset(
                "sub2api",
                "Sub2API",
                PresetKind::Relay,
                // 自建面板没有固定站点：基址留空，由用户填。
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "",
                }],
            )
        },
        PresetDef {
            balance: Some(super::BALANCE_SCHEME_NEW_API),
            ..preset(
                "newapi",
                "NewAPI",
                PresetKind::Relay,
                // 同上：自建面板，基址由用户填。
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "",
                }],
            )
        },
        PresetDef {
            balance: Some(super::BALANCE_SCHEME_SENSENOVA),
            ..preset(
                "sensenova",
                "商汤日日新",
                PresetKind::Vendor,
                &[PresetProtocol {
                    protocol: CHAT,
                    base_url: "https://api.sensenova.cn/compatible-mode/v1",
                }],
            )
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_24_presets() {
        assert_eq!(all().len(), 24);
        assert!(find("openai").is_some());
        assert!(find("opencode-zen").unwrap().no_key);
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<&str> = all().iter().map(|preset| preset.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "preset id 必须唯一");
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

    #[test]
    fn opencode_zen_is_no_key_and_has_quirks() {
        let zen = find("opencode-zen").unwrap();
        assert!(zen.no_key);
        let quirks = zen
            .quirks
            .expect("OpenCode Zen 必须带 clientEmulation 特判");
        assert_eq!(quirks["clientEmulation"], "opencode");
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

    /// 序列化出口就是 `GET /api/presets` 的形状：quirks 必须是 JSON 对象
    /// （而不是字符串），字段名走 camelCase。
    #[test]
    fn serializes_as_camel_case_json() {
        let value = serde_json::to_value(find("glm").unwrap()).unwrap();
        assert_eq!(value["id"], "glm");
        assert_eq!(value["name"], "GLM Coding");
        assert_eq!(value["kind"], "vendor");
        assert_eq!(value["noKey"], false);
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
    }
}

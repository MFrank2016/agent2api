//! 出站脱敏配置：`GET/PUT /api/masking`。
//!
//! ── 这条端点管什么 ──────────────────────────────────────────
//! 三项设置，全部允许部分更新（不给 = 不动）：
//!   - `maskSecrets`（默认关）：把 `sk-…` / `ghp_…` 等密钥换成占位符；
//!   - `maskPii`（默认关）：把邮箱换成占位符；
//!   - `maskWords`（默认空）：自定义词表，按字面匹配；超长词由规则层跳过。
//!
//! 三档互相独立：只开密钥就只盖密钥，邮箱原样上行。词表非空本身也算开启。
//! 真正改写 / 还原请求与回复的实现见 `core::masking`；本文件只是设置的
//! 读写入口。
//!
//! ── 为什么与 /api/sanitize 分成两条端点 ─────────────────────
//! `/api/sanitize` 是**出站指纹脱敏**（改写发送副本、回复不还原，用于规避
//! 上游对固定文案的识别）；这里是**可逆脱敏**（请求侧换占位符、回复侧还原）。
//! 两者的开关、词表与失败语义都不同（后者 fail-closed：脱敏失败就不发），
//! 合成一条端点会让「关掉指纹脱敏」顺带影响隐私脱敏。
//!
//! ── 校验口径 ────────────────────────────────────────────────
//! 写侧是 400 报错（用户就站在这里，类型写错要当场知道）；读侧
//! （`config::mask_from`）是回落默认 —— 手改库写坏一项不该让网关起不来。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::server::config::{self, MaskPatch, KEY_MASK_PII, KEY_MASK_SECRETS, KEY_MASK_WORDS};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

pub async fn get_masking(State(_state): State<ServerState>) -> Response {
    ok_json(masking_json())
}

pub async fn put_masking(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let patch = match parse_patch(object) {
        Ok(patch) => patch,
        Err(message) => return errors::management_error(400, message),
    };
    let saved = config::set_mask_settings(patch);
    if !saved {
        logging::log("[Config]", "⚠️  出站脱敏设置写入失败，本次运行内仍生效");
    }
    let settings = config::mask_settings();
    logging::log(
        "[Config]",
        &format!(
            "出站脱敏设置已更新：密钥={} / 个人信息={} / 自定义词={} 个",
            if settings.secrets { "开" } else { "关" },
            if settings.pii { "开" } else { "关" },
            settings.words.len()
        ),
    );
    let mut result = masking_json();
    result["saved"] = Value::Bool(saved);
    ok_json(result)
}

/// 解析部分更新入参（`null` / 缺失 = 该项不动）。
fn parse_patch(object: &Map<String, Value>) -> Result<MaskPatch, String> {
    if !object.contains_key(KEY_MASK_SECRETS)
        && !object.contains_key(KEY_MASK_PII)
        && !object.contains_key(KEY_MASK_WORDS)
    {
        return Err(format!(
            "请提供 {KEY_MASK_SECRETS}、{KEY_MASK_PII} 或 {KEY_MASK_WORDS}"
        ));
    }
    Ok(MaskPatch {
        secrets: parse_bool(object, KEY_MASK_SECRETS)?,
        pii: parse_bool(object, KEY_MASK_PII)?,
        words: parse_words(object)?,
    })
}

fn parse_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key} 必须是布尔值")),
    }
}

/// 词表：`null` / 缺失 = 不动；数组里必须是字符串；去空白、去重、保序。
///
/// 超长词不在这里拒绝 —— `RulesMasker` 会在编译规则时跳过它（配置来源
/// 不可信，宁可少盖一个词也不能让正则编译失败带走进程）。
fn parse_words(object: &Map<String, Value>) -> Result<Option<Vec<String>>, String> {
    let Some(value) = object.get(KEY_MASK_WORDS) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let items = value
        .as_array()
        .ok_or_else(|| format!("{KEY_MASK_WORDS} 必须是字符串数组"))?;
    let mut words: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let Some(word) = item.as_str() else {
            return Err(format!("{KEY_MASK_WORDS} 的元素必须是字符串"));
        };
        let word = word.trim();
        if word.is_empty() || words.iter().any(|seen| seen == word) {
            continue;
        }
        words.push(word.to_string());
    }
    Ok(Some(words))
}

/// GET 与 PUT 共用的响应体（键名对前端是契约）。
fn masking_json() -> Value {
    let settings = config::mask_settings();
    json!({
        KEY_MASK_SECRETS: settings.secrets,
        KEY_MASK_PII: settings.pii,
        KEY_MASK_WORDS: settings.words,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    #[test]
    fn partial_patch_keeps_missing_items_untouched() {
        let patch = parse_patch(&object(json!({ "maskPii": true }))).unwrap_or_default();
        assert_eq!(patch.pii, Some(true));
        assert_eq!(patch.secrets, None);
        assert!(patch.words.is_none());
    }

    #[test]
    fn words_are_trimmed_deduped_and_ordered() {
        let patch = parse_patch(&object(json!({
            "maskWords": ["  Project Falcon ", "", "Project Falcon", "a@b.com"]
        })))
        .unwrap_or_default();
        assert_eq!(
            patch.words,
            Some(vec!["Project Falcon".to_string(), "a@b.com".to_string()])
        );
    }

    #[test]
    fn null_and_empty_patches_are_rejected_consistently() {
        // 一个已知键都没给 → 400（与 /api/sanitize 同一取向：空 PUT 是误用）
        assert!(parse_patch(&object(json!({ "other": 1 }))).is_err());
        // 已知键给 null = 该项不动，但整体仍算一次合法请求
        let patch = parse_patch(&object(json!({
            "maskSecrets": null,
            "maskPii": null,
            "maskWords": null
        })))
        .unwrap_or_default();
        assert_eq!(patch.secrets, None);
        assert_eq!(patch.pii, None);
        assert!(patch.words.is_none());
    }

    #[test]
    fn wrong_types_are_rejected() {
        assert!(parse_patch(&object(json!({ "maskSecrets": "true" }))).is_err());
        assert!(parse_patch(&object(json!({ "maskWords": "secret" }))).is_err());
        assert!(parse_patch(&object(json!({ "maskWords": [42] }))).is_err());
    }

    #[test]
    fn an_empty_word_list_clears_the_table() {
        let patch = parse_patch(&object(json!({ "maskWords": [] }))).unwrap_or_default();
        assert_eq!(patch.words, Some(Vec::new()));
    }
}

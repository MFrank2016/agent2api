//! LobsterAI 凭证：accessToken / refreshToken + keyfrom 信封字段 + 设备 id。
//!
//! ── 落盘键名（通用键优先，别改名）──────────────────────────────
//! ```text
//!   accessToken     上游 access token（通用键：has_token() / 续期框架都认它）
//!   refreshToken    上游 refresh token（通用键；续期主凭证）
//!   expiresAt       access token 过期时刻（毫秒）
//!   userId          身份键（上游 user.userId，可选）
//!   deviceId        设备 id（等价于信封里的 uuid；账号级稳定）
//!   firstKeyfrom    首次使用时间戳（毫秒；信封的 firstKeyfrom）
//!   latestKeyfrom   最近使用时间戳（毫秒；信封的 latestKeyfrom）
//!   tokenTail       refresh token 尾 4 位（展示用）
//! ```
//! `accessToken` 用通用键名，是为了让通用的「有没有凭证」判定（`has_token()`）
//! 与凭证维护任务原样可用；chat 发出的 Bearer 就是它。
//!
//! ── keyfrom 是什么 ──────────────────────────────────────────
//! 上游把 `firstKeyfrom` / `latestKeyfrom` 当作「这把 key 的生命周期」证据
//! （毫秒时间戳），exchange / refresh / models 三处都要随信封上行。首次添加时
//! 两者都取当前时刻；此后每次成功续期把 `latestKeyfrom` 推到当时。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::errors::GatewayError;

use super::endpoints;

/// token 临期窗口（过期前 5 分钟视为需要刷新；与各家同值）
pub const REFRESH_WINDOW_MS: i64 = 5 * 60 * 1000;

/// 一次转发 / 目录刷新 / 签到用的凭证快照。
#[derive(Clone, Debug)]
pub struct LobsterAiCredentials {
    /// 账号记录 id
    pub id: String,
    /// 展示名
    pub name: String,
    /// 上游 access token（chat 的 Bearer）
    pub access_token: String,
    /// 上游 refresh token（续期主凭证）
    pub refresh_token: String,
    /// access token 过期时刻（毫秒；未知为 0）
    pub expires_at: i64,
    /// 身份键（userId；可选）
    pub user_id: String,
    /// 设备 id（信封的 uuid；账号级稳定）
    pub device_id: String,
    /// 首次使用时间戳（毫秒）
    pub first_keyfrom: i64,
    /// 最近使用时间戳（毫秒）
    pub latest_keyfrom: i64,
}

impl LobsterAiCredentials {
    /// 这份凭证此刻是否临期（需要主动刷新）。没有过期信息（0）时不判临期。
    pub fn expiring(&self) -> bool {
        let now = crate::server::logging::now_ms();
        self.expires_at > 0 && self.expires_at - now < REFRESH_WINDOW_MS
    }

    /// 有没有可续期的 refresh token。
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }

    /// keyfrom 信封（exchange / refresh / models 三处共用）。
    ///
    /// `include_refresh` 为 true 时带上 `refreshToken`（refresh 接口用）。
    pub fn keyfrom_json(&self, include_refresh: bool) -> Value {
        let mut payload = json!({
            "firstKeyfrom": self.first_keyfrom,
            "latestKeyfrom": self.latest_keyfrom,
            "version": endpoints::PROTOCOL_VERSION,
        });
        if let Some(object) = payload.as_object_mut() {
            if !self.device_id.is_empty() {
                object.insert("uuid".to_string(), Value::String(self.device_id.clone()));
            }
            if !self.user_id.is_empty() {
                object.insert("userId".to_string(), Value::String(self.user_id.clone()));
            }
            if include_refresh {
                object.insert(
                    "refreshToken".to_string(),
                    Value::String(self.refresh_token.clone()),
                );
            }
        }
        payload
    }
}

/// 从取值链里挑第一个非空字符串（记录可能被手工编辑过，宽容读取）
fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 会话里 `auth.accessToken`（转发链交给适配器的会话形态）
fn auth_access_token(record: &Value) -> String {
    record
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 从取值链里挑第一个非零整数
fn pick_i64(record: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(value) = record.get(*key).and_then(Value::as_i64) {
            if value > 0 {
                return value;
            }
        }
    }
    0
}

/// 从账号记录 / 会话解析凭证（`record` 为 None 时报「没有可用账号」）。
pub fn from_record(record: Option<&Value>) -> Result<LobsterAiCredentials, GatewayError> {
    let Some(record) = record else {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 LobsterAI 账号：请在账号页粘贴令牌或走浏览器登录添加",
        ));
    };
    let access_token = pick(record, &["accessToken", "access_token"]);
    let access_token = if access_token.is_empty() {
        auth_access_token(record)
    } else {
        access_token
    };
    let refresh_token = pick(record, &["refreshToken", "refresh_token"]);
    let now = crate::server::logging::now_ms();
    let first = pick_i64(record, &["firstKeyfrom"]);
    let latest = pick_i64(record, &["latestKeyfrom"]);
    Ok(LobsterAiCredentials {
        id: pick(record, &["id"]),
        name: pick(record, &["name"]),
        access_token,
        refresh_token,
        expires_at: pick_i64(record, &["expiresAt"]),
        user_id: pick(record, &["userId"]),
        device_id: pick(record, &["deviceId", "uuid"]),
        first_keyfrom: if first > 0 { first } else { now },
        latest_keyfrom: if latest > 0 { latest } else { now },
    })
}

/// 展示用的脱敏形态（只露尾 4 位；日志与报错文案都用它）
pub fn masked(secret: &str) -> String {
    let chars: Vec<char> = secret.trim().chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("…{tail}")
}

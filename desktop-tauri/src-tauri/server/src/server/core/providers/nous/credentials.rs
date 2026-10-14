//! Nous 凭证：Portal 令牌（accessToken / refreshToken）＋ 推理用 agent key，
//! 以及匿名免费档的 JWT（无账号时）。
//!
//! ── 两种凭证形态（会话里怎么区分）────────────────────────────
//! 适配器只拿到会话（`account_store::store::session_from_record` 或
//! `provider_loop` 现造的匿名最小会话），因此判别只依赖会话里的字段：
//!   - **账号形态**：记录属于本家（`provider == "nous"`）或会话带
//!     `nousAccount` 标记 / 顶层有 `agentKey`。转发用 **agent key**，
//!     基址取 `inferenceBaseUrl`（缺省 `DEFAULT_INFERENCE_BASE_URL`）；
//!   - **匿名形态**：`provider_loop` 在「一个账号都没有」时现造的最小会话
//!     （只有 `auth.accessToken`），token 就是匿名 JWT，基址恒为免费面
//!     `WELCOME_BASE_URL`。
//!
//! ── 落盘键名（与别家统一的那些，别改名）──────────────────────
//! ```text
//!   accessToken    Portal access token（通用键：`has_token()` / 续期框架都认它）
//!   refreshToken   Portal refresh token（通用键）
//!   expiresAt      Portal access token 过期时刻（毫秒）
//!   agentKey       推理用 API Key（**本家专属键**，chat 的 Bearer）
//!   agentKeyExpiresAt  agent key 过期时刻（毫秒）
//!   inferenceBaseUrl   上游给的付费推理基址（可选）
//! ```
//! `accessToken` 用通用键名，是为了让通用的「有没有凭证」判定与维护任务
//! 原样可用；chat 真正发出的 Bearer 是 `agentKey`（见 `from_record`）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::server::errors::GatewayError;

use super::endpoints;

/// token 临期窗口（过期前 5 分钟视为需要刷新；与 `auth_http::REFRESH_WINDOW_MS` 同值）
pub const REFRESH_WINDOW_MS: i64 = 5 * 60 * 1000;

/// 一次转发用的凭证快照。
#[derive(Clone, Debug)]
pub struct NousCredentials {
    /// 账号记录 id（匿名为空串）
    pub id: String,
    /// 展示名
    pub name: String,
    /// 是否匿名免费档（true 时 `token` 是匿名 JWT、`base_url` 是免费面）
    pub anonymous: bool,
    /// 用于 `Authorization: Bearer` 的凭证（账号 = agent key；匿名 = 匿名 JWT）
    /// 用于 `Authorization: Bearer` 的凭证（账号 = agent key；匿名 = 匿名 JWT）
    pub token: String,
    /// Portal access token（账号形态专用；续期 / 铸 key 时用它，匿名为空串）
    pub portal_access_token: String,
    /// 推理基址（账号 = 付费面；匿名 = 免费面）
    pub base_url: String,
    /// Portal refresh token（匿名为空串）
    pub refresh_token: String,
    /// Portal access token 过期时刻（毫秒；未知为 0）
    pub expires_at: i64,
    /// agent key 过期时刻（毫秒；未知为 0）
    pub agent_key_expires_at: i64,
}

impl NousCredentials {
    /// 这份凭证此刻是否临期（需要主动刷新）。
    ///
    /// 判据取**两者更早的那一个**：Portal access token 过期就铸不出新 key，
    /// agent key 过期就发不出请求 —— 任一临期都该刷。没有过期信息（0）的那
    /// 一半不参与判定（与各家「判不出来就不判」的口径一致）。
    pub fn expiring(&self) -> bool {
        if self.anonymous {
            return false;
        }
        let now = crate::server::logging::now_ms();
        let portal_expiring = self.expires_at > 0 && self.expires_at - now < REFRESH_WINDOW_MS;
        let agent_expiring =
            self.agent_key_expires_at > 0 && self.agent_key_expires_at - now < REFRESH_WINDOW_MS;
        portal_expiring || agent_expiring
    }

    /// 有没有可续期的 Portal refresh token。
    pub fn can_refresh(&self) -> bool {
        !self.anonymous && !self.refresh_token.trim().is_empty()
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

/// 会话里 `auth.accessToken`（匿名最小会话只有它）
fn auth_access_token(record: &Value) -> String {
    record
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 会话是不是**账号形态**（见模块头）。
fn is_account(record: &Value) -> bool {
    if record.get("provider").and_then(Value::as_str) == Some(super::PROVIDER_ID) {
        return true;
    }
    if record.get("nousAccount").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    !pick(record, &["agentKey"]).is_empty()
}

/// 从账号记录 / 会话解析凭证（`record` 为 None 时报「没有可用账号」）。
///
/// 账号形态读 `agentKey` + `inferenceBaseUrl`；匿名形态读 `auth.accessToken`
/// 并强制走免费面（见模块头）。
pub fn from_record(record: Option<&Value>) -> Result<NousCredentials, GatewayError> {
    let Some(record) = record else {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 Nous 账号：请在账号页用「Nous Portal 登录」添加，或直接以匿名免费档调用",
        ));
    };
    if is_account(record) {
        let agent_key = pick(record, &["agentKey"]);
        if agent_key.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "Nous 账号缺少推理凭证（agent key）：请点「刷新 Token」重新铸一把，或重新登录",
            ));
        }
        let base = pick(record, &["inferenceBaseUrl"]);
        let base = if base.is_empty() {
            endpoints::default_inference_base()
        } else {
            base
        };
        return Ok(NousCredentials {
            id: pick(record, &["id"]),
            name: pick(record, &["name"]),
            anonymous: false,
            token: agent_key,
            portal_access_token: pick(record, &["accessToken", "portalAccessToken"]),
            base_url: base,
            refresh_token: pick(record, &["refreshToken", "refresh_token"]),
            expires_at: record
                .get("expiresAt")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            agent_key_expires_at: record
                .get("agentKeyExpiresAt")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        });
    }
    // 匿名形态：免费面 + 匿名 JWT
    let jwt = auth_access_token(record);
    if jwt.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 Nous 账号，且未能取得匿名免费档凭证：请稍后重试或检查出网",
        ));
    }
    Ok(NousCredentials {
        id: String::new(),
        name: "Nous 匿名免费档".to_string(),
        anonymous: true,
        token: jwt,
        portal_access_token: String::new(),
        base_url: endpoints::welcome_base(),
        refresh_token: String::new(),
        expires_at: 0,
        agent_key_expires_at: 0,
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

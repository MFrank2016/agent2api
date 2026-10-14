//! Nous 账号：Portal 设备码登录落账号 + 刷新回写 + 公开形态。
//!
//! ── 账号字段（落盘形态）────────────────────────────────────────
//! ```text
//!   id                派生（见下）
//!   provider          "nous"
//!   name              备注名（可空；界面兜底用尾号）
//!   accessToken       Portal access token（通用键；续期框架 / has_token 认它）
//!   refreshToken      Portal refresh token（通用键；续期主凭证）
//!   expiresAt         Portal access token 过期时刻（毫秒）
//!   agentKey          推理用 Bearer（**本家专属键**）= OAuth access token
//!   agentKeyExpiresAt Bearer 过期时刻（毫秒，= access token 过期）
//!   inferenceBaseUrl  上游给的付费推理基址（可选；缺省用默认付费面）
//!   userId            身份键（可选；见下）
//!   tokenTail         refresh token 尾 4 位（展示用；**凭证本体不进公开形态**）
//!   source/priority/enabled/addedAt/updatedAt  与别家同形
//! ```
//!
//! ── 账号 id 怎么派生 ────────────────────────────────────────
//! 身份键顺序是 **userId / sub / email / orgId → refresh token 摘要**：
//!   - 有稳定身份（Portal 若回 `user_id` / 令牌 `sub`）时按它派生：重新登录
//!     更新同一条记录，不产生孤儿账号；
//!   - 都没有时退到 refresh token 摘要：同一枚令牌再登一次仍命中同一条记录
//!     （重新授权拿到新令牌会新增一条 —— 那时用户自己知道换了一枚，手工删旧即可，
//!     与 `antigravity_accounts` 同一取舍）。
//! 取 `sha256("nous:account-id:v1\0" + 身份)[..12]`：确定性、不可反推令牌、
//! 不同身份撞 id 的概率可忽略。
//!
//! 本文件全是「读-改-写」存储操作，**没有任何网络请求**（持锁不做网络）。
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::nous::credentials;

use super::priority::next_free_priority;
use super::sql;
use super::state::{mark_name_custom, StoredAccount};
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{token_tail_of, truncate_chars};
use super::{CredentialWrite, NOUS_PROVIDER_ID};
use crate::server::logging;

/// 备注名长度上限（与别家同一口径）
const MAX_NAME_LENGTH: usize = 100;

/// 令牌长度上限
const MAX_TOKEN_LENGTH: usize = 8192;

/// 从 payload 里取第一个非空字符串（候选键覆盖本项目落盘名与登录 / 粘贴形态）
fn pick(payload: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = payload
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

/// 身份键（有稳定身份时用，否则空串 → 退到令牌摘要）
fn identity_of(payload: &Value) -> String {
    pick(payload, &["userId", "sub", "email", "orgId", "org_id"])
}

/// 身份 + 令牌 → 稳定的账号 id（见模块头）
fn account_id_for(identity: &str, refresh_token: &str, access_token: &str) -> String {
    let seed = if !identity.is_empty() {
        identity
    } else if !refresh_token.is_empty() {
        refresh_token
    } else {
        access_token
    };
    let mut hasher = Sha256::new();
    hasher.update(b"nous:account-id:v1\0");
    hasher.update(seed.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let head = digest.get(..12).unwrap_or(digest.as_str());
    format!("nous-{head}")
}

impl AccountStore {
    /// 取一条 Nous 账号记录（公开形态的原始 JSON）。
    ///
    /// `account_id` 非空 → 按 id 直查（不属于本家返回 None）；为空 → 本家
    /// **启用中**账号里优先级最靠前的一条（转发与目录刷新默认用队首账号）。
    pub fn nous_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == NOUS_PROVIDER_ID).then(|| record.to_value());
        }
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&guard, NOUS_PROVIDER_ID)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    /// 添加/更新一个 Nous 账号（登录与「填写凭证」的唯一落账号入口）。
    ///
    /// payload（登录任务的落盘载荷，或界面直接传来的形状）：
    ///   - `accessToken` / `access_token`：Portal access token（至少要有它或 refreshToken）；
    ///   - `refreshToken` / `refresh_token`：Portal refresh token（续期主凭证）；
    ///   - `expiresAt`：access token 过期时刻（毫秒，可选）；
    ///   - `agentKey` / `apiKey`：推理用 Bearer（= OAuth access token；登录写入）；
    ///   - `agentKeyExpiresAt`：agent key 过期时刻（毫秒，可选）；
    ///   - `inferenceBaseUrl`：付费推理基址（可选）；
    ///   - `userId` / `email` / `orgId`：身份（可选）。
    ///
    /// 同一身份的记录就地更新（保留优先级与启用状态、沿用用户改过的备注名）；
    /// 撞到**别的 provider** 的 id 时报错，绝不覆写。
    pub fn add_nous_account(
        &self,
        payload: &Value,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let access_token = pick(payload, &["accessToken", "access_token"]);
        let refresh_token = pick(payload, &["refreshToken", "refresh_token"]);
        if access_token.is_empty() && refresh_token.is_empty() {
            return Err(AccountStoreError::new(
                "缺少凭证（accessToken / refreshToken）",
                400,
            ));
        }
        if access_token.chars().count() > MAX_TOKEN_LENGTH
            || refresh_token.chars().count() > MAX_TOKEN_LENGTH
        {
            return Err(AccountStoreError::new("令牌过长", 400));
        }
        let agent_key = pick(payload, &["agentKey", "apiKey"]);
        let identity = identity_of(payload);
        let id = account_id_for(&identity, &refresh_token, &access_token);

        let guard = self.guard();
        let existing = self.record_by_id(&guard, &id);
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != NOUS_PROVIDER_ID {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{existing_provider}账号占用，无法添加同一个身份\
                         的 Nous 账号（请先处理那个账号）"
                    ),
                    400,
                ));
            }
        }
        // 备注名兜底：显式传入 → 既有记录里的备注名 → 「Nous <尾号>」
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_LENGTH));
        let tail_source = if refresh_token.is_empty() {
            &access_token
        } else {
            &refresh_token
        };
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| format!("Nous {}", token_tail_of(tail_source)));
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self.with_conn(&guard, |conn| sql::priorities_all(conn))?;
                next_free_priority(&used)
            }
        };
        let now = logging::now_ms();
        let mut record = Map::new();
        record.insert("id".to_string(), Value::String(id.clone()));
        record.insert(
            "provider".to_string(),
            Value::String(NOUS_PROVIDER_ID.to_string()),
        );
        record.insert("name".to_string(), Value::String(record_name.clone()));
        mark_name_custom(
            &mut record,
            name.is_some_and(|value| !value.trim().is_empty()),
            existing.as_ref(),
        );
        if !access_token.is_empty() {
            record.insert("accessToken".to_string(), Value::String(access_token.clone()));
        }
        if !refresh_token.is_empty() {
            record.insert(
                "refreshToken".to_string(),
                Value::String(refresh_token.clone()),
            );
        }
        record.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(tail_source)),
        );
        if !agent_key.is_empty() {
            record.insert("agentKey".to_string(), Value::String(agent_key.clone()));
        }
        if let Some(expires) = payload.get("expiresAt").and_then(Value::as_i64).filter(|v| *v > 0) {
            record.insert("expiresAt".to_string(), Value::from(expires));
        }
        if let Some(expires) = payload
            .get("agentKeyExpiresAt")
            .and_then(Value::as_i64)
            .filter(|v| *v > 0)
        {
            record.insert("agentKeyExpiresAt".to_string(), Value::from(expires));
        }
        let base = pick(payload, &["inferenceBaseUrl", "inference_base_url"]);
        if !base.is_empty() {
            record.insert("inferenceBaseUrl".to_string(), Value::String(base));
        }
        if !identity.is_empty() {
            record.insert("userId".to_string(), Value::String(identity.clone()));
        }
        record.insert("source".to_string(), Value::String(source.to_string()));
        record.insert("priority".to_string(), Value::from(priority));
        record.insert(
            "enabled".to_string(),
            Value::Bool(
                existing
                    .as_ref()
                    .map(StoredAccount::enabled)
                    .unwrap_or(true),
            ),
        );
        record.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .filter(|value| *value != 0)
                    .unwrap_or(now),
            ),
        );
        record.insert("updatedAt".to_string(), Value::from(now));
        // 未知字段全量保留（用户手工加过的字段不能因为一次「更新账号」丢掉）
        let mut merged = record;
        if let Some(existing) = existing.as_ref() {
            for (key, value) in existing.fields() {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let saved = StoredAccount::from_map(merged);
        self.with_conn(&guard, |conn| sql::put(conn, &saved))?;
        logging::log(
            "[Accounts]",
            &format!(
                "✅ Nous 账号已保存: {record_name}（优先级 {priority}，refresh token {}）",
                credentials::masked(&refresh_token)
            ),
        );
        Ok(self.public_account(&saved))
    }

    /// 刷新成功后回写新凭证（`oauth::refresh_and_save` 调用）。
    ///
    /// ── 比较-再写（与另外几家同一纪律）─────────────────────────
    /// 只当记录里**此刻的** refresh token 仍等于刷新前那份快照时才写入；
    /// 不一致（用户重新登录 / 换号 / 另一轮刷新先落地）返回
    /// [`CredentialWrite::Stale`] —— 调用方改用最新快照，绝不把旧结果当成功。
    ///
    /// `None` / 空值一律**保留旧值**（见调用点的说明）。
    #[allow(clippy::too_many_arguments)]
    pub fn update_nous_credentials_if_current(
        &self,
        id: &str,
        expected_refresh_token: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at: i64,
        agent_key: Option<&str>,
        agent_key_expires_at: i64,
        inference_base_url: Option<&str>,
    ) -> Result<CredentialWrite, String> {
        let guard = self.guard();
        let Some(mut record) = self.record_by_id(&guard, id) else {
            return Ok(CredentialWrite::Stale);
        };
        if record.provider() != NOUS_PROVIDER_ID {
            return Err(format!("账号 {id} 不是 Nous 账号"));
        }
        if record.refresh_token() != expected_refresh_token {
            return Ok(CredentialWrite::Stale);
        }
        let access_token = access_token.trim();
        if !access_token.is_empty() {
            record.set("accessToken", Value::String(access_token.to_string()));
        }
        if let Some(refresh) = refresh_token.map(str::trim).filter(|text| !text.is_empty()) {
            record.set("refreshToken", Value::String(refresh.to_string()));
            record.set("tokenTail", Value::String(token_tail_of(refresh)));
        }
        if let Some(key) = agent_key.map(str::trim).filter(|text| !text.is_empty()) {
            record.set("agentKey", Value::String(key.to_string()));
        }
        if let Some(base) = inference_base_url.map(str::trim).filter(|text| !text.is_empty()) {
            record.set("inferenceBaseUrl", Value::String(base.to_string()));
        }
        if expires_at > 0 {
            record.set("expiresAt", Value::from(expires_at));
        }
        if agent_key_expires_at > 0 {
            record.set("agentKeyExpiresAt", Value::from(agent_key_expires_at));
        }
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))
            .map_err(|error| error.message)?;
        Ok(CredentialWrite::Written)
    }

    /// Nous 账号的**公开形态**（进 HTTP 响应）。
    ///
    /// 与别家同一条硬约束：**绝不透出令牌 / agent key 本体** —— 只给
    /// `tokenTail`（refresh token 尾 4 位）与三个存在性标记；`expiresAt`、
    /// `agentKeyExpiresAt`、`inferenceBaseUrl`、`userId` 是展示 / 排障需要的事实。
    pub fn to_nous_public_account(&self, record: &StoredAccount) -> Value {
        let refresh_token = record.refresh_token();
        let mut public = Map::new();
        for key in [
            "id",
            "provider",
            "name",
            "userId",
            "inferenceBaseUrl",
            "source",
            "tokenTail",
        ] {
            public.insert(
                key.to_string(),
                record.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        public.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(!refresh_token.trim().is_empty()),
        );
        public.insert(
            "hasAccessToken".to_string(),
            Value::Bool(
                record
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty()),
            ),
        );
        public.insert(
            "hasAgentKey".to_string(),
            Value::Bool(
                record
                    .get("agentKey")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty()),
            ),
        );
        public.insert(
            "expiresAt".to_string(),
            Value::from(record.expires_at().unwrap_or(0.0) as i64),
        );
        public.insert(
            "agentKeyExpiresAt".to_string(),
            Value::from(
                record
                    .get("agentKeyExpiresAt")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            ),
        );
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
        );
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| json!({})),
        );
        // 可用 = 有续期主凭证（refresh token 是本家唯一的自动续期手段）
        public.insert(
            "available".to_string(),
            Value::Bool(!refresh_token.trim().is_empty()),
        );
        Value::Object(public)
    }
}

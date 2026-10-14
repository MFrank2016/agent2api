//! Nous Portal OAuth：设备码登录（RFC 8628）+ 令牌续期 + 单飞刷新与比较再写。
//! OAuth access_token 本身即推理 Bearer（官方 hermes：`agent_key = access_token`）。
//!
//! ── 身份链（规格 A）──────────────────────────────────────────
//! ```text
//!   ① POST {portal}/api/oauth/device/code   (form) client_id=hermes-cli
//!        scope=inference:invoke
//!      → { device_code, user_code, verification_uri, verification_uri_complete,
//!          expires_in, interval }
//!   ② 用户在浏览器打开 verification_uri_complete 完成授权
//!   ③ POST {portal}/api/oauth/token  (form) grant_type=…device_code, client_id,
//!        device_code
//!      → authorization_pending | slow_down（interval+5，上限 30）|
//!        { access_token, refresh_token, token_type, expires_in, scope,
//!          inference_base_url? }
//! ```
//! `access_token`（`scope` 含 `inference:invoke`）就是推理 Bearer —— 上游已退役
//! `/api/oauth/agent-key` 铸 key 端点，不再单独铸 key。
//!
//! ── 续期（规格 A.3）─────────────────────────────────────────
//! `grant_type=refresh_token` 打同一个 token 端点，换新的 access_token
//! （上游可能同时轮换 refresh_token —— 给了就覆盖）。
//!
//! ── 单飞 + 比较再写（与 Antigravity / OfficeAce 同一纪律）──────
//! 同一凭证的并发刷新只发一次真实请求（`refresh_flight`）；回写走
//! `update_nous_credentials_if_current`（刷新前那份 refresh token 仍一致才写，
//! 否则 [`CredentialWrite::Stale`]）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::proxies::{ProxyResolution, ResolvedProxy};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, NousCredentials};
use super::{endpoints, client_for};

/// 单次请求超时
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// slow_down 时的间隔增量（秒）与上限（规格 A.2：interval+5，上限 30）
const SLOW_DOWN_STEP_SEC: i64 = 5;
const MAX_INTERVAL_SEC: i64 = 30;

/// 单飞表（键 = 账号文件 + 账号 id + refresh_token 指纹，见 [`ensure_fresh`]）
static FLIGHTS: OnceLock<Table<NousCredentials>> = OnceLock::new();

/// 账号级出口代理（解析失败按 400 报，不静默直连）—— 与 Antigravity / Trae 同名函数同一语义。
pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match crate::server::core::proxies::resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

// ─── 设备码登录 ──────────────────────────────────────────────

/// 一条已发起的设备码登录流程。
pub struct DeviceLogin {
    device_code: String,
    user_code: String,
    auth_url: String,
    expires_at: i64,
    interval_sec: i64,
}

impl DeviceLogin {
    /// 发起设备码登录（登录时无账号上下文 ⇒ 直连）。
    pub async fn start() -> Result<Self, GatewayError> {
        let form = [
            ("client_id", endpoints::CLIENT_ID),
            ("scope", endpoints::SCOPE),
        ];
        let (status, ok, payload) = post_form(
            None,
            &endpoints::portal_url(endpoints::DEVICE_CODE_PATH),
            &form,
        )
        .await?;
        if !ok {
            return Err(device_error(status, &payload, "Nous 设备码申请被拒"));
        }
        let device_code = required_str(&payload, "device_code", "Nous 设备码申请")?;
        let user_code = payload
            .get("user_code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let verification_uri = payload
            .get("verification_uri")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let verification_complete = payload
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let auth_url = verification_complete.unwrap_or_else(|| verification_uri.clone());
        if auth_url.is_empty() {
            return Err(GatewayError::with_status(
                502,
                "Nous 设备码响应缺少授权地址（verification_uri）",
            ));
        }
        let expires_in = payload.get("expires_in").and_then(Value::as_i64).unwrap_or(0);
        let interval_sec = payload
            .get("interval")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .unwrap_or(5);
        let now_sec = logging::now_ms() / 1000;
        Ok(Self {
            device_code,
            user_code,
            auth_url,
            expires_at: if expires_in > 0 { now_sec + expires_in } else { 0 },
            interval_sec,
        })
    }

    /// 交给用户打开的授权地址
    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }

    /// 任务表关联标识（`user_code` 派生；不含 device_code 本体）
    pub fn state(&self) -> String {
        if self.user_code.is_empty() {
            format!("nous-{}", logging::now_ms())
        } else {
            format!("nous-{}", self.user_code)
        }
    }

    /// 轮询间隔
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.interval_sec.max(1) as u64)
    }

    /// 上游给的截止时间（unix 秒；`0` 表示上游没给）
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }

    /// 轮询一次：`Ok(None)` = 还没授权完（继续等），`Ok(Some(..))` = 拿到令牌。
    pub async fn poll(&mut self) -> Result<Option<TokenSet>, GatewayError> {
        let form = [
            ("grant_type", endpoints::GRANT_TYPE_DEVICE),
            ("client_id", endpoints::CLIENT_ID),
            ("device_code", self.device_code.as_str()),
        ];
        let (status, ok, payload) = post_form(
            None,
            &endpoints::portal_url(endpoints::TOKEN_PATH),
            &form,
        )
        .await?;
        if ok {
            return Ok(Some(parse_token_set(&payload)?));
        }
        let error = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match error.as_str() {
            "authorization_pending" => Ok(None),
            "slow_down" => {
                // 规格 A.2：interval+5，上限 30
                self.interval_sec = (self.interval_sec + SLOW_DOWN_STEP_SEC).min(MAX_INTERVAL_SEC);
                Ok(None)
            }
            "expired_token" => Err(GatewayError::with_status(
                401,
                "Nous 设备码已过期，请重新发起登录",
            )),
            "access_denied" => Err(GatewayError::with_status(
                401,
                "Nous 授权被拒绝，请重新发起登录",
            )),
            _ => Err(device_error(status, &payload, "Nous 令牌轮询失败")),
        }
    }
}

// ─── 令牌续期 ────────────────────────────────────────────────

/// 用 refresh token 换新的令牌（规格 A.3）。
pub async fn refresh_token(
    refresh_token: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<TokenSet, GatewayError> {
    let form = [
        ("grant_type", endpoints::GRANT_TYPE_REFRESH),
        ("client_id", endpoints::CLIENT_ID),
        ("refresh_token", refresh_token),
    ];
    let (status, ok, payload) = post_form(
        proxy,
        &endpoints::portal_url(endpoints::TOKEN_PATH),
        &form,
    )
    .await?;
    if !ok {
        return Err(device_error(status, &payload, "Nous 令牌续期失败"));
    }
    parse_token_set(&payload)
}

// ─── 单飞刷新（ensure_fresh）────────────────────────────────

/// 取可用账号凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
///
/// `account_id` 为空 = 本家的队首可用账号（点名的账号取不到直接报错，
/// 不回落队首 —— 那会变成「点名了 A、用了 B」）。
pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<NousCredentials, GatewayError> {
    let record = read_record(store, account_id)?;
    let credentials = credentials::from_record(Some(&record))?;
    if credentials.anonymous {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 Nous 账号（匿名免费档不走账号续期）",
        ));
    }
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "Nous 账号缺少 refresh token，无法续期：请重新登录",
        ));
    }
    let key = format!(
        "{}:{}:{}",
        store.file_string(),
        credentials.id,
        refresh_flight::fingerprint(&credentials.refresh_token)
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(store, &record, &credentials, force).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 打一次刷新 + 回写（单飞的 leader 走这一段）。
async fn refresh_and_save(
    store: &AccountStore,
    record: &Value,
    credentials: &NousCredentials,
    force: bool,
) -> Result<NousCredentials, GatewayError> {
    let proxy = account_proxy(record)?;
    let now = logging::now_ms();
    // ① Portal access token：强制刷、或已临期（或没有过期信息）时换新
    let portal_stale = credentials.portal_access_token.trim().is_empty()
        || credentials.expires_at == 0
        || credentials.expires_at - now < credentials::REFRESH_WINDOW_MS;
    let mut portal_access_token = credentials.portal_access_token.clone();
    let mut stored_refresh_token = credentials.refresh_token.clone();
    let mut expires_at = credentials.expires_at;
    let mut inference_base_url = None;
    if force || portal_stale {
        let token_set = refresh_token(&credentials.refresh_token, proxy.as_ref()).await?;
        portal_access_token = token_set.access_token;
        if let Some(next) = token_set.refresh_token.as_ref() {
            if !next.trim().is_empty() {
                stored_refresh_token = next.clone();
            }
        }
        expires_at = now + token_set.expires_in.saturating_mul(1000);
        inference_base_url = token_set.inference_base_url;
    }
    // ② 运行时凭证 = OAuth access token 本身（官方 hermes：agent_key = access_token）。
    //    上游已退役 /api/oauth/agent-key，不再铸 key —— access token 换了就用新的，
    //    没换（仅临期判定触发）就沿用记录里那份。
    let agent_key = portal_access_token.clone();
    let agent_key_expires_at = expires_at;
    let write = store.update_nous_credentials_if_current(
        &credentials.id,
        &credentials.refresh_token,
        &portal_access_token,
        Some(stored_refresh_token.as_str()),
        expires_at,
        Some(agent_key.as_str()),
        agent_key_expires_at,
        inference_base_url.as_deref(),
    );
    match write {
        Ok(CredentialWrite::Written) => {}
        Ok(CredentialWrite::Stale) => {
            logging::log(
                "[Nous]",
                "⚠️ 刷新结果未能写回账号记录（期间被改动过），本次请求用新凭证，下次会以记录里的为准",
            );
        }
        Err(reason) => return Err(GatewayError::with_status(500, reason)),
    }
    logging::verbose(
        "[Nous]",
        &format!(
            "凭证刷新成功（access token {}，有效期至 {}）",
            credentials::masked(&agent_key),
            agent_key_expires_at
        ),
    );
    Ok(NousCredentials {
        id: credentials.id.clone(),
        name: credentials.name.clone(),
        anonymous: false,
        token: agent_key,
        portal_access_token,
        base_url: inference_base_url
            .unwrap_or_else(|| credentials.base_url.clone()),
        refresh_token: stored_refresh_token,
        expires_at,
        agent_key_expires_at,
    })
}

/// 读账号记录（点名 vs 队首）。
fn read_record(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    if let Some(record) = store.nous_account_record(account_id) {
        return Ok(record);
    }
    if account_id.trim().is_empty() {
        Err(GatewayError::with_status(
            401,
            "没有可用的 Nous 账号：请在账号页用「Nous Portal 登录」添加",
        ))
    } else {
        Err(GatewayError::with_status(
            401,
            format!("指定的 Nous 账号不存在或不可用：{account_id}"),
        ))
    }
}

// ─── 响应解析与传输 ─────────────────────────────────────────

/// 一次令牌读取（设备码成功 / 续期共用）
#[derive(Clone, Debug)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_in: i64,
    pub scope: String,
    pub inference_base_url: Option<String>,
}

/// 解析令牌端点成功响应。
fn parse_token_set(payload: &Value) -> Result<TokenSet, GatewayError> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "Nous 令牌响应缺少 access_token"))?
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(3600);
    Ok(TokenSet {
        access_token,
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        token_type: payload
            .get("token_type")
            .and_then(Value::as_str)
            .unwrap_or("Bearer")
            .to_string(),
        expires_in,
        scope: payload
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        inference_base_url: payload
            .get("inference_base_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

/// 取一个必填字符串字段（缺失或空白 → 报错）
fn required_str(payload: &Value, key: &str, what: &str) -> Result<String, GatewayError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| GatewayError::with_status(502, format!("{what}响应缺少 {key}")))
}

/// 上游错误 → 可读的网关错误（保留 `error_description`）
fn device_error(status: u16, payload: &Value, prefix: &str) -> GatewayError {
    let code = payload.get("error").and_then(Value::as_str).unwrap_or("");
    let description = payload
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = if description.is_empty() {
        code.to_string()
    } else {
        format!("{code}: {description}")
    };
    GatewayError::with_status(
        status as i32,
        if detail.is_empty() {
            format!("{prefix}（HTTP {status}）")
        } else {
            format!("{prefix}（HTTP {status}）：{detail}")
        },
    )
}

/// 发一次 `application/x-www-form-urlencoded` POST，返回 `(状态码, 是否 2xx, 解析后的 JSON)`。
///
/// OAuth 的 token / device-code 端点只收 form，因此这里不走 `send_raw`
/// （后者恒发 JSON）—— 与 Antigravity / OfficeAce 的 token 端点同一处理。
async fn post_form(
    proxy: Option<&ResolvedProxy>,
    url: &str,
    form: &[(&str, &str)],
) -> Result<(u16, bool, Value), GatewayError> {
    let client = client_for(proxy);
    let response = client
        .post(url)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .form(form)
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!(
                    "Nous Portal 请求失败：{}",
                    crate::server::core::egress::describe_error_detail(&error)
                ),
            )
        })?;
    let status = response.status().as_u16();
    let ok = response.status().is_success();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, ok, payload))
}

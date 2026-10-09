//! Antigravity 的 token 刷新：`grant_type=refresh_token` 打 Google 的 token 端点。
//!
//! ── 协议（规格 §1.5，两套参考实现逐字一致）────────────────────
//! ```text
//! POST https://oauth2.googleapis.com/token
//! Content-Type: application/x-www-form-urlencoded
//! User-Agent: vscode/1.X.X (Antigravity/{ver})        ← 原生 OAuth UA
//!
//! client_id     = <内置的公开客户端 id>
//! client_secret = <同上>
//! refresh_token = <账号记录里的主凭证>
//! grant_type    = refresh_token                      ← 下划线形态，别写成 refreshToken
//!
//! 200 → {access_token, expires_in(秒), token_type, refresh_token?, id_token?}
//! ```
//! **`refresh_token` 通常不回传**（Google 只在首次授权时下发）：那时保留旧值，
//! 绝不把已有字段洗成空（与 Trae / Qoder 的同一条规矩）。
//!
//! ── `invalid_grant` 的处置（与规格 §1.6 的差异，有意）───────────
//! Manager 收到 `invalid_grant` 会**停用账号**（写 `disabled: true` 并摘出
//! token 池）。本仓没有「停用」这条产品路径，既有几家的口径是：**报一个
//! 可识别的永久性错误，让用户重新登录/重新粘贴凭证**（Cline 的
//! 「登录态已失效（refreshToken 被拒绝），请重新登录」、Qoder 的
//! 「刷新令牌已失效或与所选地区不符，请重新登录该账号」）。本家照做：
//!   - `invalid_grant` → 401 + 「refresh token 已失效或被撤销，请重新授权后粘贴
//!     新的 refresh_token」；
//!   - `invalid_client` / `unauthorized_client` → 401 + 「OAuth 客户端未被接受」
//!     （本仓只用内置的那一个 client，多 client 轮询是 Manager 的特有能力，
//!     本步不做 —— 规格 §1.5 的「client 不匹配处理」列为 TODO）；
//!   - 其余失败 → 502（上游挂 / 网络问题，可重试），文案带上游原文（截断）。
//! 不做「500ms 退避重试确认」：那是 Manager 为了区分抖动与真失效加的，
//! 而本仓的永久性错误不会被自动重试（只在用户点刷新或下次转发时再走一遍），
//! 多打一次请求没有收益。
//!
//! ── 单飞 + 比较再写（与 Trae / Cline 同一纪律）──────────────────
//! 刷新是秒级网络动作：并发请求会各自去打一次（Google 侧对同一 refresh_token
//! 的并发刷新没有硬限制，但白打没有意义），且回写必须「比较再写」——
//! 期间用户可能重新粘贴凭证，无条件覆盖会把新凭证盖掉。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::egress;
use crate::server::core::proxies::{ProxyResolution, ResolvedProxy};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::AntigravityCredentials;
use super::endpoints;
use super::project;

/// token 端点请求超时（与别家的 30 秒同档）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 上游没给 `expires_in` 时的保守默认（Google 实测约 1 小时；给短了只会多刷一次）
const DEFAULT_EXPIRES_IN_SECONDS: i64 = 3600;

/// 上游错误文在报错里的截断长度（Google 的错误体可能很长）
const MAX_ERROR_CHARS: usize = 300;

/// 单飞表（键 = 账号文件 + 账号 id + refresh_token 指纹，见 [`ensure_fresh`]）
static FLIGHTS: OnceLock<Table<AntigravityCredentials>> = OnceLock::new();

/// 一次 token 端点调用的结果
#[derive(Clone, Debug)]
pub struct TokenResponse {
    /// 新的访问令牌
    pub access_token: String,
    /// 新的 refresh_token（**通常为 None** —— Google 不回传时保留旧值）
    pub refresh_token: Option<String>,
    /// 有效期（秒）
    pub expires_in: i64,
    /// `token_type`（`Bearer`）
    pub token_type: String,
}

/// 账号级出口代理（解析失败按 400 报，不静默直连）—— 与 `trae::adapter` /
/// `accio::auth` 里同名函数同一语义、同一文案来源。
pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match crate::server::core::proxies::resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 截断上游错误文（按字符，UTF-8 安全）
fn truncate(text: &str) -> String {
    let collapsed: String = text.chars().filter(|character| !character.is_control()).collect();
    match collapsed.char_indices().nth(MAX_ERROR_CHARS) {
        Some((index, _)) => format!("{}…", &collapsed[..index]),
        None => collapsed,
    }
}

/// 上游错误体里的 `error` / `error_description`（非 JSON 时给空）
fn upstream_error(payload: Option<&Value>) -> (String, String) {
    let Some(payload) = payload else {
        return (String::new(), String::new());
    };
    let code = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let description = payload
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    (code, description)
}

/// 解析 token 端点的一位读数（`expires_in` 接受数字与数字字符串两种形态）
fn expires_in_of(payload: &Value) -> i64 {
    match payload.get("expires_in") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// 解析成功响应（200 且带 `access_token`）
fn parse_token_response(payload: &Value) -> Result<TokenResponse, GatewayError> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            GatewayError::with_status(502, "Antigravity 刷新响应缺少 access_token，旧凭证未被覆盖")
        })?;
    let refresh_token = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let expires_in = expires_in_of(payload);
    let token_type = payload
        .get("token_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("Bearer")
        .to_string();
    Ok(TokenResponse {
        access_token,
        refresh_token,
        expires_in,
        token_type,
    })
}

/// 把一次失败响应翻成网关错误（见模块头的三档处置）。
fn classify_failure(status: u16, text: &str, payload: Option<&Value>) -> GatewayError {
    let (code, description) = upstream_error(payload);
    let detail = if description.is_empty() {
        truncate(text)
    } else {
        truncate(&description)
    };
    match code.as_str() {
        "invalid_grant" => GatewayError::with_status(
            401,
            "Antigravity 的 refresh token 已失效或被撤销（Google 返回 invalid_grant）：\
             请在 Google 账号授权页重新授权后，把新的 refresh_token 粘贴进本账号",
        ),
        "invalid_client" | "unauthorized_client" => GatewayError::with_status(
            401,
            format!(
                "Antigravity 的 OAuth 客户端未被 Google 接受（{code}）：\
                 本网关内置的 client 与该 refresh token 不匹配，请重新授权后粘贴新的 refresh_token"
            ),
        ),
        _ => {
            let hint = if detail.is_empty() {
                String::new()
            } else {
                format!("：{detail}")
            };
            GatewayError::with_status(502, format!("Antigravity 刷新 token 失败（{status}）{hint}"))
        }
    }
}

/// 打一次 token 端点（**纯网络 + 解析，不落盘**）。
///
/// 日志只打「成功/失败 + 状态码」，**绝不打印 token 本体或表单内容**
/// （refresh_token 是账号主凭证）。
pub async fn refresh_access_token(
    refresh_token: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<TokenResponse, GatewayError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Antigravity 账号缺少 refresh token，请重新粘贴（Google OAuth 的 refresh_token）",
        ));
    }
    let client = egress::client_for(proxy);
    let response = client
        .post(endpoints::TOKEN_URL)
        .header("User-Agent", endpoints::oauth_user_agent())
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        // `Accept: application/json` 与别家的管理接口一致；token 端点本就回 JSON
        .header("Accept", "application/json")
        .form(&[
            ("client_id", endpoints::CLIENT_ID),
            ("client_secret", endpoints::CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", endpoints::GRANT_TYPE_REFRESH),
        ])
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!("Antigravity 刷新请求失败：{}", egress::describe_error_detail(&error)),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let error = classify_failure(status, &text, Some(&payload));
        logging::log(
            "[Antigravity]",
            &format!("❌ 刷新 token 失败（HTTP {status}）：{}", error.message),
        );
        return Err(error);
    }
    parse_token_response(&payload)
}

/// 取可用凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
///
/// `account_id` 为空 = 本家的队首可用账号（与别家同口径；点名的账号取不到
/// 直接报错，不回落队首 —— 那会变成「点名了 A、用了 B」）。
pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<AntigravityCredentials, GatewayError> {
    let record = read_record(store, account_id)?;
    let credentials = super::credentials::from_record(Some(&record))?;
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "Antigravity 账号缺少 refresh token，无法续期，请重新粘贴",
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
            let result = refresh_and_save(store, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 打一次刷新 + 回写（单飞的 leader 走这一段）。
///
/// ── 回写的四处取值（`None`/空值一律**保留旧值**）───────────────
///   - `access_token`：新令牌（必有）；
///   - `refresh_token`：上游回传才覆盖（见模块头）；
///   - `expiresAt`：`now + expires_in`（规格 §2 的 `expiry_timestamp` 口径）；
///   - `projectId`：本次**刷到了**才写（见下）。
///
/// ── project 的发现时机 ──────────────────────────────────────
/// `project` 只影响聊天请求（规格 §3.4：目录 / 额度 / loadCodeAssist 都忽略它），
/// 因此这里只做**一次** best-effort 补齐：账号记录里没有
/// projectId 时顺手发现一次，失败只打日志、不影响刷新结果（用户加账号时
/// `login.rs` 不阻塞同一条路径）。
async fn refresh_and_save(
    store: &AccountStore,
    record: &Value,
    credentials: &AntigravityCredentials,
) -> Result<AntigravityCredentials, GatewayError> {
    let proxy = account_proxy(record)?;
    let response = refresh_access_token(&credentials.refresh_token, proxy.as_ref()).await?;
    let mut fresh = credentials.clone();
    fresh.access_token = response.access_token.trim().to_string();
    if let Some(refresh) = response.refresh_token.as_ref() {
        fresh.refresh_token = refresh.trim().to_string();
    }
    // `expires_in` 越界（上游给了脏值）时按默认 1 小时算：给短了只会多刷一次，
    // 给长了会让失效令牌留在「不临期」状态 —— 两者都比信任一个脏值好。
    let expires_in = if (1..=7 * 24 * 3600).contains(&response.expires_in) {
        response.expires_in
    } else {
        DEFAULT_EXPIRES_IN_SECONDS
    };
    fresh.expires_at = logging::now_ms() + expires_in * 1000;
    logging::verbose(
        "[Antigravity]",
        &format!(
            "token 刷新成功（token_type {}，有效期 {expires_in} 秒）",
            response.token_type
        ),
    );
    // best-effort：project 缺失时补一次（只影响聊天，失败不阻断刷新）
    let mut project_id: Option<String> = None;
    if credentials.project_id.trim().is_empty() {
        if let Ok(found) = project::discover_project(&fresh.access_token, proxy.as_ref()).await {
            if !found.trim().is_empty() {
                project_id = Some(found.clone());
                fresh.project_id = found;
            }
        }
    }
    match store.update_antigravity_credentials_if_current(
        &fresh.id,
        &credentials.refresh_token,
        &fresh.access_token,
        response.refresh_token.as_deref(),
        fresh.expires_at,
        project_id.as_deref(),
    ) {
        Ok(CredentialWrite::Written) => Ok(fresh),
        Ok(CredentialWrite::Stale) => {
            // 记录在刷新期间被换过（重新粘贴 / 手工编辑）：把已刷出的令牌用掉，
            // 但不覆盖记录，并如实说一句让用户知道（与 Trae 同一处置）。
            logging::log(
                "[Antigravity]",
                "⚠️ 刷新结果未能写回账号记录（期间被改动过），本次请求用新令牌，下次会以记录里的为准",
            );
            Ok(fresh)
        }
        Err(reason) => Err(GatewayError::with_status(500, reason)),
    }
}

/// 读账号记录（点名 vs 队首）。
///
/// 点名的账号不存在 → 报错，**不回落队首**：那会把「点名的账号坏了」变成
/// 「静默用了别人的额度」（与 Trae / ZCode 同一条纪律）。
pub fn read_record(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    if account_id.trim().is_empty() {
        return store
            .antigravity_account_record("")
            .ok_or_else(|| GatewayError::with_status(401, "没有可用的 Antigravity 账号，请先在「账号」页添加"));
    }
    store
        .antigravity_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(401, "找不到该 Antigravity 账号"))
}

//! LobsterAI 令牌保活：把全部 LobsterAI 账号的 access token 主动刷一遍。
//!
//! ── 与 `credential_maintenance` 的分工 ────────────────────────
//! `credential_maintenance` 只刷**已过期 / 临期**的凭证；本任务是无条件地按
//! 配置的节奏替每个账号打一次 `POST /api/auth/refresh`（对照参考实现
//! `RunKeepaliveNow` 的每日保活），把 access token 的剩余有效期始终顶到最长
//! —— 桌面端长期不开、又没人转发时，账号也不会因为 access token 到期而在账号
//! 页显示「已过期」。两者共用 `refresh::ensure_fresh`（单飞 + 比较再写）。
//!
//! ── 会话死亡 → 禁用（与别家同款）─────────────────────────────
//! 刷新被上游拒绝（HTTP 401 / 403，refresh token 已废）说明这个账号只能重新
//! 登录：把它 `enabled` 置 false，避免它继续参与转发、每轮都白打一次上游。
//! 与 `usage_query` 的「余额不足自动禁用」走同一条写路径（`update_account`），
//! 变更日志同样由那一步统一打。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::logging;

use super::{refresh, PROVIDER_ID};

/// 逐条结果状态（与 `credential_maintenance` 的三档同形，摘要统计共用口径）
pub const STATUS_REFRESHED: &str = "refreshed";
/// 没有 refresh token，无法续期（不是失败）
pub const STATUS_SKIPPED: &str = "skipped";
/// 尝试续期但失败（结果里带可读的 `message`）
pub const STATUS_FAILED: &str = "failed";

/// 把全部 LobsterAI 账号的 token 刷一遍（保活）。
///
/// 返回逐账号结果 `[{id, status, message?}]`（没有 LobsterAI 账号时为空数组）。
/// 没有 refresh token 的账号跳过（不是失败：它可能仍有一枚有效的 access token，
/// 只是没有续期手段）；刷新失败里 HTTP 401 / 403 判为「会话死亡」，顺带禁用该
/// 账号（见模块头）。
///
/// 逐个 await（不并发）：保活是低频维护动作、单个账号打一次上游，串行既避开
/// 风控也保证日志顺序稳定；单个账号失败被 match 收敛成一条结果，不中断循环。
pub async fn refresh_all_tokens(store: &AccountStore) -> Vec<Value> {
    let snapshot = store.list_accounts();
    let accounts: Vec<Value> = snapshot
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut results = Vec::new();
    for account in &accounts {
        if account.get("provider").and_then(Value::as_str) != Some(PROVIDER_ID) {
            continue;
        }
        let Some(id) = account
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        // 没有 refresh token 的账号无法续期：跳过（不是失败），见函数头
        if account.get("hasRefreshToken").and_then(Value::as_bool) == Some(false) {
            results.push(json!({
                "id": id,
                "status": STATUS_SKIPPED,
                "message": "没有 refresh token，无法续期",
            }));
            continue;
        }
        // 单飞：与转发链的懒刷新并发时只会真打一次上游（`refresh_flight`）
        match refresh::ensure_fresh(store, id, true).await {
            Ok(_) => {
                logging::verbose("[LobsterAI]", &format!("令牌保活：账号 {id} 已刷新"));
                results.push(json!({ "id": id, "status": STATUS_REFRESHED }));
            }
            Err(error) => {
                if is_session_dead(&error) {
                    disable_account(store, id);
                    results.push(json!({
                        "id": id,
                        "status": STATUS_FAILED,
                        "message": format!(
                            "{}（登录态已失效，已自动禁用，请重新登录）",
                            error.message
                        ),
                    }));
                } else {
                    logging::verbose(
                        "[LobsterAI]",
                        &format!("令牌保活：账号 {id} 刷新失败：{}", error.message),
                    );
                    results.push(json!({
                        "id": id,
                        "status": STATUS_FAILED,
                        "message": error.message,
                    }));
                }
            }
        }
    }
    results
}

/// 刷新失败是否意味着「会话死亡」（refresh token 被上游作废）。
///
/// 上游拒绝续期时 `refresh::refresh_tokens` 把 HTTP 状态原样带出：401 / 403
/// 是「这份 refresh token 不再被接受」，只能重新登录 —— 与参考实现
/// `ErrSessionDead`（401 + 40100/40101）同一判定方向（这里按 HTTP 状态码，
/// 因为本家的业务码经信封解包后已折进 502，不再单独可辨）。
fn is_session_dead(error: &crate::server::errors::GatewayError) -> bool {
    matches!(error.status_code, 401 | 403)
}

/// 会话死亡的账号自动禁用（与「余额不足自动禁用」同一写路径与语义）。
///
/// `update_account` 幂等：账号已经是禁用状态时 `changes` 为空，此时不重复打
/// 日志（避免每轮保活都给同一个死账号刷一条 warn）。
fn disable_account(store: &AccountStore, id: &str) {
    match store.update_account(id, &json!({ "enabled": false })) {
        Ok((_, changes)) if !changes.is_empty() => logging::log_with_level(
            "[LobsterAI]",
            &format!("⛔ 账号 {id} 的登录态已失效，已自动禁用（请重新登录）"),
            "warn",
        ),
        Ok(_) => {}
        Err(error) => logging::verbose(
            "[LobsterAI]",
            &format!("账号 {id} 自动禁用失败：{}", error.message),
        ),
    }
}

//! Trae 的每日签到（`checkin_credits/claim`，与积分读数同属 ug 一族接口）。
//!
//! ── 上游是什么（实测确认）───────────────────────────────────
//! Trae 客户端「领取每日积分」走的是一条**独立于积分读数**的链，两条端点都在
//! `https://api.trae.cn` 的 `trae/api/v2/ug/` 下，请求体都是空对象 `{}`：
//!
//! ```text
//! POST /trae/api/v2/ug/checkin_credits/claim
//!   → {"code":0,"message":"success"}                       // 领到
//!   → {"code":9074,"message":"当前参与用户太多，请稍后再试"}   // 风控
//! POST /trae/api/v2/ug/checkin_credits/status
//!   → {"checked_in":bool,"code":0,"credits":100,"did_checked_in":bool,
//!      "enable":bool,"extra_credits":100,"message":"success"}
//! ```
//!
//! ── 为什么复用 ug 那套头（而不是转发那套 SOLO 头）────────────
//! claim / status 与 `ide_user_ent_usage` 是**同一族画像**：VSCode 插件进程 UA、
//! `Package-Type`、`X-User-Region`、`Authorization: Cloud-IDE-JWT <token>`，
//! 域名固定 `https://api.trae.cn`。所以这里直接复用 `usage::ug_headers` ——
//! 手搓一份头几乎必然漏掉某个画像字段，而谱系不匹配时上游回的不是普通报错
//! 而是 401 `code=1001`「we are not able to authenticate you」（见 `usage.rs`
//! 模块头），排查成本极高。
//!
//! ── 风控前科：这是本模块的三条硬纪律 ─────────────────────────
//! `usage.rs` 的模块头特意记着参考实现的一段前科：把出口 IP 打进封禁 ——
//! `9074` 重试链 + 面板连点。因此本模块立三条纪律，且**不允许"顺手"放宽**：
//!   1. **一次调用只打一次 claim**，绝不重试。9074 是上游在说"人太多"，
//!      再打只会加深风控；
//!   2. **不轮换 `device_id`**。换设备指纹等于换账号（见 `credentials.rs` 的
//!      "设备指纹与凭据同生共死"），那正是参考实现踩过的坑；
//!   3. status 只是**尽力而为的补充**（拿奖励额拼成功文案），它失败绝不影响
//!      签到结论 —— 补一次展示用的读数，不该让一次成功的签到变成失败。
//!
//! ── 与别家的形状对齐 ────────────────────────────────────────
//! 返回 `{success, msg, ...}`，与 WorkBuddy / 小浣熊 / AutoClaw / Qoder /
//! Loomy 同一形状（`billing::checkin::claim_result` 的汇总只认这两个字段，
//! 于是工作流、日志与界面三处都不需要为本家再加分支）。`success` 的口径是
//! 「**本次真的领到了**」：`code == 0` 才算成功；9074 与其它非 0 业务码都是
//! `success: false` + 原因，前端把这类当 warn 提示显示。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::time::Duration;

use serde_json::{Value, json};

use crate::server::core::account_store::AccountStore;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

use super::adapter::{account_proxy, read_record, renew_if_due};
use super::credentials::Credential;
use super::http::post_json;
use super::usage::{UG_HOST, ug_headers};

/// 领取每日积分的路径（ug 族，host 固定 `https://api.trae.cn`）。
const CLAIM_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";
/// 签到状态路径（best-effort，只用来给成功文案补一个奖励额）。
const STATUS_PATH: &str = "/trae/api/v2/ug/checkin_credits/status";

/// 风控业务码：上游在「当前参与用户太多」时回它（实测两个账号的其中一个是
/// 持续 9074，另一个正常 0）。
const RISK_CONTROL_CODE: i64 = 9074;
/// 9074 的固定文案。**钉死**而不是回显上游 `message`：这句是抓包实证的原文，
/// 固定住之后即便上游改了措辞，界面文案也不会跟着飘。
const RISK_CONTROL_MESSAGE: &str = "当前参与用户太多，请稍后再试";
/// 非 0 业务码但上游没给可读文案时的兜底失败文案。
const FALLBACK_FAILURE_MESSAGE: &str = "签到失败";

/// 单次请求超时，与 `usage.rs` 的 `REQUEST_TIMEOUT` 同口径（20 秒）。
///
/// 必须显式设：`egress` 默认的 read_timeout 是 600 秒（留给 SSE 长连接），
/// 不设的话一个挂住的签到请求会把「立即执行一次」拖到一直转圈。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 错误体进消息的截断长度（与参考实现 200 字符同一用意：别把整页 HTML 灌进日志）。
const ERROR_BODY_HEAD: usize = 200;

/// Trae 账号的每日签到。
///
/// 取凭证链与 `usage.rs::query_usage` **逐条一致**（账号记录 → 凭据还原 →
/// 出口代理 → 临期续期），因为积分族接口对谱系/画像最敏感：拿一个刚被服务端
/// 作废的串去打，得到的是 401「unable to authenticate」而不是一句可读的失败。
///
/// 返回 `{success, msg, ...}`（形状说明见模块头）。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = read_record(store, account_id)?;
    let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    let proxy = account_proxy(&record)?;
    // 临期先续期（与 query_usage 同一处置，见那里的注释）。
    let credential = renew_if_due(store, &record, &credential, proxy.as_ref()).await?;
    if !credential.valid() {
        return Err(GatewayError::with_status(401, "Trae 账号里没有可用凭证，无法签到"));
    }

    // 头集合复用 ug 族（模块头已说明为什么不手搓）。`ug_headers` 返回 BTreeMap，
    // 这里转成 `post_json` 要的切片形态 —— 转换方式与 `usage.rs::read_at` 一致。
    let prepared: Vec<(String, String)> = ug_headers(
        credential.variant(),
        credential.access_token.trim(),
        credential.device_id.trim(),
    )
    .into_iter()
    .collect();
    let headers: Vec<(&str, String)> =
        prepared.iter().map(|(name, value)| (name.as_str(), value.clone())).collect();

    let claim_url = format!("{UG_HOST}{CLAIM_PATH}");
    let reply =
        post_json(&claim_url, &json!({}), &headers, REQUEST_TIMEOUT, proxy.as_ref()).await?;
    // 非 2xx 一律当失败上抛（与别家对 HTTP 失败的处置一致）：业务码只承载在 2xx 体里。
    if !(200..300).contains(&reply.status) {
        let head: String = reply.body.chars().take(ERROR_BODY_HEAD).collect();
        return Err(GatewayError::with_status(
            i32::from(reply.status),
            format!("Trae 签到接口返回 {}: {}", reply.status, head.trim()),
        ));
    }
    let payload = reply.json().unwrap_or_else(|| json!({}));

    // 只在**领到**时才去拉状态：非 0 码直接按失败返回，既省一次毫无意义的往返，
    // 也少一次可能触发风控的出站。status 失败（网络/非 2xx/非 0 码）都当"没有"，
    // 绝不升级成签到失败。
    let status = if business_code(&payload) == Some(0) {
        fetch_status(&headers, proxy.as_ref()).await
    } else {
        None
    };
    Ok(claim_result(&payload, status.as_ref()))
}

/// 业务码。缺键返回 `None` —— **不**当 0。
///
/// 签到里 `code == 0` 是「成功」，把缺键当成功会让一个空响应体（或一段
/// 非 JSON 的 2xx 体）被报成「领到了积分」：假成功比报错更坏 —— 用户会以为
/// 今天已经签过，于是白丢一天。所以缺键一律按失败处理（与 `claim_result` 一致）。
fn business_code(payload: &Value) -> Option<i64> {
    payload.get("code").and_then(Value::as_i64)
}

/// 拉一次签到状态（best-effort）。任何异常都返回 `None`，由调用方退化成
/// 不带奖励额的成功文案。
async fn fetch_status(
    headers: &[(&str, String)],
    proxy: Option<&ResolvedProxy>,
) -> Option<Value> {
    let url = format!("{UG_HOST}{STATUS_PATH}");
    let reply = post_json(&url, &json!({}), headers, REQUEST_TIMEOUT, proxy).await.ok()?;
    if !(200..300).contains(&reply.status) {
        return None;
    }
    let payload = reply.json()?;
    // 只认 `code == 0` 的合法对象：一个失败体不该伪装成一份合法状态。
    if !payload.is_object() || business_code(&payload) != Some(0) {
        return None;
    }
    Some(payload)
}

/// 把 claim 响应摊成统一结果（**纯函数**，测试直接喂 JSON）。
///
/// `status` 只在 claim 成功时才有意义：它负责把成功文案从「签到成功」升级成
/// 「签到成功，获得 {credits} 积分」，并附上 `extraCredits`。claim 失败时
/// 传入的 `status` 会被忽略 —— 9074 不会因为碰巧拿到一份"已签到"的状态而翻盘。
fn claim_result(payload: &Value, status: Option<&Value>) -> Value {
    let code = match business_code(payload) {
        Some(code) => code,
        // 没有可读业务码 = 上游没给出可判定的成功/失败（空体、非 JSON、字段缺失）。
        // 这**不是**成功，如实报失败。
        None => return json!({ "success": false, "msg": FALLBACK_FAILURE_MESSAGE }),
    };
    if code == 0 {
        return success_result(status);
    }
    if code == RISK_CONTROL_CODE {
        return json!({
            "success": false,
            "msg": RISK_CONTROL_MESSAGE,
            "code": code,
        });
    }
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(FALLBACK_FAILURE_MESSAGE);
    json!({
        "success": false,
        "msg": message,
        "code": code,
    })
}

/// 成功结果。`status` 给了就用它的 `credits` 拼奖励额文案，并附 `extraCredits`；
/// 没给（或给不出 `credits`）就退回一句「签到成功」。
fn success_result(status: Option<&Value>) -> Value {
    let Some(status) = status else {
        return json!({ "success": true, "msg": "签到成功" });
    };
    let credits = status.get("credits").and_then(Value::as_i64);
    let extra_credits = status.get("extra_credits").and_then(Value::as_i64);
    let mut object = serde_json::Map::new();
    object.insert("success".to_string(), Value::Bool(true));
    object.insert(
        "msg".to_string(),
        Value::String(match credits {
            Some(credits) => format!("签到成功，获得 {credits} 积分"),
            None => "签到成功".to_string(),
        }),
    );
    if let Some(extra) = extra_credits {
        object.insert("extraCredits".to_string(), Value::from(extra));
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    //! 全是**纯解析**用例，一次网络都不打：把上游实测的响应（成功 / 9074 /
    //! 其它非 0 码 / 缺 code）与 status 的各种组合喂进 `claim_result`，钉住返回形状。
    use super::*;

    #[test]
    fn a_successful_claim_reports_the_reward_from_status() {
        let claim = json!({"code": 0, "message": "success"});
        let status = json!({
            "checked_in": true, "code": 0, "credits": 100, "did_checked_in": true,
            "enable": true, "extra_credits": 100, "message": "success"
        });
        let result = claim_result(&claim, Some(&status));
        assert_eq!(Some(true), result.get("success").and_then(Value::as_bool));
        assert_eq!(Some("签到成功，获得 100 积分"), result.get("msg").and_then(Value::as_str));
        assert_eq!(Some(100), result.get("extraCredits").and_then(Value::as_i64));
    }

    #[test]
    fn a_successful_claim_without_status_still_succeeds() {
        let claim = json!({"code": 0, "message": "success"});
        let result = claim_result(&claim, None);
        assert_eq!(Some(true), result.get("success").and_then(Value::as_bool));
        assert_eq!(Some("签到成功"), result.get("msg").and_then(Value::as_str));
        assert!(result.get("extraCredits").is_none(), "没有 status 就不该凭空造一个额外积分");
    }

    #[test]
    fn risk_control_9074_is_reported_verbatim_and_never_succeeds() {
        let claim = json!({"code": 9074, "message": "当前参与用户太多，请稍后再试"});
        // 即便同时拿到一份"已签到"的 status，9074 也不许翻成成功 —— 这次没领到。
        let status = json!({"code": 0, "credits": 100, "extra_credits": 100});
        let result = claim_result(&claim, Some(&status));
        assert_eq!(Some(false), result.get("success").and_then(Value::as_bool));
        assert_eq!(Some(RISK_CONTROL_CODE), result.get("code").and_then(Value::as_i64));
        assert_eq!(Some(RISK_CONTROL_MESSAGE), result.get("msg").and_then(Value::as_str));
    }

    #[test]
    fn an_unknown_non_zero_code_keeps_the_upstream_message() {
        let claim = json!({"code": 1001, "message": "we are not able to authenticate you"});
        let result = claim_result(&claim, None);
        assert_eq!(Some(false), result.get("success").and_then(Value::as_bool));
        assert_eq!(Some(1001), result.get("code").and_then(Value::as_i64));
        assert_eq!(
            Some("we are not able to authenticate you"),
            result.get("msg").and_then(Value::as_str)
        );
    }

    #[test]
    fn a_non_zero_code_without_a_message_falls_back() {
        let result = claim_result(&json!({"code": 500}), None);
        assert_eq!(Some(false), result.get("success").and_then(Value::as_bool));
        assert_eq!(Some(500), result.get("code").and_then(Value::as_i64));
        assert_eq!(Some(FALLBACK_FAILURE_MESSAGE), result.get("msg").and_then(Value::as_str));
    }

    #[test]
    fn a_response_without_a_code_is_a_failure_not_a_silent_success() {
        // 空体 / 非 JSON 的 2xx 体在解析层会变成 `{}`：缺 `code` 绝不能报成功，
        // 否则用户会以为今天签过了而白丢一天（假成功比报错更坏）。
        for payload in [json!({}), json!({"message": "success"})] {
            let result = claim_result(&payload, None);
            assert_eq!(Some(false), result.get("success").and_then(Value::as_bool), "{payload}");
            assert_eq!(
                Some(FALLBACK_FAILURE_MESSAGE),
                result.get("msg").and_then(Value::as_str),
                "{payload}"
            );
        }
    }

    #[test]
    fn a_status_without_credits_still_forms_a_success_message() {
        // status 拿到了但缺 credits（上游字段缺失）：成功结论不变，只是文案退化。
        let claim = json!({"code": 0, "message": "success"});
        let status = json!({"code": 0, "checked_in": true});
        let result = claim_result(&claim, Some(&status));
        assert_eq!(Some(true), result.get("success").and_then(Value::as_bool));
        assert_eq!(Some("签到成功"), result.get("msg").and_then(Value::as_str));
        assert!(result.get("extraCredits").is_none());
    }
}

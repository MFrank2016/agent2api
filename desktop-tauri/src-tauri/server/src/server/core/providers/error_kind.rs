//! 配额类失败的**归类**：同一份 429 到底该按哪种冷却处理。
//!
//! ── 为什么要分类 ────────────────────────────────────────────
//! 上游的「限流」不是一个东西：余额耗尽要**停整个账号**直到充值，套餐周期
//! 用尽要停到重置时刻，分钟级 RPM 超限几十秒后就该恢复。三类都用 429 + 一段
//! 文案表达，若一律按「短冷却」处理，余额不足的账号会被反复放回池子里打，
//! 形成 429 风暴；一律按「长冷却」又会把一个只是这一分钟打快了的账号关半天。
//! 所以先把 429 判成 [`QuotaKind`]，再由上层决定 rest 时长与作用域。
//!
//! ── 判定口径 ───────────────────────────────────────────────
//! 只看 `message` 词表 + 少量厂商特例（`vendor` / `code`），**不猜**：
//! 命中哪一层的词就归哪一类，命中顺序即优先级（信用类最具体，先判）。
//! 全都没命中时回落 [`QuotaKind::Quota`] —— 保守选择：宁可让该模型多休息
//! 一会儿，也不要把额度类失败误判成分钟级限流而反复重试（见设计文档
//! 「无法判定时回落 Quota（保守，但不再误判为 rate）」）。
//!
//! 词表全部小写，匹配前把上游文案转小写（与 `content_block` 同一口径）；
//! 中文词不受大小写转换影响，直接子串匹配即可。
//!
//! ── 关于 `status` ──────────────────────────────────────────
//! 本函数只被「已经判定为配额类」的失败调用，状态码不参与词表匹配；参数
//! 保留是为了后续按厂商状态码（如 402）做覆盖时不必改调用方签名。

/// 配额类失败的**冷却口径**：决定 rest 时长与作用域（账号级 / 模型级）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaKind {
    /// 余额 / 积分耗尽：账号级，停到充值或手动解除。
    Credit,
    /// 额度耗尽（非余额）：模型级，停到重置时刻。
    Quota,
    /// 分钟级限流：模型级，短 rest（`retryAfterMs` 或默认几十秒）。
    Rate,
    /// 套餐周期用量用尽：模型级，停到周期重置。
    Plan,
    /// 需要人机验证：账号级，长 rest。
    Verification,
}

/// 余额 / 积分类文案（最具体，优先于限流词）。
const CREDIT_WORDS: &[&str] = &[
    "余额不足",
    "insufficient_quota",
    "credit",
    "积分不足",
    "no balance",
];
/// 分钟级限流文案：必须是**明确的时间粒度**，不含泛化的 "too many requests"。
///
/// 泛化的 429 文案无法区分「这一分钟打快了」与「额度没了」，按设计文档的
/// 保守口径回落 [`QuotaKind::Quota`]，不判 Rate。
const RATE_WORDS: &[&str] = &["per minute", "per second", "rpm", "tpm", "rate limit"];
/// 套餐 / 周期用量文案。
const PLAN_WORDS: &[&str] = &[
    "weekly",
    "monthly",
    "usage limit",
    "套餐",
    "plan limit",
    "resets in",
];
/// 人机验证文案。
const VERIFY_WORDS: &[&str] = &["verify", "verification", "captcha", "验证"];

/// 智谱系厂商身份：内置 provider id（`chatglm`）、自定义提供商的余额方案
/// （`zhipu` / `zhipu_zai`）与 preset key（`glm` / `glm-cn`）都指向同一家。
const ZHIPU_VENDORS: &[&str] = &["zhipu", "zhipu_zai", "glm", "glm-cn", "chatglm"];

/// 把一次配额类失败判成 [`QuotaKind`]。
///
/// `message` 是上游返回的原文（可为空）；`vendor` 是**厂商身份** —— 内置
/// provider id，或自定义提供商的余额方案 / preset key（同一个上游在不同
/// 接入形态下叫法不同，见 [`ZHIPU_VENDORS`]）；`code` 是上游业务码，
/// 同样只用于厂商特例。
pub fn classify_quota_kind(
    _status: u16,
    code: Option<i64>,
    message: &str,
    vendor: &str,
) -> QuotaKind {
    let text = message.to_ascii_lowercase();
    if CREDIT_WORDS.iter().any(|word| text.contains(word)) {
        return QuotaKind::Credit;
    }
    if VERIFY_WORDS.iter().any(|word| text.contains(word)) {
        return QuotaKind::Verification;
    }
    if RATE_WORDS.iter().any(|word| text.contains(word)) {
        return QuotaKind::Rate;
    }
    if PLAN_WORDS.iter().any(|word| text.contains(word)) {
        return QuotaKind::Plan;
    }
    // 厂商特例：智谱 1113 是额度不足，文案里不一定带词表关键词。
    // 这里认的是「厂商身份」而非单一 provider id：内置 `chatglm`、自定义
    // 提供商的余额方案 `zhipu` / `zhipu_zai`、preset key `glm` / `glm-cn`
    // 打的是同一家上游，业务码口径一致。
    if ZHIPU_VENDORS.contains(&vendor) && code == Some(1113) {
        return QuotaKind::Credit;
    }
    QuotaKind::Quota
}

/// 从响应头里取 `Retry-After`（秒）并转成毫秒。
///
/// 头名大小写不敏感；值不是整数（如 HTTP-date 形式）或缺失时返回 `None`，
/// 由上层回落到默认 rest 时长。
///
/// `<= 0` 一律按「没给」处理（返回 `None`）：与本项目其它 `Retry-After`
/// 消费点口径一致（`server/errors.rs` 不发 `Retry-After: 0`，
/// `plugin_host/strict_errors.rs` 的 `retry_reset` 对 0 返回 `None`）。
/// 若返回 `Some(0)`，上层会把它当成「立即恢复」而立刻重新派发 —— 正是
/// 429 风暴要避免的。
pub fn retry_after_ms(headers: &[(String, String)]) -> Option<i64> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<i64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(|seconds| seconds.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_words_win_over_rate_words() {
        assert_eq!(
            classify_quota_kind(429, Some(1113), "余额不足或无可用资源包", "zhipu"),
            QuotaKind::Credit
        );
    }

    #[test]
    fn minute_words_are_rate_not_quota() {
        assert_eq!(
            classify_quota_kind(
                429,
                None,
                "Rate limit exceeded: 20 requests per minute",
                "openrouter"
            ),
            QuotaKind::Rate
        );
    }

    #[test]
    fn plan_words_are_plan() {
        assert_eq!(
            classify_quota_kind(
                429,
                None,
                "weekly standard usage limit resets in 2d",
                "factory"
            ),
            QuotaKind::Plan
        );
    }

    #[test]
    fn unknown_falls_back_to_quota() {
        assert_eq!(
            classify_quota_kind(429, None, "too many requests", "custom-x"),
            QuotaKind::Quota
        );
    }

    #[test]
    fn retry_after_parses_seconds() {
        let headers = vec![("retry-after".to_string(), "120".to_string())];
        assert_eq!(retry_after_ms(&headers), Some(120_000));
    }

    #[test]
    fn verification_words_are_verification() {
        assert_eq!(
            classify_quota_kind(429, None, "please complete the captcha", "custom-y"),
            QuotaKind::Verification
        );
    }

    #[test]
    fn zhipu_code_is_credit_without_words() {
        // 自定义提供商接入时 `vendor` 是余额方案 / preset key。
        assert_eq!(
            classify_quota_kind(429, Some(1113), "", "zhipu"),
            QuotaKind::Credit
        );
        assert_eq!(
            classify_quota_kind(429, Some(1113), "", "zhipu_zai"),
            QuotaKind::Credit
        );
        assert_eq!(
            classify_quota_kind(429, Some(1113), "", "glm"),
            QuotaKind::Credit
        );
        assert_eq!(
            classify_quota_kind(429, Some(1113), "", "glm-cn"),
            QuotaKind::Credit
        );
    }

    #[test]
    fn chatglm_code_is_credit_without_words() {
        // 生产形态：内置 provider id 是 `chatglm`，不存在 id `zhipu`。
        assert_eq!(
            classify_quota_kind(429, Some(1113), "", "chatglm"),
            QuotaKind::Credit
        );
    }

    #[test]
    fn retry_after_ignores_invalid_or_absent_values() {
        let headers = vec![("retry-after".to_string(), "soon".to_string())];
        assert_eq!(retry_after_ms(&headers), None);
        assert_eq!(retry_after_ms(&[]), None);
    }

    #[test]
    fn retry_after_zero_or_negative_is_no_header() {
        assert_eq!(
            retry_after_ms(&[("retry-after".to_string(), "0".to_string())]),
            None
        );
        assert_eq!(
            retry_after_ms(&[("retry-after".to_string(), "-5".to_string())]),
            None
        );
    }
}

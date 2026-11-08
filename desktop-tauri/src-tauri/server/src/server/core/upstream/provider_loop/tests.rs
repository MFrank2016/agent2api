use super::*;
use crate::server::core::upstream::usage::RequestTelemetry;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

async fn mock_upstream(
    status: u16,
    content_type: &'static str,
    body: &'static str,
) -> (
    TransportRequest,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let app = axum::Router::new().route(
        "/messages",
        axum::routing::post(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [(axum::http::header::CONTENT_TYPE, content_type)],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/messages", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        TransportRequest {
            url,
            headers: vec![("Content-Type".into(), "application/json".into())],
            payload: r#"{"stream":true}"#.into(),
            egress: crate::server::core::proxies::AccountEgress::Direct,
            // 测试替身按默认能力位（false = 直连），与生产里不带这一步的家同款
            system_proxy_when_unset: false,
            // 测试请求体没有走脱敏门禁（未开启脱敏），因此不带会话
            mask_session: None,
        },
        hits,
        task,
    )
}

// ── ZCode 的响应判定与错误分类（测试随实现搬到插件）────────────────────────
//
// 这里原有五条用例钉的是 ZCode 适配器的两件事：
//   · `is_error_response`：HTTP 200 + `application/json` 是**业务拒绝**而不是
//     SSE 正文（`1005 exceed quota limit` 走 200 回来），网关必须先读错误体；
//   · `classify_error`：401 → TokenExpired、429 / 业务码 1005 → QuotaLimited。
//
// 这两条判据都跟着 ZCode 的实现搬到 `agent2api-providers` 了，因此用例也一起
// 搬走 —— 留在网关只会编译不过（`ZCODE_ADAPTER` 已不存在）。插件侧的同名用例
// 在 `zcode::mod` 的 `classify` 附近。
//
// 顺带说明**为什么这段注释留着**：`provider_loop` 的「HTTP 200 也可能是错误」
// 与「401/429 各自的动作」这两条**编排层**语义没有变，下面几条用例
// （会话式路径的限额记账）仍在验证它们。

// ── 会话式路径的限额记账（`mark_conversation_limited`）──────────────────────
//
// 这四条钉的是同一处缺陷：改造前该分支内联调用 `rotate::mark_account_limited`，
// `reset_at` 实参写死 `None` ⇒ 适配器算好的恢复时刻被丢在函数参数上，存储层落到
// 「缺失即 10 分钟」的兜底冷却（`account_store::store_admin::mark_rate_limited`）。
// 症状不是报错，而是**同一个已耗尽的账号按兜底常量的节拍被反复重新撞**：CodeArts
// 的福利日池要到第二天零点才有额度，生产实测两小时内白撞 8 次（每次 0.2–0.3 秒、
// 被拒不计费、整条请求仍 200 —— 只看响应看不出来）。
//
// 因此在改前的代码上第 1、4 条必红；第 2、3 条是「修复没有过头」的对照组。

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 临时库目录，Drop 时删除。
///
/// 刻意不在建库前删了事：文件名带 pid，下一轮 pid 变了就永远删不到（曾在一台机器上
/// 攒出几百份）。调用方按 `let (_dir, service, id) = temp_service();` 接 —— 局部变量
/// 逆序析构，先关库再删目录。
struct TempDbDir(std::path::PathBuf);

impl Drop for TempDbDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 一份临时库 + 挂在其上的 [`UpstreamService`] + 里面唯一那条账号的 id。
///
/// 不发任何网络请求：记账与读回都只碰存储层，与转发选路用的是同一份读写口径。
fn temp_service() -> (TempDbDir, UpstreamService, String) {
    use crate::server::core::account_store::AccountStore;
    use crate::server::core::auth::AuthService;
    use crate::server::db::Db;

    let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("pl-quota-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = AccountStore::with_db(Some(Db::open(&dir.join("agent2api.db")).expect("临时库应当能建起来")));
    let saved = store
        .add_account(
            &json!({"auth": {"accessToken": "at"}, "account": {"uid": "quota-1"}}),
            None,
            None,
        )
        .expect("临时账号应当能建起来");
    let id = saved["id"].as_str().expect("公开形态应带 id").to_string();
    let service = UpstreamService::new(store.clone(), AuthService::for_store(store));
    (TempDbDir(dir), service, id)
}

/// 本家分类结果 → 记账名单（只取本次这一个键，绕开目录缓存的全局态）。
fn single(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

#[test]
fn stateful_quota_bookkeeping_keeps_the_adapter_reset_at() {
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    // 可辨恢复时刻：3 小时后。刻意与 10 分钟兜底不同量级 —— 改前这里落的是
    // now+600000，等值断言必红；而文案里没有任何日期形态，兜底的第二次机会
    // （`errors::parse_quota_reset_at`）也救不出这个值，差异只能来自透传。
    let reset_at = now + 3 * 60 * 60 * 1000;
    let class = UpstreamErrorClass::QuotaLimited {
        reset_at: Some(reset_at),
        message: "上游返回 403: insufficient quota".to_string(),
        upstream_code: Some(4291200),
        status: 403,
    };
    // 整组（福利池按账号记账）与单键走的是同一个循环，这里给两个名字覆盖循环体
    let group = vec!["m-benefit-a".to_string(), "m-benefit-b".to_string()];
    mark_conversation_limited(
        &service,
        &id,
        &group,
        &class,
        "上游返回 403: insufficient quota",
        "custom-test",
    );
    for name in &group {
        let stored = rotate::account_limit_reset_at(&service, &id, name);
        assert_eq!(stored, reset_at, "{name} 的 resetAt 应当是适配器给的恢复时刻");
        let minutes = (stored - now) / 60_000;
        assert!(
            (stored - (now + 10 * 60 * 1000)).abs() > 60_000,
            "{name} 落到了 10 分钟兜底冷却：{minutes} 分钟"
        );
    }
    // 同一份分类里的另两个字段也照原样落地（写死 None 的那版同样丢掉过它们吗 ——
    // 没有，但这里是唯一一处能一次钉住「记账四元组一致」的地方）
    let limits = {
        let snapshot = service.store.list_accounts();
        snapshot["accounts"].as_array().unwrap()[0]["rateLimits"].clone()
    };
    assert_eq!(limits["m-benefit-a"]["status"], json!(403));
    assert_eq!(limits["m-benefit-a"]["code"], json!(4291200));
}

#[test]
fn stateful_quota_bookkeeping_falls_back_when_adapter_gives_no_reset_at() {
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    // 429 是分钟级限流，CodeArts 的 classify 那边就给 None（给它零点会把一个只是
    // 暂时繁忙的账号打死一整天）—— 兜底冷却是它应得的行为，修复不许把它抹掉。
    // 文案取无日期形态：解析兜底（`parse_quota_reset_at`）也救不出时刻。
    let message = "上游返回 429: too many requests";
    let class = UpstreamErrorClass::QuotaLimited {
        reset_at: None,
        message: message.to_string(),
        upstream_code: None,
        status: 429,
    };
    mark_conversation_limited(
        &service,
        &id,
        &single("m-429"),
        &class,
        message,
        "custom-test",
    );
    let stored = rotate::account_limit_reset_at(&service, &id, "m-429");
    let seconds = (stored - now) / 1000;
    assert!(
        stored > now + 9 * 60 * 1000 && stored <= now + 11 * 60 * 1000,
        "无恢复时刻时应落 10 分钟兜底，实际 {seconds} 秒"
    );
}

#[test]
fn stateful_quota_bookkeeping_ignores_non_quota_classes() {
    let (_dir, service, id) = temp_service();
    // 对照组：调用方的 `if let` 已经拦下非 QuotaLimited，函数自己也不该动手 ——
    // 内容闸门/权限那一类失败本来就不该罚账号（冷却一整天尤其不能给它们）
    let message = "上游返回 403: permission denied";
    let class = UpstreamErrorClass::Fatal {
        status: 403,
        message: message.to_string(),
        upstream_code: None,
    };
    mark_conversation_limited(
        &service,
        &id,
        &single("m-fatal"),
        &class,
        message,
        "custom-test",
    );
    assert_eq!(rotate::account_limit_reset_at(&service, &id, "m-fatal"), 0, "非限额分类不该落冷却");
}

#[test]
#[ignore = "reason: plugin-owned behavior — CodeArts daily-pool classification (InferHub.4291.200 → QuotaLimited with the next Beijing midnight) lives in the agent2api-providers plugin; the gateway unit suite only sees MissingPluginAdapter. Bookkeeping of a supplied QuotaLimited.reset_at is still covered by stateful_quota_bookkeeping_keeps_the_adapter_reset_at."]
fn codearts_daily_pool_envelope_cools_to_the_boundary_instead_of_ten_minutes() {
    use crate::server::errors::GatewayError;

    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    let now = crate::server::core::beijing::now();
    let tomorrow = now.date_naive().succ_opt().expect("次日有效");
    let midnight = tomorrow.and_hms_opt(0, 0, 0).unwrap()
        .and_local_timezone(crate::server::core::beijing::offset()).single().unwrap();
    let reset_at = midnight.timestamp_millis();
    let message = "日额度已用尽";
    let class = UpstreamErrorClass::QuotaLimited {
        reset_at: Some(reset_at),
        message: message.into(),
        upstream_code: Some(4291200),
        status: 403,
    };
    mark_conversation_limited(
        &service,
        &id,
        &single("glm-5.3-flash"),
        &class,
        message,
        "codearts",
    );

    let stored = rotate::account_limit_reset_at(&service, &id, "glm-5.3-flash");
    assert_eq!(stored, reset_at, "落库的恢复时刻必须是适配器算出的日池重置点");
    // ── 断言「是下一个北京零点」，而不是「距现在 > 10 分钟」（2026-10-13 修）──
    //
    // 原断言写的是 `minutes > 10`，本意是「不是 10 分钟兜底常量」；但它有个
    // 时变缺陷：日池重置点就是**下一个北京零点**，于是每天 23:50–00:00 这十分钟
    // 里「距零点」天然小于 10 分钟，用例必然变红（实测 2026-10-13 23:49 撞上）。
    // 那是测试写法的问题，不是实现的 —— 与兜底常量无关的失败被报成了兜底。
    //
    // 改成直接钉住真正的判据：恢复时刻必须落在**当天或次日的北京零点**上，
    // 且距现在不超过 24 小时（日池的语义就是「到零点重置」）。这样任何时刻
    // 跑都成立，也不再依赖「现在离零点有多远」。
    let minutes = (stored - logging::now_ms()) / 60_000;
    assert!(
        (0..=24 * 60).contains(&minutes),
        "日池的恢复时刻应当落在 24 小时以内，实际 {minutes} 分钟"
    );
    // 判据：恢复时刻在北京时区里必须是 00:00:00 —— 日池按天重置。
    // 10 分钟兜底常量（now + 10min）落在零点上的概率可忽略，这一条足以把它排除。
    let at = crate::server::core::beijing::date_time_of_ms(stored).expect("合法时间戳");
    use chrono::Timelike;
    assert!(
        at.hour() == 0 && at.minute() == 0 && at.second() == 0,
        "恢复时刻必须是北京零点（日池按天重置），实际 {}",
        at.format("%Y-%m-%d %H:%M:%S")
    );
}

#[test]
fn that_one_argument_is_the_divider_between_a_day_and_ten_minutes() {
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    // 同一个账号库里放两条账号：一条按「改后的写法」记账，一条按「改前的写法」
    // （同一个 rotate::mark_account_limited，只是恢复时刻写死 None）。
    // 同一份分类、同一句文案 —— 差别只有那一行实参。
    let pre_fix = service
        .store
        .add_account(
            &json!({"auth": {"accessToken": "at2"}, "account": {"uid": "quota-2"}}),
            None,
            None,
        )
        .expect("对照账号应当能建起来");
    let pre_fix_id = pre_fix["id"].as_str().expect("公开形态应带 id").to_string();

    let now = logging::now_ms();
    let reset_at = now + 3 * 60 * 60 * 1000;
    let message = "上游返回 403: insufficient quota";
    let class = UpstreamErrorClass::QuotaLimited {
        reset_at: Some(reset_at),
        message: message.to_string(),
        upstream_code: Some(4291200),
        status: 403,
    };
    mark_conversation_limited(
        &service,
        &id,
        &single("m-after"),
        &class,
        message,
        "custom-test",
    );
    // 改前的那一发：另三个字段照旧，恢复时刻写成 None
    rotate::mark_account_limited(
        &service,
        &pre_fix_id,
        "m-before",
        403,
        Some(4291200),
        None,
        message,
        "custom-test",
        None,
    );

    let after = rotate::account_limit_reset_at(&service, &id, "m-after");
    let before = rotate::account_limit_reset_at(&service, &pre_fix_id, "m-before");
    assert_eq!(after, reset_at, "改后应透传适配器给的恢复时刻");
    let before_minutes = (before - now) / 60_000;
    assert!(
        before_minutes >= 9 && before_minutes <= 11,
        "改前落的是存储层兜底冷却（10 分钟），实际 {before_minutes} 分钟"
    );
    // 分水岭本身：同一份分类结果，两种写法差出 2 小时以上 —— 白撞节拍就是从这里来的
    assert!(
        after - before > 2 * 60 * 60 * 1000,
        "透传与写死 None 的差距应当是「一整天 vs 10 分钟」量级，实际差 {} 分钟",
        (after - before) / 60_000
    );
}

// ── 分类结果 → rest 作用域（Task 1.3）───────────────────────────────────────
//
// 归类（`classify_quota_kind`）只回答「这是哪一类 429」；「停谁、停多久」是
// 编排层的事：余额 / 验证类**硬停整个账号**，额度 / 套餐 / 分钟限流只停**这个
// 模型**。前者进 `RouteState::account_rest`（选路硬跳过，不分模型），后者进
// `model_rest`（只跳过那一格）—— 写错表的症状是「一个模型的分钟限流把整账号
// 其他模型也关了」，或者反过来「余额耗尽只停一个模型、换个模型继续白撞」。
//
// 另外钉住两条**跨层一致性**（改前它们各说各话）：
//   · 内存 rest 与落库的 `rateLimits.resetAt` 必须是**同一个截止时刻**
//     （选路先查落库条目，不一致时内存那份就是摆设）；
//   · 落库/落内存的键必须是**上游真名**（请求名只在客户端那一侧成立）。

/// 账号级**硬**休息的截止时刻（没有记录时为 0）。
fn account_rest_until(service: &UpstreamService, account_id: &str) -> i64 {
    service
        .route
        .snapshot("", logging::now_ms(), 0)
        .account_resting
        .get(account_id)
        .copied()
        .unwrap_or(0)
}

/// 模型级 rest 的截止时刻（没有记录时为 0）。
fn model_rest_until(service: &UpstreamService, account_id: &str, model: &str) -> i64 {
    service
        .route
        .snapshot_for_model("", model, logging::now_ms(), 0)
        .model_resting
        .get(&(account_id.to_string(), model.to_string()))
        .copied()
        .unwrap_or(0)
}

/// 落库条目本身（`rateLimits[model]`；没有记录时为 `Null`）。
fn persisted_limit(service: &UpstreamService, account_id: &str, model: &str) -> Value {
    let snapshot = service.store.list_accounts();
    crate::server::core::routing::accounts_of(&snapshot)
        .iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
        .and_then(|account| account.get("rateLimits"))
        .and_then(|limits| limits.get(model))
        .cloned()
        .unwrap_or(Value::Null)
}

/// 用例串行跑：本模块改的是进程级路由设置快照（`config::update` 是非原子
/// 读改写），与 config / think_strip / provider_options 的用例共用同一把锁。
fn hold_config() -> std::sync::MutexGuard<'static, ()> {
    crate::server::core::provider_options::CONFIG_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 用例期间把 `routingShadowQuotaKind` 设成指定值，`Drop` 时恢复原值。
///
/// 影子模式是**进程级**配置：不恢复的话，任何后跑的用例都会读到本用例写下的
/// 值（本仓已经吃过一次全局状态污染的苦，见 `api::think_strip` 的 `RestoreThink`）。
struct ShadowQuotaKind {
    previous: bool,
}

impl ShadowQuotaKind {
    fn set(enabled: bool) -> Self {
        let previous = config::routing_settings().shadow_quota_kind;
        config::set_routing(config::RoutingPatch {
            shadow_quota_kind: Some(enabled),
            ..Default::default()
        });
        Self { previous }
    }
}

impl Drop for ShadowQuotaKind {
    fn drop(&mut self) {
        let _ = config::set_routing(config::RoutingPatch {
            shadow_quota_kind: Some(self.previous),
            ..Default::default()
        });
    }
}

/// 余额不足 ⇒ **账号级硬休息**（默认 24 小时）、不写模型 rest，且落库的
/// `resetAt` 与内存是同一个时刻。
///
/// 反了会怎样：只停一个模型时，同一账号换一个模型继续打，仍然每次都被
/// 「余额不足」拒 —— 一轮对话里白撞 N 次；而落库停在 10 分钟兜底时，选路按
/// 落库那条放行，内存的 24 小时只是软垫底（照样会被选中）。
#[test]
fn quota_rest_credit_stops_the_account_not_the_model() {
    // Task 1.4：这些用例钉的是「关掉影子模式」后的完整 rest 行为。
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    rotate::mark_account_limited(
        &service,
        &id,
        "m-credit",
        429,
        None,
        None,
        "上游返回 429: 余额不足或无可用资源包",
        "custom-credit",
        None,
    );
    let until = account_rest_until(&service, &id);
    let hours = (until - now) / 3_600_000;
    assert!(
        (23..=25).contains(&hours),
        "余额类应当硬停整个账号约 24 小时，实际 {hours} 小时"
    );
    assert_eq!(
        model_rest_until(&service, &id, "m-credit"),
        0,
        "余额类不该再写一条模型级 rest"
    );
    assert_eq!(
        rotate::account_limit_reset_at(&service, &id, "m-credit"),
        until,
        "落库的 resetAt 必须与内存的账号级休息同一个截止时刻"
    );
}

/// 需要人机验证 ⇒ 与余额类同档：**账号级硬休息**（不是模型级）。
#[test]
fn quota_rest_verification_also_stops_the_whole_account() {
    // Task 1.4：这些用例钉的是「关掉影子模式」后的完整 rest 行为。
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    rotate::mark_account_limited(
        &service,
        &id,
        "m-captcha",
        429,
        None,
        None,
        "上游返回 429: please complete the captcha to continue",
        "custom-verify",
        None,
    );
    assert!(
        account_rest_until(&service, &id) > 0,
        "验证类应当硬停整个账号"
    );
    assert_eq!(
        model_rest_until(&service, &id, "m-captcha"),
        0,
        "验证类不该只停一个模型"
    );
}

/// 成功转发一次 ⇒ **立即**解除账号级硬休息（不等 24 小时截止）。
///
/// 三条成功出口（自定义 / 会话式 / 无状态）共用 `note_route_success`。
/// 没有这条解除路径时：充值到账、验证过完、或者当初那条 429 本就是误判，
/// 账号仍被硬停到截止时刻 —— 期间所有模型都被 `plan_route` 跳过，只能重启进程。
#[test]
fn a_successful_forward_lifts_the_account_rest() {
    // Task 1.4：这些用例钉的是「关掉影子模式」后的完整 rest 行为。
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    rotate::mark_account_limited(
        &service,
        &id,
        "m-credit",
        429,
        None,
        None,
        "上游返回 429: 余额不足或无可用资源包",
        "custom-credit",
        None,
    );
    assert!(
        account_rest_until(&service, &id) > 0,
        "前置：余额类应当硬停账号"
    );
    note_route_success(&service, &id, "m-credit");
    assert_eq!(
        account_rest_until(&service, &id),
        0,
        "成功一次应当立即解除账号级硬休息"
    );
}

/// 账号页「清除限流」同样解除内存里的硬休息（不只是落库的 `rateLimits`）。
///
/// 端点清的是落库条目，硬休息在进程内存的另一张表；不一起清的话按钮看起来
/// 「点了没反应」（转发仍被硬跳过）。
#[test]
fn the_manual_clear_lifts_the_account_rest() {
    // Task 1.4：这些用例钉的是「关掉影子模式」后的完整 rest 行为。
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    rotate::mark_account_limited(
        &service,
        &id,
        "m-credit",
        429,
        None,
        None,
        "上游返回 429: 余额不足或无可用资源包",
        "custom-credit",
        None,
    );
    assert!(
        account_rest_until(&service, &id) > 0,
        "前置：余额类应当硬停账号"
    );
    // 账号页「清除限流」→ `api::accounts::clear_rate_limits` 调的正是这个
    service.clear_account_rest(&id);
    assert_eq!(
        account_rest_until(&service, &id),
        0,
        "手动清除应当解除账号级硬休息"
    );
}

/// 分钟级限流 ⇒ **模型级**短 rest（`retryAfterMs` 或默认 60 秒），账号不休息；
/// 落库的 `resetAt` 也必须是这 60 秒，而不是存储层的 10 分钟兜底。
///
/// 反了会怎样：一次「20 requests per minute」把整个账号停 24 小时（最贵的误判），
/// 或者内存说 60 秒、落库说 10 分钟 —— 选路按落库那条走，实际就是 10 分钟。
#[test]
fn quota_rest_rate_stops_only_the_model_for_a_minute() {
    // Task 1.4：这些用例钉的是「关掉影子模式」后的完整 rest 行为。
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    rotate::mark_account_limited(
        &service,
        &id,
        "m-rate",
        429,
        None,
        None,
        "上游返回 429: Rate limit exceeded: 20 requests per minute",
        "openrouter",
        Some(60_000),
    );
    let until = model_rest_until(&service, &id, "m-rate");
    let ms = until - now;
    assert!(
        (59_000..=61_000).contains(&ms),
        "分钟级限流应当只停这个模型约 60 秒，实际 {ms} 毫秒"
    );
    assert_eq!(
        account_rest_until(&service, &id),
        0,
        "分钟级限流不该让整个账号休息"
    );
    let persisted = rotate::account_limit_reset_at(&service, &id, "m-rate");
    assert_eq!(
        persisted, until,
        "落库的 resetAt 必须与内存的模型级 rest 同一个截止时刻"
    );
    assert!(
        persisted < now + 5 * 60 * 1000,
        "落库不该退回 10 分钟兜底（实际 {} 秒）",
        (persisted - now) / 1000
    );
}

/// 额度类 + 上游给了重置时刻 ⇒ 模型级 rest **停到那一刻**（不是 10 分钟兜底），
/// 落库同一个值。
///
/// 反了会怎样：日池 / 套餐额度的恢复时刻被兜底常量覆盖，账号按 10 分钟的节拍
/// 反复去撞一个当天不会再恢复的模型（生产实测两小时白撞 8 次）。
#[test]
fn quota_rest_quota_with_reset_stops_the_model_until_then() {
    // Task 1.4：这些用例钉的是「关掉影子模式」后的完整 rest 行为。
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    let reset_at = now + 3 * 60 * 60 * 1000;
    rotate::mark_account_limited(
        &service,
        &id,
        "m-quota",
        429,
        None,
        Some(reset_at),
        "上游返回 429: too many requests",
        "custom-quota",
        None,
    );
    assert_eq!(
        model_rest_until(&service, &id, "m-quota"),
        reset_at,
        "额度类的模型 rest 应当停到上游给的重置时刻"
    );
    assert_eq!(
        account_rest_until(&service, &id),
        0,
        "额度类只停模型，不停整个账号"
    );
    assert_eq!(
        rotate::account_limit_reset_at(&service, &id, "m-quota"),
        reset_at,
        "落库的 resetAt 与内存的模型级 rest 必须一致"
    );
}

// ── 影子模式开关（Task 1.4）───────────────────────────────────────────────
//
// Task 1.3 的新作用域在真流量上的判据还没攒够：分类错一次的代价是「整个账号
// 被停一天」或「只停一个模型、换个模型继续白撞」。`routingShadowQuotaKind`
// 默认开，让 `rateLimits` 先把分类结果（reason / scope）记下来供观察，而选路
// 状态保持升级前的行为；关掉后才按分类真正写 rest。

/// 默认（影子开）：**只落 `rateLimits`**，不写账号级 / 模型级 rest。
///
/// 反了会怎样：升级即全量切换 —— 一个尚未验证的分类判据直接改变所有账号的
/// 选路，且用户没有任何开关可以退回去。
#[test]
fn shadow_mode_records_the_limit_without_touching_route_state() {
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(true);
    let (_dir, service, id) = temp_service();
    let now = logging::now_ms();
    rotate::mark_account_limited(
        &service,
        &id,
        "m-shadow-credit",
        429,
        None,
        None,
        "上游返回 429: 余额不足或无可用资源包",
        "custom-credit",
        None,
    );
    // 落库条目仍要带分类字段：影子模式观察的正是这些值
    let entry = persisted_limit(&service, &id, "m-shadow-credit");
    assert!(!entry.is_null(), "影子模式下 rateLimits 仍必须落库");
    assert_eq!(
        entry.get("reason").and_then(Value::as_str),
        Some("credit"),
        "reason 必须记下来（影子模式的观察对象）"
    );
    assert_eq!(
        entry.get("scope").and_then(Value::as_str),
        Some("account"),
        "scope 必须记下来（影子模式的观察对象）"
    );
    let persisted = rotate::account_limit_reset_at(&service, &id, "m-shadow-credit");
    assert!(persisted > now, "落库的 resetAt 必须是未来时刻");
    let minutes = (persisted - now) / 60_000;
    assert!(
        (9..=11).contains(&minutes),
        "影子模式落库必须沿用旧的 10 分钟兜底（而不是分类的 24 小时），实际 {minutes} 分钟"
    );
    assert_eq!(
        account_rest_until(&service, &id),
        0,
        "影子模式下不许写账号级 rest（选路保持旧行为）"
    );
    assert_eq!(
        model_rest_until(&service, &id, "m-shadow-credit"),
        0,
        "影子模式下不许写模型级 rest（选路保持旧行为）"
    );
}

/// 关掉影子模式：分类重新决定作用域 —— 余额类写账号级 rest，分钟限流写模型级。
#[test]
fn turning_shadow_off_restores_the_classified_rest_scopes() {
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);

    let (_credit_dir, credit_service, credit_id) = temp_service();
    rotate::mark_account_limited(
        &credit_service,
        &credit_id,
        "m-shadow-credit",
        429,
        None,
        None,
        "上游返回 429: 余额不足或无可用资源包",
        "custom-credit",
        None,
    );
    assert!(
        account_rest_until(&credit_service, &credit_id) > 0,
        "关掉影子模式后，余额类必须硬停整个账号"
    );
    assert_eq!(
        model_rest_until(&credit_service, &credit_id, "m-shadow-credit"),
        0,
        "余额类仍不该写模型级 rest"
    );

    let (_rate_dir, rate_service, rate_id) = temp_service();
    let now = logging::now_ms();
    rotate::mark_account_limited(
        &rate_service,
        &rate_id,
        "m-shadow-rate",
        429,
        None,
        None,
        "上游返回 429: Rate limit exceeded: 20 requests per minute",
        "openrouter",
        Some(30_000),
    );
    let until = model_rest_until(&rate_service, &rate_id, "m-shadow-rate");
    let ms = until - now;
    assert!(
        (29_000..=31_000).contains(&ms),
        "关掉影子模式后，分钟限流应当只停这个模型约 30 秒，实际 {ms} 毫秒"
    );
    assert_eq!(
        account_rest_until(&rate_service, &rate_id),
        0,
        "分钟限流不该让整个账号休息"
    );
}

/// 200 + 正文配额拒绝的记账必须落在**上游真名**上（不是客户端请求名）。
///
/// 反了会怎样：`rateLimits` 写在一个没人读的键上 —— 判定侧
/// （`plan_route` → `rate_limit_reset_at`）与清除侧（成功转发后的
/// `cap_cleared`）都按真名查，于是「标记了却永不生效、也永不清除」，
/// 只能人工去账号页清。改前正是这样（传的是 `&model`）。
#[test]
fn body_refusal_bookkeeping_is_keyed_by_the_wire_model() {
    // Task 1.4：这些用例钉的是「关掉影子模式」后的完整 rest 行为。
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    let reset_at = logging::now_ms() + 30 * 60 * 1000;
    // 请求名 `alias` 在这一家实际发出 `wire-model`（映射生效时两者不同）
    mark_body_refusal_limited(
        &service,
        &id,
        "wire-model",
        Some(reset_at),
        "上游返回 200: 额度耗尽",
        "custom-refusal",
    );
    assert_eq!(
        model_rest_until(&service, &id, "wire-model"),
        reset_at,
        "模型级 rest 必须记在真名上"
    );
    assert_eq!(
        model_rest_until(&service, &id, "alias"),
        0,
        "请求名上不该有任何 rest（没人按它查）"
    );
    assert_eq!(
        rotate::account_limit_reset_at(&service, &id, "wire-model"),
        reset_at,
        "落库条目也必须记在真名上"
    );
    assert_eq!(
        rotate::account_limit_reset_at(&service, &id, "alias"),
        0,
        "请求名上不该有落库条目"
    );
}

/// 调用点本身也钉一次：`attempt_queue` 的正文拒绝分支必须把 `&wire_model`
/// 交给记账函数。
///
/// ── 为什么这里读源码 ─────────────────────────────────────────
/// 上一条件用覆盖的是「记账函数按真名落键」，覆盖不到「调用点传了谁」——
/// 而 Finding 1 的全部内容就是调用点传了 `&model`。这条路径又没法端到端跑：
/// 正文配额标记由插件适配器声明（`stream_refusal_markers`），网关内置适配器
/// 一个都不声明，测试进程里没有能触发它的适配器。于是退一步钉住那一行实参：
/// 它一旦改回请求名，这条用例立刻红，而不是等生产上「标了却永不生效」。
#[test]
fn the_refusal_call_site_passes_the_wire_model() {
    let source = include_str!("../provider_loop.rs");
    let call = source
        .split("let reset_text = mark_body_refusal_limited(")
        .nth(1)
        .expect("正文拒绝分支应当调用 mark_body_refusal_limited");
    let args = &call[..call.find(");").expect("调用应当以 ); 结束")];
    assert!(
        args.contains("&wire_model"),
        "调用点必须传上游真名（判定侧按它查）：{args}"
    );
    assert!(
        !args.contains("&model,"),
        "调用点不能传客户端请求名（没人按它查）：{args}"
    );
}

/// 调用点同样钉一次：自定义提供商分支必须传**解析后的厂商身份**，不是 id 本身。
///
/// 理由与上一条同款（读源码，因为这条分支要真实的自定义家配置与出网替身）：
/// 解析本身由 `custom_zhipu_vendor_resolves_to_the_balance_scheme` 覆盖，这里
/// 只钉「调用点有没有用它」。改回 `&custom_provider_id` 时智谱 1113 特例对
/// 自定义家重新变成死代码，而任何单测都不会红 —— 所以在这里红。
#[test]
fn the_custom_call_site_resolves_the_vendor_identity() {
    let source = include_str!("../provider_loop.rs");
    // 文件里第一处 `rotate::mark_account_limited(` 就是自定义分支
    let call_start = source
        .find("rotate::mark_account_limited(")
        .expect("自定义分支应当记账");
    let call = &source[call_start..];
    let args = &call[..call.find(");").expect("调用应当以 ); 结束")];
    assert!(
        args.contains("&vendor"),
        "自定义分支应当传解析后的厂商身份：{args}"
    );
    let window = &source[call_start.saturating_sub(400)..call_start];
    assert!(
        window.contains("let vendor = rotate::vendor_identity(&custom_provider_id);"),
        "厂商身份必须由余额方案解析（否则智谱 1113 特例对自定义家永远不命中）"
    );
}

/// 自定义家的厂商身份先解析成**余额方案**（`zhipu`），智谱 1113 特例才活着。
///
/// 反了会怎样：自定义 provider id 是 `custom-0c8cd27f7e10` 这类用户自取的串，
/// `ZHIPU_VENDORS` 永远不匹配 ⇒「业务码 1113 + 文案里没有关键词」的余额耗尽
/// 会被判成 `Quota`（只停一个模型），账号换个模型继续被拒。
#[test]
fn custom_zhipu_vendor_resolves_to_the_balance_scheme() {
    use crate::server::core::custom_providers;
    use crate::server::core::providers::error_kind::{classify_quota_kind, QuotaKind};

    let key = custom_providers::KEY_CUSTOM_PROVIDERS;
    let previous = crate::server::config::with_raw(|raw| raw.get(key).cloned());
    crate::server::config::update_raw_field(
        key,
        json!([{ "id": "custom-zhipu-test", "name": "智谱", "balanceScheme": "zhipu" }]),
    );

    // 内置家：身份就是 provider id 本身
    assert_eq!(rotate::vendor_identity("workbuddy"), "workbuddy");
    // 自定义家：先问余额方案
    let vendor = rotate::vendor_identity("custom-zhipu-test");
    assert_eq!(vendor, "zhipu", "自定义家的厂商身份应当是余额方案");
    assert_eq!(
        classify_quota_kind(429, Some(1113), "", &vendor),
        QuotaKind::Credit,
        "解析后的厂商身份必须让智谱 1113 判成 Credit（账号级）"
    );
    // 没接余额查询的自定义家：回落 provider id 本身
    assert_eq!(rotate::vendor_identity("custom-plain"), "custom-plain");

    // 还原全局配置（本用例只借这一次读；别的用例可能同时读同一个键）
    match previous {
        Some(Some(value)) => {
            crate::server::config::update_raw_field(key, value);
        }
        _ => {
            crate::server::config::update_raw_field(key, json!([]));
        }
    }
}

const APIG_TOO_LARGE_BODY: &str = r#"{"error_msg":"Request entity too large","error_code":"APIG.0201","request_id":"apig-test"}"#;

fn fatal(status: u16) -> UpstreamErrorClass {
    UpstreamErrorClass::Fatal {
        status,
        message: format!("上游返回 {status}"),
        upstream_code: None,
    }
}

/// 413 说的是「这份字节太大」，重发一遍没有任何会变的量 ⇒ 不消耗原地重发预算。
/// 改造前它落到兜底那一档，三次 ×5 秒间隔 + 每次约 13 秒往返 = 客户端的 60 秒
/// 等待墙先到，面板只剩「408 客户端在响应完成前断开」，真因（413）埋在明细里。
#[test]
fn a_payload_size_rejection_gets_no_in_place_retry() {
    assert!(
        PAYLOAD_SIZE_REJECTION_STATUSES.contains(&413),
        "413 应当在「按体积拒收」名单里"
    );
    assert!(
        fallback_retry_advice(&fatal(413), 3, 413).is_none(),
        "预算还剩 3 次也不该给 413 提原地重发"
    );
}

/// 反对照：其它 4xx/未知失败照旧走兜底原地重发（这条改动只放开"体积拒收"那一格，
/// 没有把 400 这类"含义由上游决定"的码一并排除）。404 已于 issue #255 单独豁免
/// （路由 / 模型名类确定性失败，见 `DETERMINISTIC_NOT_FOUND_STATUSES`），不在此列。
#[test]
fn other_fatal_statuses_still_get_the_fallback_resend() {
    for status in [400u16, 409] {
        assert!(
            fallback_retry_advice(&fatal(status), 3, status).is_some(),
            "HTTP {status} 的语义由上游决定，兜底原地重发的既有行为不该变"
        );
    }
    assert!(
        !TRANSIENT_RETRY_STATUSES.contains(&413),
        "413 不该被当成瞬时抖动（否则瞬时那一档会把它重发回去）"
    );
}

/// 端到端：打到真实返回 413 的替身上身，**只发一次**（改造前是 1 + 3 次）。
#[tokio::test]
async fn a_real_413_response_is_sent_once_and_handed_back() {
    let (transport, hits, task) =
        mock_upstream(413, "application/json; charset=utf-8", APIG_TOO_LARGE_BODY).await;
    let mut budget = RetryBudget::new(3);
    let capture = crate::server::core::debug_traffic::TrafficCapture::begin("payload-too-large");
    let telemetry = Arc::new(RequestTelemetry::new());
    // 适配器只提供「怎么判上游错误」这一半；413 的处置在**编排层**
    // （`transport::RetryBudget` + `fallback_retry_advice`），因此随便挑一个
    // 内置家即可。原先这里用 ZCode 的实例，它随实现搬到插件了 —— 换成本家
    // 走得通的 WorkBuddy，断言的仍是同一处语义。
    let result = send_with_retry(
        adapter_for(ProviderKind::WorkBuddy),
        &transport,
        &mut budget,
        Some(&capture),
        &telemetry,
        false,
        None,
        None,
    )
    .await;
    task.abort();
    let failure = match result {
        Ok(_) => panic!("HTTP 413 不该被当成成功流"),
        Err(failure) => failure,
    };
    assert_eq!(
        1,
        hits.load(Ordering::SeqCst),
        "体积拒收的失败应当一次就交回，不该原地重发"
    );
    assert!(
        matches!(failure.class, UpstreamErrorClass::Fatal { status: 413, .. }),
        "状态码 413 要原样带回，不该被折成别的失败"
    );
}

#[test]
fn a_deterministic_404_skips_the_in_place_resend() {
    // issue #255 的形状：OfficeAce 对目录里有、套餐里没有的名字回
    // `404 {"code":null,"message":"Invalid model."}`，旧判据把它当普通 Fatal，
    // 原地重发 3 次 × 5 秒才换号——客户端白等 15 秒，换来的还是同一个 404。
    let not_found = UpstreamErrorClass::Fatal {
        status: 404,
        upstream_code: None,
        message: "上游返回 404: {\"code\":null,\"message\":\"Invalid model.\"}".to_string(),
    };
    assert!(
        fallback_retry_advice(&not_found, 3, 404).is_none(),
        "名字类确定性失败不该再原地重发"
    );
    // 对照：普通 400 仍走原地重发（400 的语义由上游定，重发是安全默认）
    let bad_request = UpstreamErrorClass::Fatal {
        status: 400,
        upstream_code: None,
        message: "上游返回 400: whatever".to_string(),
    };
    assert!(
        fallback_retry_advice(&bad_request, 3, 400).is_some(),
        "404 的豁免不许顺手把 400 也豁免了"
    );
}

// ── 逐家开关的读取口径（`frame_policy_of`）────────────────────────────
//
// 回归背景：`stripNewlineKeepalive` 早就在 `provider_options::KNOWN_KEYS` 里登记，
// 写侧返回 `Ok` 并记一条成功日志，但转发时只读全局键 —— 用户逐家配了、什么都没
// 发生、也没有任何提示（与 7f4026be 修过的 think-tags 是同一类缺陷）。
//
// 这两条钉的是**读取口径**，不是「某个值等于某个常量」：
//   - 能力位声明的家：逐家值必须盖过全局值（两个方向都测，防止只加了一个 `||`）；
//   - 没声明能力位的家：配成 true 也不许丢帧（`declared && switch_on` 的收窄语义）。

/// 用例串行跑：本组改的是进程级配置快照，与 `provider_options` 的用例共用一把锁。
fn hold_provider_options() -> std::sync::MutexGuard<'static, ()> {
    crate::server::core::provider_options::CONFIG_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 快照未装入时补一个空的 `providerOptions`（`set()` 把「未初始化」当硬错误）。
fn ensure_provider_options_snapshot() {
    if crate::server::config::with_raw(|_| ()).is_some() {
        return;
    }
    let _ = crate::server::config::update_raw_field(
        "providerOptions",
        serde_json::Value::Object(serde_json::Map::new()),
    );
}

/// 用例结束后把 `providerOptions` 恢复原样（Drop 保证 panic 时也执行）。
struct RestoreProviderOptions {
    previous: Option<serde_json::Value>,
}

impl RestoreProviderOptions {
    fn capture() -> Self {
        Self {
            previous: crate::server::config::with_raw(|raw| raw.get("providerOptions").cloned())
                .flatten(),
        }
    }
}

impl Drop for RestoreProviderOptions {
    fn drop(&mut self) {
        let value = self
            .previous
            .clone()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let _ = crate::server::config::update_raw_field("providerOptions", value);
    }
}

/// 迁移后的测试替身：只声明网关侧要读的那一项能力位。
///
/// `adapter_for` 现在对任何 kind 都返回 `MissingPluginAdapter`（真实现全在
/// `agent2api-providers` 插件里）。下面两条用例钉的是**网关读取口径**
/// （能力位 && 逐家开关），与哪家实现无关，所以用一个最小替身声明能力位，
/// 而不是把内置实现搬回网关。
struct CapabilityTestAdapter {
    kind: ProviderKind,
    strip_newline_chunks: bool,
}

impl ProviderAdapter for CapabilityTestAdapter {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn list_models(&self) -> Vec<Value> {
        Vec::new()
    }

    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        UpstreamErrorClass::Fatal {
            status,
            message: error_body.to_string(),
            upstream_code: None,
        }
    }

    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &axum::http::HeaderMap,
    ) -> Result<crate::server::core::providers::adapter::ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(501, "测试替身不构造上游请求"))
    }

    fn sse_strip_newline_chunks(&self) -> bool {
        self.strip_newline_chunks
    }

    fn ensure_access_token<'a>(
        &'a self,
        _store: &'a crate::server::core::account_store::AccountStore,
        _account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { Err(GatewayError::with_status(501, "测试替身不取凭证")) })
    }

    fn refresh_models<'a>(
        &'a self,
        _store: &'a crate::server::core::account_store::AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = crate::server::core::providers::adapter::ModelRefreshOutcome,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            crate::server::core::providers::adapter::ModelRefreshOutcome::failed(
                "测试替身不刷新模型目录",
            )
        })
    }
}

#[test]
fn a_declared_provider_honours_its_own_newline_switch_over_the_global_one() {
    let _guard = hold_provider_options();
    let _restore = RestoreProviderOptions::capture();
    ensure_provider_options_snapshot();

    // 用替身声明 `sse_strip_newline_chunks`（能力位），模拟插件家「开关真能
    // 生效」的那一类。断言写成**相对全局默认值的差分形式**，不依赖环境变量：
    // 逐家值 = !全局 ⇒ 策略必须 = !全局；逐家值 = 全局 ⇒ 策略必须 = 全局。
    let adapter = CapabilityTestAdapter {
        kind: ProviderKind::WorkBuddy,
        strip_newline_chunks: true,
    };
    assert!(
        adapter.sse_strip_newline_chunks(),
        "前提：替身声明了能力位"
    );
    let provider_id = crate::server::core::providers::kind_id(adapter.kind());
    let key = crate::server::config::KEY_STRIP_NEWLINE_KEEPALIVE;
    let global = crate::server::config::strip_newline_keepalive();

    crate::server::core::provider_options::set(provider_id, key, !global)
        .expect("登记过的键必须能写");
    assert_eq!(
        frame_policy_of(&adapter, "m").strip_newline_chunks,
        !global,
        "逐家开关必须盖过全局值（只读全局的话这里会等于 {global}）"
    );

    crate::server::core::provider_options::set(provider_id, key, global)
        .expect("登记过的键必须能写");
    assert_eq!(
        frame_policy_of(&adapter, "m").strip_newline_chunks,
        global,
        "逐家值等于全局值时也要如实装配"
    );
}

#[test]
fn an_undeclared_provider_cannot_expand_into_newline_stripping() {
    let _guard = hold_provider_options();
    let _restore = RestoreProviderOptions::capture();
    ensure_provider_options_snapshot();

    // ZCode 没声明能力位：逐家配 true 也不许开始丢帧 —— 开关只能收窄、不能扩张。
    let adapter = adapter_for(ProviderKind::Zcode);
    assert!(
        !adapter.sse_strip_newline_chunks(),
        "前提：ZCode 不声明能力位"
    );
    let provider_id = crate::server::core::providers::kind_id(adapter.kind());
    let key = crate::server::config::KEY_STRIP_NEWLINE_KEEPALIVE;

    crate::server::core::provider_options::set(provider_id, key, true).expect("登记过的键必须能写");
    assert!(
        !frame_policy_of(adapter, "m").strip_newline_chunks,
        "没有能力位的家配了开关也不许丢帧"
    );
}

// ── 滚动 RPM 的发送前闸门（Task 2.3）────────────────────────────────────────
//
// 选路层的 `rpm_blocked_accounts` 只**查**窗口（不占名额），真正的占名额发生
// 在发送前这一道闸门里。分开的理由：一次请求可能选路多次（429 顺延、粘性命中、
// 队列面板预演），若在选路里 `allow`，一条请求会吃掉好几份配额，而且面板刷新
// 也会偷偷消耗额度。
//
// 这条用例钉住三件事：① 打满后返回 429；② 带 `Retry-After`（秒，向上取整）；
// ③ 被拒的那次**不记账**（不会越拒越长），且 `maxRpm == 0` 恒放行。

#[test]
fn rpm_send_time_refusal_is_429_with_retry_after() {
    use axum::http::header::RETRY_AFTER;
    use crate::server::core::upstream::rpm::RpmWindow;

    let rpm = RpmWindow::new();
    let account = json!({"id": "a", "provider": "p", "enabled": true, "maxRpm": 1});
    // 第一次：占一个名额
    assert!(
        rpm_gate(&rpm, Some("a"), Some(&account), 1_000).is_ok(),
        "窗口空着时应当放行"
    );
    // 同一分钟内第二次：拒绝
    let error = rpm_gate(&rpm, Some("a"), Some(&account), 2_000)
        .expect_err("打满后必须拒绝");
    assert_eq!(error.status_code, 429, "RPM 超限是 429");
    // 1_000ms 那次命中要到 61_000 才滑出，此刻还差 59_000ms → 59 秒
    assert_eq!(
        error.retry_after_secs,
        Some(59),
        "Retry-After 必须是窗口剩余秒数（向上取整）"
    );
    let response = error.payload_response();
    assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers().get(RETRY_AFTER).and_then(|value| value.to_str().ok()),
        Some("59"),
        "响应必须带 Retry-After 头"
    );
    // 被拒的那次不记账：过了一分钟窗口，名额恰好回来一个（不是被拒次数拖长）
    assert!(
        rpm_gate(&rpm, Some("a"), Some(&account), 61_000).is_ok(),
        "窗口滚动后名额应当回来"
    );
    // 不限速（maxRpm 缺键 = 0）恒放行，也不留痕
    let unlimited = json!({"id": "b", "provider": "p", "enabled": true});
    for now in 0..5 {
        assert!(rpm_gate(&rpm, Some("b"), Some(&unlimited), now).is_ok());
    }
    assert_eq!(rpm.wait_ms("b", 0, 10), 0);
}

// ── 储备账号在循环里的次序（Task 2.3）──────────────────────────────────────
//
// 计划层（`plan_route`）已经把 `reserve` 全局后置；这里钉的是**循环层**：
// 第一个候选是 kept（成功即返回，不会碰储备），只有 kept 进了 `tried_ids`
// （失败顺延）之后，储备才会被提升并交出来。

#[test]
fn reserve_is_only_reached_after_the_kept_account_is_tried() {
    // 用一个自定义 provider id（`custom-` 前缀）：内置家要装了插件才算「可见」，
    // 而本用例只关心队列次序，不该依赖测试环境里装没装插件。
    const PROVIDER: &str = "custom-loop-test";
    let (_dir, service, _seed) = temp_service();
    // 先加 kept、再加 reserve：`add_account` 按加入顺序排优先级（队尾），
    // 于是 kept 是队首、reserve 是第 2 位。
    let kept = service
        .store
        .add_account(
            &json!({"auth": {"accessToken": "at1"}, "account": {"uid": "kept-1"}}),
            None,
            Some(PROVIDER),
        )
        .expect("kept 账号应当能建起来")["id"]
        .as_str()
        .expect("公开形态应带 id")
        .to_string();
    let second = service
        .store
        .add_account(
            &json!({"auth": {"accessToken": "at2"}, "account": {"uid": "reserve-2"}}),
            None,
            Some(PROVIDER),
        )
        .expect("第二个账号应当能建起来");
    let reserve = second["id"].as_str().expect("公开形态应带 id").to_string();
    // 同一家两个账号、maxAccounts = 1：队首 kept 可用，后面的才是储备
    for id in [&kept, &reserve] {
        service
            .store
            .update_account(id, &json!({"maxAccounts": 1}))
            .expect("上限应当能写进去");
    }
    let keys = rotate::CooldownKeys::new("m");
    let providers = [PROVIDER];

    let first = rotate::pick_next_account(&service, &providers, &keys, &[], None)
        .expect("队首应当有可用账号");
    assert_eq!(
        first.get("id").and_then(Value::as_str),
        Some(kept.as_str()),
        "第一个候选必须是 kept（它成功就返回，轮不到储备）"
    );

    let after_failure =
        rotate::pick_next_account(&service, &providers, &keys, &[kept.clone()], None)
            .expect("kept 失败顺延后储备应当被提升");
    assert_eq!(
        after_failure.get("id").and_then(Value::as_str),
        Some(reserve.as_str()),
        "kept 已试过（失败）后才轮到储备"
    );
}



// ── 模型级 rest 的解除路径（deferred minor）──────────────────────────────
//
// 模型级 rest 原先只有写入路径：上游提前恢复 / 分类误判之后，账号仍会被
// `plan_route` 钉到截止时刻。成功一次必须能立即解除这一格。

#[test]
fn a_successful_forward_lifts_the_model_rest() {
    let _guard = hold_config();
    let _shadow = ShadowQuotaKind::set(false);
    let (_dir, service, id) = temp_service();
    rotate::mark_account_limited(
        &service,
        &id,
        "m-rate",
        429,
        None,
        None,
        "上游返回 429: Rate limit exceeded: 20 requests per minute",
        "openrouter",
        Some(30_000),
    );
    assert!(
        model_rest_until(&service, &id, "m-rate") > 0,
        "前置：分钟限流应当写模型级 rest"
    );
    note_route_success(&service, &id, "m-rate");
    assert_eq!(
        model_rest_until(&service, &id, "m-rate"),
        0,
        "成功一次应当立即解除这个模型的 rest"
    );
}

// ── 账号级硬休息不被兜底选择器绕过（I2）──────────────────────────────────
//
// 第一级选路本来就跳过硬休息账号；但「全部冷却 / 并发满」时的两级兜底曾直接从
// `enabled` 里挑，于是余额耗尽 / 待验证的账号会被重新塞回发送队列。这里把两个
// 候选都标成冷却（迫使进入兜底），再给其中一个加硬休息：兜底必须挑另一个。

#[tokio::test]
async fn the_last_resort_selector_skips_hard_resting_accounts() {
    const PROVIDER: &str = "custom-loop-rest-test";
    let _guard = hold_config();
    let (_dir, service, _seed) = temp_service();
    let add = |uid: &str, token: &str| {
        service
            .store
            .add_account(
                &json!({"auth": {"accessToken": token}, "account": {"uid": uid}}),
                None,
                Some(PROVIDER),
            )
            .expect("账号应当能建起来")["id"]
            .as_str()
            .expect("公开形态应带 id")
            .to_string()
    };
    let rested = add("rested-1", "at-rested");
    let healthy = add("healthy-2", "at-healthy");
    // 两个都进入「限额冷却」：第一级选路会全部跳过，迫使走兜底选择器。
    for id in [&rested, &healthy] {
        rotate::mark_account_limited(
            &service,
            id,
            "m-rest",
            429,
            None,
            None,
            "上游返回 429: Rate limit exceeded: 20 requests per minute",
            "openrouter",
            Some(30_000),
        );
    }
    // 只给其中一个加账号级硬休息（余额耗尽类）。
    service
        .route
        .account_rest_mark(&rested, logging::now_ms() + 24 * 3600 * 1000);

    let keys = rotate::CooldownKeys::new("m-rest");
    let providers = [PROVIDER];
    let routing = crate::server::core::upstream::route_context::RequestRouting::capture("m-rest", &service.store);
    let target = rotate::select_target_account(&service, &providers, &keys, &[], None, "", &routing)
        .await
        .expect("兜底应当能选出未被硬休息的账号");
    assert_eq!(
        target.account_id.as_deref(),
        Some(healthy.as_str()),
        "硬休息账号不能被兜底选择器重新选中"
    );

    // 钉住（模型测试）是既有例外：点名硬休息账号时仍要让它发一次。
    let pinned = rotate::select_target_account(
        &service,
        &providers,
        &keys,
        &[],
        Some(&rested),
        "",
        &routing,
    )
    .await
    .expect("钉住的账号应当照常被选中");
    assert_eq!(
        pinned.account_id.as_deref(),
        Some(rested.as_str()),
        "pinned 语义不受硬休息过滤影响"
    );
}

// ── 有状态 / 插件 provider 的出站脱敏门禁（C1）────────────────────────────
//
// 会话式 provider 的请求体由适配器直接发上游，编排层挂不上 `MaskSession`，
// 回复侧也没有还原点。脱敏开启时必须**拒绝发送**（503、不可重试），而不是把
// 明文交给适配器。这条用例用有状态 mock 适配器钉住「503 + 一次都没发」。

/// 有状态 mock：`forward_conversation` 只计数，被调到就说明门禁漏了。
struct StatefulMaskProbe {
    calls: Arc<AtomicUsize>,
}

impl ProviderAdapter for StatefulMaskProbe {
    fn kind(&self) -> ProviderKind {
        ProviderKind::WorkBuddy
    }

    fn label(&self) -> &'static str {
        "有状态测试家"
    }

    fn list_models(&self) -> Vec<Value> {
        Vec::new()
    }

    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &axum::http::HeaderMap,
    ) -> Result<crate::server::core::providers::adapter::ChatRequestPlan, GatewayError> {
        Err(GatewayError::new("有状态测试家不该走无状态构造"))
    }

    fn classify_error(&self, status: u16, _error_body: &Value) -> UpstreamErrorClass {
        UpstreamErrorClass::Fatal {
            status,
            message: "测试分类".to_string(),
            upstream_code: None,
        }
    }

    fn ensure_access_token<'a>(
        &'a self,
        _store: &'a crate::server::core::account_store::AccountStore,
        _account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(String::new()) })
    }

    fn refresh_models<'a>(
        &'a self,
        _store: &'a crate::server::core::account_store::AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = crate::server::core::providers::adapter::ModelRefreshOutcome,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move { crate::server::core::providers::adapter::ModelRefreshOutcome::unchanged() })
    }

    fn is_stateful(&self) -> bool {
        true
    }

    fn forward_conversation<'a>(
        &'a self,
        _store: &'a crate::server::core::account_store::AccountStore,
        _account_id: &'a str,
        _body: &'a Value,
        _client_headers: &'a axum::http::HeaderMap,
        _proxy: Option<crate::server::core::proxies::ResolvedProxy>,
        _stream: bool,
        _telemetry: &'a Arc<RequestTelemetry>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ForwardOutcome, GatewayError>> + Send + 'a>,
    > {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(ForwardOutcome::Completion {
                body: json!({ "ok": true }),
            })
        })
    }
}

#[tokio::test]
async fn stateful_provider_refuses_to_send_when_masking_is_enabled() {
    let (_dir, service, id) = temp_service();
    let masker = Arc::new(crate::server::core::masking::RulesMasker::default());
    let _guard = crate::server::core::masking::config::testing::enabled_with(masker);

    let calls = Arc::new(AtomicUsize::new(0));
    let adapter = StatefulMaskProbe {
        calls: Arc::clone(&calls),
    };

    let body = json!({"model": "m", "messages": [{"role": "user", "content": "key sk-abc123"}]});
    let headers = axum::http::HeaderMap::new();
    let telemetry = Arc::new(RequestTelemetry::new());
    let sanitize = crate::server::config::SanitizeSettings::default();
    let providers = std::collections::BTreeMap::new();
    let prompt = crate::server::core::prompt::PromptPlan::new(
        crate::server::core::prompt::PromptChoice {
            mode: crate::server::core::prompt::PromptMode::Passthrough,
            text: "",
            source: crate::server::core::prompt::PromptSource::None,
        },
        &providers,
    );
    let ctx = ProviderContext {
        body: &body,
        routing: crate::server::core::upstream::route_context::RequestRouting::capture("m", &service.store),
        compact_routed: false,
        stream: false,
        images: None,
        client_headers: &headers,
        telemetry: &telemetry,
        sanitize: &sanitize,
        developer_role_compat: true,
        image_compress: crate::server::config::ImageCompressSettings::default(),
        prompt,
        preserve_system_prompt: false,
        key_scope: None,
        vision: None,
        direct_provider: None,
        direct_model: None,
        pinned_account: None,
        ignore_model_gate: false,
        proxy_override: None,
    };
    let target = RouteTarget {
        provider: "workbuddy".to_string(),
        account_id: Some(id),
        account: None,
        egress: crate::server::core::proxies::AccountEgress::Direct,
        priority: None,
        proxy_notice: None,
        decision: None,
    };
    let mut slot: Option<InFlightGuard> = None;
    let mut connections = ConnectionGuard::new(service.connections());

    let error = match attempt_stateful(
        &service,
        &ctx,
        "workbuddy",
        &adapter,
        target,
        &mut slot,
        &mut connections,
        false,
    )
    .await
    {
        Ok(_) => panic!("脱敏开启时有状态 provider 必须拒绝发送"),
        Err(error) => error,
    };

    assert_eq!(error.status_code, 503);
    assert_eq!(error.code.as_deref(), Some("masking_unsupported"));
    assert!(
        error.message.contains("出站脱敏已开启"),
        "{}",
        error.message
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "门禁必须在调用适配器之前返回：上游一次都不该被打到"
    );
}

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

    let (_dir, service, id) = temp_service();
    // 生产库里那条真实文案（HTTP 200 的 SSE 里折出 403，首包门送进会话式分支）
    let error = GatewayError::with_status(403, "上游报告 InferHub.4291.200：insufficient quota");
    let class = adapter_for(ProviderKind::CodeArts).classify_conversation_error(&error);
    let UpstreamErrorClass::QuotaLimited { reset_at: Some(reset_at), .. } = &class else {
        panic!("福利池耗尽的文案应分类为带恢复时刻的 QuotaLimited，实际 {class:?}");
    };
    mark_conversation_limited(
        &service,
        &id,
        &single("glm-5.3-flash"),
        &class,
        &error.message,
        "codearts",
    );

    let stored = rotate::account_limit_reset_at(&service, &id, "glm-5.3-flash");
    assert_eq!(stored, *reset_at, "落库的恢复时刻必须是适配器算出的日池重置点");
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
// 编排层的事：余额 / 验证类停**整个账号**，额度 / 套餐 / 分钟限流只停**这个
// 模型**。两类写进 `RouteState` 的两张不同表（账号级 `backoff` / 模型级
// `model_rest`），选路侧据此分别垫底与跳过 —— 写错表的症状是「一个模型的
// 分钟限流把整账号其他模型也关了」，或者反过来「余额耗尽只停一个模型」。

/// 账号级退避的截止时刻（没有记录时为 0）。
fn account_backoff_until(service: &UpstreamService, account_id: &str) -> i64 {
    service
        .route
        .snapshot("", logging::now_ms(), 0)
        .resting
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

/// 余额不足 ⇒ **账号级**长休息（默认 24 小时），且**不**写模型级 rest。
///
/// 反了会怎样：只停一个模型时，同一账号换一个模型继续打，仍然每次都被
/// 「余额不足」拒 —— 一轮对话里白撞 N 次；反过来若这里写了模型 rest，选路
/// 那边两张表都命中，效果虽然更狠，但语义混乱（账号页会显示一个假的模型冷却）。
#[test]
fn quota_rest_credit_stops_the_account_not_the_model() {
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
    let until = account_backoff_until(&service, &id);
    let hours = (until - now) / 3_600_000;
    assert!(
        (23..=25).contains(&hours),
        "余额类应当停整个账号约 24 小时，实际 {hours} 小时"
    );
    assert_eq!(
        model_rest_until(&service, &id, "m-credit"),
        0,
        "余额类不该再写一条模型级 rest"
    );
}

/// 分钟级限流 ⇒ **模型级**短 rest（`retryAfterMs` 或默认 60 秒），账号不休息。
///
/// 反了会怎样：一次「20 requests per minute」把整个账号停 24 小时，同一账号
/// 其他模型全部不可用 —— 这是最贵的一种误判。
#[test]
fn quota_rest_rate_stops_only_the_model_for_a_minute() {
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
        account_backoff_until(&service, &id),
        0,
        "分钟级限流不该让整个账号休息"
    );
}

/// 额度类 + 上游给了重置时刻 ⇒ 模型级 rest **停到那一刻**（不是 10 分钟兜底）。
///
/// 反了会怎样：日池 / 套餐额度的恢复时刻被兜底常量覆盖，账号按 10 分钟的节拍
/// 反复去撞一个当天不会再恢复的模型（生产实测两小时白撞 8 次）。
#[test]
fn quota_rest_quota_with_reset_stops_the_model_until_then() {
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
        account_backoff_until(&service, &id),
        0,
        "额度类只停模型，不停整个账号"
    );
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
    // 适配器只提供「怎么判上游错误」这一半；413 的处置在**编排层**
    // （`transport::RetryBudget` + `fallback_retry_advice`），因此随便挑一个
    // 内置家即可。原先这里用 ZCode 的实例，它随实现搬到插件了 —— 换成本家
    // 走得通的 WorkBuddy，断言的仍是同一处语义。
    let result = send_with_retry(
        adapter_for(ProviderKind::WorkBuddy),
        &transport,
        &mut budget,
        Some(&capture),
        &std::sync::Arc::new(RequestTelemetry::new()),
        false,
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

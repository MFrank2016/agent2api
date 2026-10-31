//! 计费 / 签到 / 运营活动的**编排层**。
//!
//! ── 这里为什么变薄了（2026-10 插件化）───────────────────────
//! 端点路径（`/v2/billing/meter/daily-checkin`）、计费专用的头集合、重复领取的
//! 上游文案（「今天已签到，请明天再来」）、跨站落点（`www.codebuddy.cn`）与
//! 商品码 —— 全是「这家上游长什么样」的知识，已整体搬到提供商插件
//! （`workbuddy::billing`）。本模块只剩三件事：
//!
//!   1. **取会话**：当前账号（或调用方指定的账号）→ 一条可用于路由的会话；
//!   2. **转发**：把动作名与账号 id 交给插件的 `admin.action` / `usage.query`；
//!   3. **组合**：`checkin_and_report` 这种「状态 + 领取 + 余额」的编排，
//!      以及国际版每日活跃的读数拼装。
//!
//! ── 账号 id 从哪来 ──────────────────────────────────────────
//! `admin.action` 必须带账号 id 才能路由到插件里的那条记录，而本模块的若干
//! 入口收的是**会话**（`store.get_session_by_id` 的产物，刻意不含 id）。
//! 所以按 `(provider, uid)` 反查一次 —— 地区从会话的 `edition` 派生
//! （账号存储写会话时就是从记录抄的）。
//!
//! ── 文件分工 ────────────────────────────────────────────────
//!   mod.rs        服务句柄、错误类型、转发与组合（本文件）
//!   checkin.rs    账号级签到（目标集合 + 串行执行，向定时签到暴露同一条路径）
//!   keepalive.rs  国际版日活保活的**模型链配置**（用户配置，留在网关）
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块零 unwrap/expect/panic。

pub mod checkin;
pub mod keepalive;
mod request;
mod travel;
pub use travel::TravelOpts;

use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::AuthService;
use crate::server::core::auth_http::{send_raw, ApiResponse};
use crate::server::core::endpoints::{normalize_endpoint, RESPONSE_CODE_OK};
use crate::server::core::providers::adapter::adapter_by_id;
use crate::server::core::providers::workbuddy::Region;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::logging;

use request::{build_headers, whitelist_headers, BillingCall, BillingSpec, CallOptions};

/// 计费接口单请求总超时（对照 Node 版 REQUEST_TIMEOUT_MS）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 计费/活动接口的错误（对应 Node 版 WorkBuddyBillingError）。
///
/// `status_code` 会原样变成 HTTP 状态码（504 超时 / 401 登录态过期 /
/// 上游 HTTP 状态码），所以它必须是 Option 之外的一个确定值 ——
/// Node 版构造函数默认 502。
#[derive(Clone, Debug)]
pub struct BillingError {
    pub message: String,
    pub status_code: i32,
    pub upstream_code: Option<i64>,
}

impl BillingError {
    pub fn new(message: impl Into<String>, status_code: i32) -> Self {
        Self { message: message.into(), status_code, upstream_code: None }
    }

    pub fn with_code(
        message: impl Into<String>,
        status_code: i32,
        upstream_code: Option<i64>,
    ) -> Self {
        Self { message: message.into(), status_code, upstream_code }
    }

    /// 转成统一的网关错误。
    ///
    /// 计费路由的失败 body 走 OpenAI 风格（`/api/usage` 那几条在 server.mjs
    /// 的最外层大 try 里 → errorPayload）—— 由路由层决定用哪个信封，
    /// 这里只提供转换。
    pub fn to_gateway_error(&self) -> crate::server::errors::GatewayError {
        let mut error = crate::server::errors::GatewayError::with_status(
            self.status_code,
            self.message.clone(),
        );
        if let Some(code) = self.upstream_code {
            error = error.upstream_code(code);
        }
        error
    }

    /// 插件侧错误 → 本模块的错误（状态码原样带过来）。
    fn from_gateway(error: crate::server::errors::GatewayError) -> Self {
        Self::with_code(error.message, error.status_code, error.upstream_code)
    }
}

impl std::fmt::Display for BillingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 国际版每日活跃任务的**执行粒度**（对应签到中心的三颗手动按钮；
/// 定时与批量签到恒走 [`WorkbuddyActivity::Full`]，粒度细分只是手动入口的事）。
///
/// 三种粒度的边界就是「哪几步要打上游」：
///   * `Full`：探测 + 条件领取 + 保活 —— 一次点击完整跑一遍；
///   * `Claim`：探测 + 条件领取 —— 只想要奖励，不多打保活那发对话；
///   * `Keepalive`：只保活 —— 活动没开时只维持账号活跃，不打探测与领取。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkbuddyActivity {
    Full,
    Claim,
    Keepalive,
}

impl WorkbuddyActivity {
    /// API 层传来的 mode 字符串 → 粒度。未知值回落 Full —— 手动入口的
    /// 缺省行为是「完整跑一遍」，比「悄悄少做一步」更接近点击者的本意。
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("claim") => Self::Claim,
            Some("keepalive") => Self::Keepalive,
            _ => Self::Full,
        }
    }

    /// 结果行里回带的 mode（前端排查「这轮实际跑了哪几步」用）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Claim => "claim",
            Self::Keepalive => "keepalive",
        }
    }
}

/// 计费客户端句柄：持有鉴权服务（取会话用）。
///
/// Node 版是 `createWorkBuddyBilling({ auth, ... })`，靠 `auth.getCurrentSession()`
/// 拿默认会话。按账号查询（/api/accounts/usage、/api/accounts/checkin）需要
/// **指定账号**的会话，那条路径由 `billing::checkin` 的入参显式传入 store，
/// 因此这里不再持有账号存储句柄。
#[derive(Clone)]
pub struct BillingService {
    auth: AuthService,
}

impl BillingService {
    pub fn new(auth: AuthService) -> Self {
        Self { auth }
    }

    /// 当前配置里的 locale（/api/usage 与 claim-and-report 用）
    fn current_locale() -> String {
        config::current().locale().to_string()
    }

    /// 取默认会话（当前账号）；没有可用登录态时报 401。
    ///
    /// 文案照抄 Node 版，但去掉了「node server.mjs --login」那半句 ——
    /// 壳内 Rust 版没有那个命令行入口，指向不存在的命令会误导用户。
    async fn require_session(&self) -> Result<Value, BillingError> {
        let session = self.auth.get_current_session().await.map_err(|error| {
            let status = error.http_status();
            BillingError::with_code(error.message, status, error.upstream_code)
        })?;
        match session {
            Some(session) if has_access_token(&session) => Ok(session),
            _ => Err(BillingError::new("当前没有可用登录态：请先在桌面端完成登录", 401)),
        }
    }

    /// 配置里的 locale → `Accept-Language` 的粗粒度值（zh / en）。
    fn accept_language(locale: Option<&str>) -> String {
        let Some(locale) = locale else {
            return "zh".to_string();
        };
        let normalized = locale.to_lowercase();
        if normalized.starts_with("zh") {
            return "zh".to_string();
        }
        if normalized.starts_with("en") {
            return "en".to_string();
        }
        "zh".to_string()
    }

    /// 会话里的出口（`session.proxy` → ResolvedProxy）。
    ///
    /// 会话里的 proxy 由账号存储组装，形态与 ResolvedProxy 的 JSON 完全一致；
    /// 数据坏了（缺主机/端口非法）时按直连处理并记一条 warn ——
    /// 出网代理是可选配置，它的故障不该让整条链路失败。
    fn proxy_of(session: &Value) -> Option<ResolvedProxy> {
        match ResolvedProxy::from_json(session.get("proxy").unwrap_or(&Value::Null)) {
            Ok(proxy) => proxy,
            Err(reason) => {
                logging::log(
                    "[Upstream]",
                    &format!("⚠️ 账号代理不可用（{reason}），本次回退直连"),
                );
                None
            }
        }
    }

    async fn call_billing(
        &self,
        spec: BillingSpec,
        options: CallOptions<'_>,
    ) -> Result<BillingCall, BillingError> {
        let session = match options.session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        // 账户级端点：国内版/国际版账号各走自己的站点。
        // 会话缺 endpoint 时回落到 auth 的默认 baseUrl（Node 的
        // `normalizeEndpoint(activeSession.endpoint || baseUrl)`）
        let endpoint = match options.base_url {
            Some(base) => normalize_endpoint(base),
            None => session
                .get("endpoint")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(normalize_endpoint)
                .unwrap_or_default(),
        };
        // 跨站端点（新手礼包 / 活动补偿）：落点由 spec 固定，不跟账号端点走（见 `base` 字段）
        let base = spec.base.map(str::to_string).unwrap_or(endpoint);
        let url = format!("{base}{}{}", spec.path, options.query.unwrap_or(""));

        let mut extra: Vec<(String, String)> = Vec::new();
        if let Some(locale) = options.locale {
            extra.push(("Accept-Language".to_string(), Self::accept_language(Some(locale))));
        }
        if spec.whitelist_headers {
            extra.extend(whitelist_headers(&session));
        }
        // 头顺序与 Node 的 `{...base, ...extra, X-Enterprise-Id, ...}` 一致：
        // 条件头在 extra 之后（同名时后者胜，Node 的对象展开也是这个顺序）
        let headers = build_headers(&session, &extra, options.suppress_identity_headers);

        let body = if spec.method == "GET" || spec.method == "HEAD" {
            None
        } else {
            // Node 是 `JSON.stringify(body ?? spec.body ?? {})` ——
            // 缺省体一律是空对象，不能省掉（上游对缺 body 的 POST 会报 400）
            Some(options.body.cloned().unwrap_or_else(|| spec.body()))
        };

        let proxy = Self::proxy_of(&session);
        logging::verbose(
            "[Billing]",
            &format!(
                "{} {url}{}",
                spec.method,
                match &proxy {
                    Some(proxy) if !proxy.label.is_empty() => format!(" 经代理 {}", proxy.label),
                    Some(proxy) => format!(" 经代理 {}", proxy.host),
                    None => String::new(),
                }
            ),
        );

        let response: ApiResponse = send_raw(
            spec.method,
            &url,
            body.as_ref(),
            &headers,
            proxy.as_ref(),
            Some(REQUEST_TIMEOUT_MS),
        )
        .await
        .map_err(|error| {
            if error.is_timeout() {
                BillingError::new("计费接口请求超时", 504)
            } else {
                // 文案对齐 Node：`计费接口请求失败: ${error.message}`
                BillingError::new(format!("计费接口请求失败: {error}"), 502)
            }
        })?;

        let payload = response.payload;
        let code = payload
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_i64);
        // Node: `payload?.msg || payload?.message` —— 空串也当「没有」，
        // 所以这里要过滤空串，不能拿到 Some("") 就当命中了
        let msg = payload
            .as_ref()
            .and_then(|value| {
                value
                    .get("msg")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .or_else(|| {
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                    })
            })
            .map(str::to_string);
        // Node 的 `requestId: payload?.requestId` 在缺失时是 undefined，
        // JSON 化会**丢掉这个键**；用 Option 表达同一语义（None = 不出键）
        let request_id = payload
            .as_ref()
            .and_then(|value| value.get("requestId"))
            .cloned();

        if response.status == 401 || response.status == 403 {
            return Err(BillingError::with_code(
                "登录态已过期或被拒绝，无法调用计费接口",
                401,
                code,
            ));
        }
        if !response.ok {
            // Node: `计费接口返回 HTTP ${status}${payload?.msg || payload?.message ? `: ...` : ''}`
            let detail = payload
                .as_ref()
                .and_then(|value| {
                    value
                        .get("msg")
                        .and_then(Value::as_str)
                        .or_else(|| value.get("message").and_then(Value::as_str))
                })
                .unwrap_or("");
            // 「今天已签到」不是错误：WorkBuddy 国内版在当日已领过时，`daily-checkin`
            // 返回的是 **HTTP 400 + 文案「今天已签到，请明天再来」**，而不是
            // HTTP 200 + 非 0 code（后者才是 `checkin.rs` 原先设想的形态，见
            // `checkin_completed_today` 的注释）。不放行的话，这条正常终态会被
            // 整条丢成 `Err`，于是 `claim` 为 null，导致：
            //   1. `checkin_completed_today` 永远判不出「今天已签」，`mark_checkin`
            //      不被调用 → 面板的「已签到」标识永远不亮、`checkinAt` 恒为 null；
            //   2. 定时签到的 `lastResult` 恒定「成功领取 0/N」，用户看不出其实签成了，
            //      会反复重试或去查凭证。
            //
            // 判据刻意收得很紧，只放行**明确说「今天已领过」**的那一种：
            //   · 只认 400 / 409 —— 400 是实测的，409 是防御性放行（详见
            //     `is_duplicate_claim` 的说明）；
            //   · 只认 `msg` 文案含「已签到 / 已领取」—— 与 `checkin_completed_today`
            //     的第 3 条判据同源，两处必须一致，否则放行了却仍判不出「已完成」；
            //   · 只在签到领取那条调用点上开（`tolerate_duplicate_claim`）。
            // 不满足任何一条就照旧报错：`checkin.rs` 特意强调过**不能宽到「只要
            // 没报错就算已签到」**，否则网络错误、凭证失效会被误判成已签到而白丢
            // 一天（实测见过批次撞风控全部未领取的情形）。
            if options.tolerate_duplicate_claim {
                if is_duplicate_claim(response.status, detail) {
                    logging::verbose(
                        "[Billing]",
                        &format!("签到重复领取（HTTP {}），按「今日已完成」处理: {detail}", response.status),
                    );
                    return Ok(BillingCall { code, msg, request_id, data: Value::Null, raw: payload });
                }
                // 开了容错却**没**命中：最可能是上游改了文案（判据是文案匹配，
                // 见 `is_duplicate_claim`）。留一条现场 —— 否则「面板又不像 #137
                // 那样亮了」会查无实据：没有这条日志，文案漂移与真故障在日志里
                // 长得一模一样，排查要从头再来一遍。verbose 级，正常运行时不出声。
                logging::verbose(
                    "[Billing]",
                    &format!(
                        "签到领取返回 HTTP {}，未命中「重复领取」文案，按错误处理: {detail}",
                        response.status
                    ),
                );
            }
            let message = if detail.is_empty() {
                format!("计费接口返回 HTTP {}", response.status)
            } else {
                format!("计费接口返回 HTTP {}: {detail}", response.status)
            };
            return Err(BillingError::with_code(message, response.status as i32, code));
        }
        if let Some(code_value) = code {
            if code_value != RESPONSE_CODE_OK && options.expect_code_ok {
                // Node: `${payload.msg || payload.message || `计费接口返回 code=${code}`}`
                let message = msg
                    .clone()
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| format!("计费接口返回 code={code_value}"));
                return Err(BillingError::with_code(
                    message,
                    response.status as i32,
                    Some(code_value),
                ));
            }
        }

        let data = payload
            .as_ref()
            .and_then(|value| value.get("data"))
            .cloned()
            .unwrap_or(Value::Null);
        Ok(BillingCall { code, msg, request_id, data, raw: payload })
    }

    /// 会话 → `(provider_id, account_id)`。
    ///
    /// 地区从会话的 `edition` 派生（账号存储写会话时就是从记录抄的），
    /// 再用 `(provider, uid)` 反查记录 id —— `admin.action` 要的是 id。
    ///
    /// ── uid 取不到时的退路（issue #380）────────────────────────────
    /// 会话是 `session_from_record` 写的，那条路径现在会回落
    /// `pluginPublic` / `pluginSecret`，所以正常情况下这里能拿到 uid。
    /// 但如果会话来自更早的调用方（已构造好的 JSON），uid 仍可能为空 ——
    /// 那时按 `user-<uid>` 的记录 id 形态再试一次：WorkBuddy 系的插件落库
    /// 记录 id 就是那个形态（`accounts::identity_of`）。
    fn account_of(&self, session: &Value) -> Option<(String, String)> {
        let uid = session
            .get("account")
            .and_then(|account| account.get("uid"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let region = Region::from_edition_id(session.get("edition").and_then(Value::as_str));
        let provider = region.provider_id();
        let store = self.auth.store();
        store
            .account_id_of_identity(provider, uid)
            .or_else(|| store.account_id_of_user_id_suffix(provider, uid))
            .map(|id| (provider.to_string(), id))
    }

    /// 转发一次管理动作到插件（`admin.action`）。
    async fn admin(
        &self,
        session: &Value,
        action: &str,
        payload: &Value,
    ) -> Result<Value, BillingError> {
        let Some((provider, account_id)) = self.account_of(session) else {
            return Err(BillingError::new(
                "找不到该会话对应的账号记录，无法执行该操作",
                400,
            ));
        };
        let Some(adapter) = adapter_by_id(&provider) else {
            return Err(BillingError::new(
                format!(
                    "{} 的计费接口需要安装提供商插件：本版本不再内置反向代理实现",
                    crate::server::core::providers::label_of(&provider)
                ),
                503,
            ));
        };
        let store: AccountStore = self.auth.store();
        adapter
            .admin_action(&store, &account_id, action, payload)
            .await
            .map_err(BillingError::from_gateway)
    }

    // ─── 签到 ───────────────────────────────────────────────

    /// 签到活动状态（`AuthService.getCheckinStatus`）。
    ///
    /// 成功但 data 为空时返回 null（与桌面端一致）。国际版走每日活跃探测，
    /// 响应归一成同一套键（`active` / `todayCheckedIn`），调用方不用分版本
    /// 读字段 —— 归一在插件里（`billing::ops::checkin_status`）。
    pub async fn get_checkin_status(&self, session: Option<&Value>) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        self.admin(&active, "billing.checkin-status", &Value::Null).await
    }

    /// 领取每日签到积分（`AuthService.claimDailyCheckin`）。
    ///
    /// 幂等：已领取时上游返回非 0 code（或 HTTP 400），插件那边原样返回
    /// `{success:false, code, msg}` ——「今天已签到」不是错误，前端面板会把它
    /// 显示成一条 warn 提示。
    ///
    /// 国际版没有「领取签到」这个动作：它走每日活跃任务链
    /// （[`Self::workbuddy_daily_activity`]），并把领取结果、活动状态与保活
    /// 读数一并装进 claim 返回 —— 组合入口与签到链两条路都从这进，
    /// 一次调用不重复打上游。
    pub async fn claim_daily_checkin(&self, session: Option<&Value>) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        if is_international(&active) {
            let activity = self
                .workbuddy_daily_activity(&active, WorkbuddyActivity::Full)
                .await;
            let mut claim = activity.get("claim").cloned().unwrap_or_else(|| {
                json!({
                    "success": false,
                    "code": -1,
                    "msg": "活跃任务未返回领取结果",
                })
            });
            if let Some(object) = claim.as_object_mut() {
                object.insert(
                    "activity".to_string(),
                    activity.get("activity").cloned().unwrap_or(Value::Null),
                );
                object.insert(
                    "status".to_string(),
                    activity.get("status").cloned().unwrap_or(Value::Null),
                );
            }
            return Ok(claim);
        }
        self.admin(&active, "billing.checkin-claim", &Value::Null).await
    }

    /// 领取一次性运营奖励（`claim-gift` / `claim-compensation`）。
    ///
    /// 国内版专属（插件侧对国际版明确报错）。每号一次 ——「已领取」与
    /// 「没有可领取的」都是正常终态，插件原样返回而不是丢成 `Err`。
    async fn claim_reward(&self, kind: &str, session: &Value) -> Result<Value, BillingError> {
        self.admin(session, "billing.reward", &json!({ "kind": kind }))
            .await
    }

    /// 领取活动补偿（如 2026-10 的 DeepSeek 故障补偿 500 积分）
    pub async fn claim_compensation(&self, session: &Value) -> Result<Value, BillingError> {
        self.claim_reward("compensation", session).await
    }

    /// 领取新手礼包（一次性新用户积分包，没领过的账号才有）
    pub async fn claim_gift(&self, session: &Value) -> Result<Value, BillingError> {
        self.claim_reward("gift", session).await
    }

    // ─── 余额 ───────────────────────────────────────────────

    /// 积分简报（`{kind, unlimited, totalLeft, planLeft, bonusLeft}` + 明细 + 套餐）。
    ///
    /// 翻译逻辑（上游嵌套结构 → 前端契约）在插件里；这里只负责
    /// 「会话 → 账号 id → `usage.query`」。
    pub async fn query_credits_summary(
        &self,
        session: Option<&Value>,
        _locale: Option<&str>,
    ) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        let Some((provider, account_id)) = self.account_of(&active) else {
            return Err(BillingError::new(
                "找不到该会话对应的账号记录，无法查询积分",
                400,
            ));
        };
        let Some(adapter) = adapter_by_id(&provider) else {
            return Err(BillingError::new(
                format!(
                    "{} 的积分查询需要安装提供商插件：本版本不再内置反向代理实现",
                    crate::server::core::providers::label_of(&provider)
                ),
                503,
            ));
        };
        let store = self.auth.store();
        adapter
            .query_usage(&store, &account_id)
            .await
            .map_err(BillingError::from_gateway)
    }

    /// 默认语言下的积分简报（路由层入口，对照 server.mjs
    /// `billing.queryCreditsSummary({ locale: opts.locale })`）
    pub async fn query_credits_summary_default(&self) -> Result<Value, BillingError> {
        let locale = Self::current_locale();
        self.query_credits_summary(None, Some(&locale)).await
    }

    // ─── 运营活动 ───────────────────────────────────────────

    /// 运营 banner：需客户端白名单头，否则被拦截（返回 null，与桌面端一致）。
    ///
    /// **吞掉所有错误并返回 null** —— banner 是页面上的一块装饰，拉不到就不
    /// 显示，不该让首屏出现一条红色报错（与 Node 版一致）。
    pub async fn get_activity_banner(&self, session: Option<&Value>) -> Value {
        let active = match session {
            Some(session) => session.clone(),
            None => match self.require_session().await {
                Ok(session) => session,
                Err(error) => {
                    logging::log("[Activity]", &format!("banner 拉取失败: {}", error.message));
                    return Value::Null;
                }
            },
        };
        match self.admin(&active, "billing.banner", &Value::Null).await {
            Ok(value) => value,
            Err(error) => {
                logging::log("[Activity]", &format!("banner 拉取失败: {}", error.message));
                Value::Null
            }
        }
    }

    /// 大使（推广）状态。同样吞错返回 null（verbose 级别日志）。
    pub async fn get_ambassador_status(&self, session: Option<&Value>) -> Value {
        let active = match session {
            Some(session) => session.clone(),
            None => match self.require_session().await {
                Ok(session) => session,
                Err(error) => {
                    logging::verbose(
                        "[Activity]",
                        &format!("ambassador 状态拉取失败: {}", error.message),
                    );
                    return Value::Null;
                }
            },
        };
        match self.admin(&active, "billing.ambassador", &Value::Null).await {
            Ok(value) => value,
            Err(error) => {
                logging::verbose(
                    "[Activity]",
                    &format!("ambassador 状态拉取失败: {}", error.message),
                );
                Value::Null
            }
        }
    }

    /// 签到 + 查余额的组合动作（"签到并回报最新积分"）。
    ///
    /// 已签到时不重复领取，直接返回当前额度。
    ///
    /// 并发语义：Node 用 `Promise.all` 并发跑「查状态 + 领取」。
    /// 这里必须**顺序**执行 —— 两者共享同一个底层连接池没问题，
    /// 但并发会在上游留下两条几乎同时到达的签到请求，而 Node 版
    /// `checkinAndReport` 的注释明确说「额度可能因签到变化，稍等一下再查」，
    /// 顺序执行才保证 `usage` 反映的是领取之后的额度。
    pub async fn checkin_and_report(
        &self,
        session: Option<&Value>,
        locale: Option<&str>,
    ) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };

        // 国际版没有国内版的普通签到接口；组合入口沿用同一条活跃任务链路，
        // 并把探测/领取/保活结果一起返回，避免重复打上游。
        if is_international(&active) {
            let activity = self
                .workbuddy_daily_activity(&active, WorkbuddyActivity::Full)
                .await;
            let usage = match self.query_credits_summary(Some(&active), locale).await {
                Ok(value) => value,
                Err(error) => json!({ "error": error.message }),
            };
            return Ok(json!({
                "checkinStatus": activity.get("status").cloned().unwrap_or(Value::Null),
                "claim": activity.get("claim").cloned().unwrap_or(Value::Null),
                "activity": activity.get("activity").cloned().unwrap_or(Value::Null),
                "usage": usage,
            }));
        }

        // 查状态与领取：Node 并发，这里顺序（理由见上）。
        // 领取失败不抛出 —— 收敛成 `{success:false, code:-1, msg}`，与 Node 的
        // `.catch(error => ({ success:false, code:-1, msg: error.message }))` 一致
        let status = self.get_checkin_status(Some(&active)).await?;
        let claim = match self.claim_daily_checkin(Some(&active)).await {
            Ok(value) => value,
            Err(error) => json!({ "success": false, "code": -1, "msg": error.message }),
        };
        // 额度可能因签到变化，稍等一下再查（对照 Node 的 sleep(500)）
        if claim.get("success").and_then(Value::as_bool).unwrap_or(false) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let usage = match self.query_credits_summary(Some(&active), locale).await {
            Ok(value) => value,
            Err(error) => json!({ "error": error.message }),
        };
        Ok(json!({
            "checkinStatus": status,
            "claim": claim,
            "usage": usage,
        }))
    }

    /// 默认语言下的签到组合动作（路由层入口）
    pub async fn checkin_and_report_default(&self) -> Result<Value, BillingError> {
        let locale = Self::current_locale();
        self.checkin_and_report(None, Some(&locale)).await
    }

    // ─── 国际版每日活跃 ─────────────────────────────────────

    /// 国际版每日活跃任务：活动探测 → 条件领取 → 免费模型保活 → 网页会话。
    ///
    /// ── 这里只做转发 ────────────────────────────────────────────
    /// 执行体在**提供商插件**里（`workbuddy::keepalive`，动作 `activity.run`）。
    /// 三段流程里全是「国际版上游长什么样」的知识：端点、`chat_request_send`
    /// 事件的字段、网页会话的 `conversationOrigin` 与浏览器 UA。网关这边只剩
    /// 「读配置、拿模型链、挑账号、透传结果」。
    ///
    /// ── 模型链 ──────────────────────────────────────────────────
    /// `keepalive::models()`（签到中心可自定义）作为 payload 传给插件：那是
    /// **用户的**配置，留在网关的 config.json 里，插件不持有影子副本。
    /// 用户没配时是空数组 —— 缺省链在插件里（「上游此刻有哪些免费模型」是
    /// 上游知识，不该在网关留一份会过时的名单）。
    pub async fn workbuddy_daily_activity(&self, session: &Value, mode: WorkbuddyActivity) -> Value {
        let mut blocked = None;
        let mode = if mode != WorkbuddyActivity::Claim {
            match keepalive::authorized_models() {
                Ok(_) => mode,
                Err(error) if mode == WorkbuddyActivity::Keepalive => return activity_failure(&error.message),
                Err(error) => {
                    blocked = Some(error.message);
                    WorkbuddyActivity::Claim
                }
            }
        } else { mode };
        let payload = json!({
            "mode": mode.as_str(),
            "models": keepalive::models(),
        });
        match self.admin(session, "activity.run", &payload).await {
            Ok(mut result) => {
                if let Some(reason) = blocked {
                    if let Some(activity) = result.get_mut("activity").and_then(Value::as_object_mut) {
                        activity.insert("error".to_string(), json!(reason));
                        activity.insert("modelCallsSkipped".to_string(), Value::Bool(true));
                    }
                }
                result
            }
            Err(error) => activity_failure(&error.message),
        }
    }
}

/// 这次非 2xx 响应是否表示「今天已经领过了」（签到重复领取）。
fn is_duplicate_claim(status: u16, detail: &str) -> bool {
    matches!(status, 400 | 409) && (detail.contains("已签到") || detail.contains("已领取"))
}

/// 一次国际版每日活跃任务**没跑起来**时的结果（形状与插件的成功返回一致）。
///
/// ── 为什么不是 `Err` ────────────────────────────────────────
/// 调用方（签到链、`claim_daily_checkin`、手动三颗按钮）都按「`activity` 段
/// 有读数」设计：这里返回 Err 会把「上游没答上」变成整个签到失败。与
/// 「活动未开放属正常状态、失败收敛为结果字段」是同一口径 —— 差别只在
/// 这次连请求都没发出去（没装插件 / 会话对不上账号），原因写进 `claim.msg`
/// 与 `activity.report.msg`，用户看得见。
fn activity_failure(reason: &str) -> Value {
    logging::verbose("[Checkin]", &format!("WorkBuddy 国际版每日活跃未执行: {reason}"));
    json!({
        "status": Value::Null,
        "claim": {
            "success": false,
            "code": -1,
            "msg": reason,
        },
        "activity": {
            "mode": Value::Null,
            "active": Value::Null,
            "todayCheckedIn": Value::Null,
            "statusAvailable": Value::Null,
            "report": {
                "succeeded": false,
                "streakDays": Value::Null,
                "msg": reason,
            },
            "pokeSucceeded": Value::Null,
            "pokeModel": Value::Null,
            "webConversation": Value::Null,
        },
    })
}

/// 会话是否带 accessToken（Node: `session?.auth?.accessToken` 真值判定）
fn has_access_token(session: &Value) -> bool {
    session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
}

/// 国际版会话判定（拆家后账号落盘仍带 `edition`，session 从账号派生时继承它）。
/// 国内版与国际版的签到语义不同（前者有签到接口、后者只有活跃任务），
/// 这条判定是两条链分流的唯一闸口。
fn is_international(session: &Value) -> bool {
    Region::from_edition_id(session.get("edition").and_then(Value::as_str)) == Region::Intl
}

//! LobsterAI（有道龙虾 / NetEase Youdao）接入：**无状态** SSE 转发 + 令牌续期 +
//! 远程模型目录 + 每日签到 + 积分余额 + 浏览器 OAuth 登录。
//!
//! ── 这条通道长什么样（逆向契约）────────────────────────────────
//! ```text
//!   基址   {base} = https://lobsterai-server.youdao.com（LOBSTERAI_API_BASE 可覆盖）
//!   身份   POST /api/auth/exchange（授权码 → accessToken / refreshToken）
//!          POST /api/auth/refresh（keyfrom + refreshToken → 新令牌）
//!   转发   POST /api/proxy/v1/chat/completions（OpenAI 兼容，SSE-only）
//!   目录   GET  /api/models/available（Bearer + 能力头）
//!   签到   GET  /api/client-activities/slot → context → POST .../actions/check_in
//!   余额   GET  /api/user/profile-summary
//! ```
//! 上游是 OpenAI 兼容的 chat 端点（无状态），因此 `is_stateful` 走默认 false。
//!
//! ── 子模块分工 ──────────────────────────────────────────────
//! ```text
//!   endpoints.rs    地址 / 路径 / 协议版本 / 真实客户端版本（缓存）/ 逐字请求头
//!   credentials.rs  账号记录 → 凭证快照（accessToken / refreshToken / keyfrom / 设备 id）
//!   refresh.rs      令牌续期（单飞 + 比较再写）
//!   models.rs       GET /api/models/available（5 分钟缓存 + 落盘缓存）
//!   balance.rs      GET /api/user/profile-summary（归一成统一积分形状）
//!   checkin.rs      每日签到三步时序（slot → context → check_in → 回读确认）
//!   login.rs        浏览器 OAuth（授权码交换）+ 粘贴令牌归一化
//!   adapter.rs      ProviderAdapter 实现（装配层）
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic。

pub mod adapter;
pub mod balance;
pub mod checkin;
pub mod credentials;
pub mod endpoints;
pub mod login;
pub mod models;
pub mod refresh;

pub use adapter::{LobsterAiAdapter, LOBSTERAI_ADAPTER};

/// 本家的 provider id（与 `providers::kind_id(ProviderKind::LobsterAi)` 同源）
pub const PROVIDER_ID: &str = "lobsterai";

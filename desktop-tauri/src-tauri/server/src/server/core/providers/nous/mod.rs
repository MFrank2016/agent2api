//! Nous Research Portal（`portal.nousresearch.com`）接入：**无状态** OpenAI
//! 兼容转发 + Portal OAuth 设备码登录 + 匿名免费档。
//!
//! ── 这条通道长什么样（规格 A/B/C）────────────────────────────
//! 一条身份链（Portal OAuth 设备码 → access/refresh token；access_token 即推理 Bearer），
//! 两个推理面：
//! ```text
//!   账号：Bearer <OAuth access_token> 打 https://inference-api.nousresearch.com/v1
//!   匿名：Bearer <匿名 JWT>  打 https://welcome-api.nousresearch.com/v1（只认 nous/welcome）
//! ```
//! 与官方 `hermes-agent` 的行为一致：**没有账号时透明地走匿名免费档**。
//!
//! ── 为什么是无状态 ──────────────────────────────────────────
//! 上游推理面就是 OpenAI `chat/completions`（一次请求一次回答），没有会话闸门
//! 或多步协议。所以 `is_stateful` 走默认 false，转发链只需 `build_chat_request`；
//! 账号轮换、限额冷却、退避重试、usage 与取消全部由编排层承担。
//!
//! ── 子模块分工 ──────────────────────────────────────────────
//! ```text
//!   endpoints.rs    地址 / 路径 / 客户端常量 / UA 伪装 / 头集合
//!   credentials.rs  账号记录 / 会话 → 凭证快照（账号形态 vs 匿名形态）
//!   models.rs       GET {base}/models（10 分钟缓存 + 内置兜底清单）
//!   oauth.rs        设备码登录 + 令牌续期 + 单飞刷新
//!   anonymous.rs    匿名身份创建 / JWT 换取 + 进程级缓存
//!   adapter.rs      ProviderAdapter 实现（装配层）
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic。

pub mod adapter;
pub mod anonymous;
pub mod credentials;
pub mod endpoints;
pub mod models;
pub mod oauth;

pub use adapter::{NousAdapter, NOUS_ADAPTER};

use std::sync::Arc;

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;

/// 本家的 provider id（与 `providers::kind_id(ProviderKind::Nous)` 同源）
pub const PROVIDER_ID: &str = "nous";

/// 出网客户端：登录 / 刷新链路用。
///
/// 口径与 Antigravity / OfficeAce 一致：账号级代理存在就用它；否则走系统代理
/// 探测（`egress::client_for_system_proxy()`）—— 本家上游在海外，系统里配好的
/// 代理往往就是本机唯一能出网的那条；系统没配代理时它等价于直连，不引入新的
/// 失败面。转发链路的出口由编排层按账号解析后单独给（见 `TransportRequest`）。
pub(crate) fn client_for(proxy: Option<&ResolvedProxy>) -> Arc<reqwest::Client> {
    match proxy {
        Some(proxy) => egress::client_for(Some(proxy)),
        None => egress::client_for_system_proxy(),
    }
}

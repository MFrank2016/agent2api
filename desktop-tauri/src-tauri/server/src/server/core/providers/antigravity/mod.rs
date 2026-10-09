//! Antigravity（Google 的 AI IDE）适配实现：**本步只接账号 / token 刷新 / 模型目录**。
//!
//! ── 上游长什么样（逆向来源：规格 `_recon/antigravity-spec.md`，参考
//! `Acankao/Antigravity-Manager`（Rust/Tauri，主源）与 `Acankao/9router`（Node，
//! 交叉验证））──────────────────────────────────────────────────
//! Antigravity IDE 的推理走 **Google Cloud Code Assist**（`v1internal`），
//! **不是** `generativelanguage.googleapis.com`，也不是 OpenAI / Anthropic 协议：
//!
//! ```text
//!   站点     全球统一（没有地区参数、没有国内/国际双站点 —— 规格 §6）
//!   鉴权     Google OAuth 2.0（授权码 + loopback，本步只做粘贴 refresh_token）
//!             → Authorization: Bearer {access_token}
//!   推理     POST {base}/v1internal:streamGenerateContent?alt=sse     （下一步接）
//!   目录     POST {base}/v1internal:fetchAvailableModels
//!   project  POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist
//!            （无 project 时再 onboardUser；两个调用固定走 prod，见 project.rs）
//! ```
//! 三个环境基址（`sandbox` → `daily` → `prod`，**不是地区**）：聊天流量优先
//! sandbox/daily 以规避 prod 的 429；project 发现固定 prod。
//!
//! ── 为什么这么建模（三处与直觉不同的决定）──────────────────────
//!   1. **单家、无地区拆分**：本家没有 region 参数（规格 §6），因此不需要
//!      MonkeyCode / AutoClaw 那种 `region.rs`；一个 provider、一个目录缓存格；
//!   2. **`is_stateful` 恒 false**：上游是「一次 HTTP 请求 = 一次生成」，
//!      只是响应帧是 v1internal 信封（`data: {"response":{…}}`）+ Gemini 字段
//!      路径。下一步的路线是 **`UpstreamResponse` 加一个变体 + 翻译层**（与
//!      Command Code 的 NDJSON、ZCode 的 Anthropic 同一处置），不是
//!      `forward_conversation` —— 论证见 `adapter.rs` 的模块头；
//!   3. **凭证以 `refreshToken` 为准**：access_token 只活一小时，refresh_token
//!      才是长寿命主凭证（Google 只在首次授权下发，丢了只能重新授权）。
//!      token 刷新打的是 Google 的 `oauth2.googleapis.com/token`（form 表单），
//!      不是上游自己的接口 —— 与其它几家「刷新打自家端点」的形态不同。
//!
//! ── 本步的交付边界（严格）──────────────────────────────────────
//! ```text
//!   ✅ 账号：粘贴 refresh token 添加（归一化 + 一次真实校验刷新）
//!   ✅ token 刷新：临期主动刷 + 401 后强制刷（单飞 + 比较再写）
//!   ✅ 模型目录：POST :fetchAvailableModels（只列 Gemini）+ 内置兜底清单
//!   ✅ 注册接线：ProviderKind / 注册表 / adapter_for / 目录缓存 / 账号存储 / 添加分支
//!   ❌ 聊天转发：build_chat_request 返回 501「尚未接通」（下一步）
//!   ❌ 网页登录：本步不开窗口（supports_web_login 保持 false）
//! ```
//!
//! ── 网页登录接起来便不便宜（评估结论，**本步不实现**）──────────
//! 结论：**中等偏便宜，但不是「几行」**，且有一处未确认的前提。
//!   - 便宜的半边：Antigravity 没有自己的登录页 —— 它直接用 **Google OAuth
//!     授权码 + loopback 回调**（规格 §1.2/§1.3：临时端口 + 任意路径
//!     `/oauth-callback`，无 PKCE、无设备码）。本仓已有同形态的两套先例
//!     （raccoon 的自定义协议回调、Trae 的 `callback_server.rs` loopback 监听），
//!     骨架（起 listener → 校验 state → 换 token → 落账号）可以直接照搬。
//!   - 不便宜的部分：授权 URL 的 6 个 scope、`access_type=offline` +
//!     `prompt=consent` 的组合、以及「Google 是否接受这个 client 的动态
//!     loopback 端口」都还没实测过（规格 §8.5 把「官方确切 redirect_uri 注册值」
//!     列为**未确认**）。另需一个回调服务器 + 状态管理（约 200–300 行，
//!     与 Trae 的 `callback_server.rs` 同量级），并把登录窗口域名白名单
//!     （壳侧 `src/login.rs::allowed_hosts`）加上 `accounts.google.com`。
//!   - 判断：**值得做，但应该等聊天接通、这家的账号真的有用之后再排**；
//!     本步粘贴式已覆盖「从已登录的 IDE / 参考实现里导出 refresh_token」这条
//!     主路径。
//!
//! ── 子模块分工 ──────────────────────────────────────────────
//! ```text
//!   endpoints.rs    OAuth 常量 / v1internal 三个环境基址 / 方法名 / 请求头 / UA 约束
//!   credentials.rs  账号凭证（refreshToken 主 + accessToken 缓存）+ 临期判定
//!   oauth.rs        Google token 端点刷新（form）+ 单飞 + 比较再写 + invalid_grant 处置
//!   project.rs      cloudaicompanionProject 发现（loadCodeAssist → onboardUser）
//!   models.rs       :fetchAvailableModels（只列 Gemini）+ 内置兜底 + 落盘缓存
//!   login.rs        粘贴式归一化（1// 前缀 / 引号 / Bearer）+ 一次校验刷新
//!   adapter.rs      ProviderAdapter 实现（账号 / 刷新 / 目录已通；转发 501）
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic。
//! **不发 `x-goog-api-client`**（规格 §7.2：属于 IDE 的 JS 层，发了形成矛盾指纹）。

pub mod adapter;
pub mod credentials;
pub mod endpoints;
pub mod login;
pub mod models;
pub mod oauth;
pub mod project;

pub use adapter::{AntigravityAdapter, ANTIGRAVITY_ADAPTER};

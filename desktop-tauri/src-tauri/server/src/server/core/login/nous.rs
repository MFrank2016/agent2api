//! Nous Portal 设备码登录任务（RFC 8628）与通用 start / wait / cancel 状态表的衔接。
//!
//! ── 与另外几条登录链的关系 ──────────────────────────────────
//! 形态上介于「Qoder 设备授权」与「ZCode 服务端中介轮询」之间：
//!   · 与 Qoder / Cline 同款：**设备码**（同步 POST 一次拿 `user_code` 与授权页
//!     地址），用户在浏览器里确认；
//!   · 与 ZCode / OfficeAce 同款：确认是在浏览器里发生的，网关只能**轮询**
//!     令牌端点，因此 spawn 一个后台任务轮询，结果写回任务句柄供 `/wait` 取。
//!
//! ── 拿到令牌即可推理：access_token 就是 Bearer ──────────────
//! 设备码换到的 `access_token` 即推理 Bearer（scope `inference:invoke`；官方
//! hermes：`agent_key = access_token`）。上游已退役 `/api/oauth/agent-key`，
//! 因此不再铸 key —— 直接把 access_token 落成凭证（`agentKey` 字段）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::time::Duration;

use serde_json::json;

use crate::server::core::endpoints::resolve_edition;
use crate::server::core::providers::nous::oauth::DeviceLogin;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::{finish_task_error, LoginService, LoginTaskHandle, LOGIN_TIMEOUT_MS};

impl LoginService {
    /// 发起 Nous Portal 设备码登录。
    pub fn start_nous_login(&self) -> Result<LoginTaskHandle, String> {
        let info = resolve_edition(None);
        let handle = self.new_handle_for_provider(info, kind_id(ProviderKind::Nous));
        // 本地关联串（上游 device_code 要到 start 之后才有，见模块头）
        let state = format!("nous-{}", logging::now_ms());
        handle.update(|task| {
            task.state = Some(state.clone());
        });
        self.tasks.register(&state, handle.clone());
        let service = self.clone();
        let task = handle.clone();
        crate::spawn_task(async move {
            service.run_nous_login(task).await;
        });
        logging::log("[Login]", "发起 Nous Portal 网页登录（等待授权…）");
        Ok(handle)
    }

    async fn run_nous_login(&self, handle: LoginTaskHandle) {
        // ① 设备码申请：失败就直接结束（拿不到授权地址，没有可等的东西）
        let mut flow = match DeviceLogin::start().await {
            Ok(flow) => flow,
            Err(error) => {
                finish_task_error(&handle, &error.message);
                return;
            }
        };
        if handle.snapshot().canceled {
            return;
        }
        // 回填授权地址 + 真 state（前端轮询到它就把用户送去浏览器授权）
        let auth_url = flow.auth_url().to_string();
        let real_state = flow.state();
        handle.update(|task| {
            task.auth_url = Some(auth_url.clone());
            task.state = Some(real_state.clone());
        });
        self.tasks.register(&real_state, handle.clone());

        let local_deadline = tokio::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS);
        // 上游给的 expires_at 更权威（到点 device_code 作废），但它可能没给（0）
        let upstream_deadline = match flow.expires_at() {
            0 => None,
            seconds => {
                let now_sec = logging::now_ms() / 1000;
                let remaining = (seconds - now_sec).max(0) as u64;
                Some(tokio::time::Instant::now() + Duration::from_secs(remaining))
            }
        };
        let deadline = match upstream_deadline {
            Some(value) => value.min(local_deadline),
            None => local_deadline,
        };

        loop {
            if handle.snapshot().canceled {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                finish_task_error(&handle, "Nous 网页登录超时，请重新发起");
                return;
            }
            tokio::time::sleep(flow.poll_interval()).await;
            if handle.snapshot().canceled {
                return;
            }
            match flow.poll().await {
                // 还没授权完 —— 继续等
                Ok(None) => continue,
                Ok(Some(tokens)) => {
                    // 与取消共用任务锁：取消先发生就绝不落账号，落盘先发生则视为已完成
                    let mut task = handle.lock();
                    if task.done || task.canceled {
                        return;
                    }
                    // access_token 即推理 Bearer（不再铸 agent key）；`agentKey` 字段
                    // 存同一枚 access_token，续期时会被新的 access_token 覆盖。
                    let expires_at = logging::now_ms() + tokens.expires_in.saturating_mul(1000);
                    let access_token = tokens.access_token.clone();
                    let payload = json!({
                        "accessToken": tokens.access_token,
                        "refreshToken": tokens.refresh_token,
                        "expiresAt": expires_at,
                        "agentKey": access_token,
                        "agentKeyExpiresAt": expires_at,
                        "inferenceBaseUrl": tokens.inference_base_url,
                    });
                    match self.store.add_nous_account(&payload, None, "web") {
                        Ok(account) => {
                            task.session = Some(json!({
                                "accountUid": account.get("id"),
                                "nickname": account.get("name"),
                                "provider": kind_id(ProviderKind::Nous),
                            }));
                            logging::log(
                                "[Login]",
                                "✅ Nous Portal 网页登录完成，账号已加入列表",
                            );
                        }
                        Err(error) => task.error = Some(error.message),
                    }
                    task.done = true;
                    task.finished_at = Some(logging::now_ms());
                    return;
                }
                Err(error) => {
                    finish_task_error(&handle, &error.message);
                    return;
                }
            }
        }
    }
}

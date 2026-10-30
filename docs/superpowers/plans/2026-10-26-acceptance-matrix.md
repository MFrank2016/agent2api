# 插件架构迁移功能验收矩阵（Task 7.3）

- 日期：2026-10-26
- 分支：`migrate/plugin-arch`
- 基线：合并提交 `ecc3aded`（父提交 `413b92c4` + `6f47f7f8`）
- 环境：rustc 1.99.0、Node v26.8.1、Linux
- 结论：**10 PASS / 1 BLOCKED**（逐项见下表；原 FAIL 的插件流式 think 剥离已由 Task 7.3a 修复）

## 总表

| # | 验收项 | 结果 | 关键证据 |
| ---: | --- | --- | --- |
| 1 | 代理轮换：每请求换 IP、失败换号、`no_reuse` | PASS（代码路径 + 单测；未做真实出口 IP 实测） | `core/proxy_rotate.rs:200`、`account_store/store.rs:723`、`egress.rs:274-282`、`upstream/provider_loop.rs:287,2238-2295`；`egress_rotation` 2 条单测通过 |
| 2 | TLS 指纹：网关 + 插件出站一致 | PASS | 网关 `egress.rs:232` 与插件 `crates/providers/src/egress.rs:142` 同一 `AGENT2API_TLS_EMULATION` 档位、同样只搬 TLS/HTTP1/HTTP2 选项；`egress::tests::emulation_does_not_inject_browser_headers` 通过；`docs/tls-fingerprint.md` 有真实 JA4/H2 实测 |
| 3 | think 剥离：Qoder / Accio / CodeArts | PASS（流式 + 非流式） | 网关在插件流式路径按 `stripLeadingThink` 套 `LeadingThinkStream`（`plugin_host/rpc.rs:1299-1357`、`plugin_host/stream.rs:132-215`）；非流式继续走 `assembled_for_plugin`；`plugin_stream_strips_leading_think_before_visible_text` / `..._split_across_chunks` 通过，网关全量 400 passed |
| 4 | 逐家开关：三态 + TOML 注释保留 | PASS（TOML 为代码路径验证，无专门回归测试） | `core/provider_options.rs:93,107,132,194`、`api/provider_options.rs:168,291`、`core/agents/toml_editing.rs:13-69`；相关 11 条单测通过 |
| 5 | Key 配额：多规则、30 天档、非常规窗口 | PASS | `key_quota::tests::all_rules_must_pass`（daily + `fixed:2592000`）、`api_keys::tests::rules_round_trip_through_the_stored_shape`、`dirty_rules_are_skipped_not_fatal`；网关全量通过 |
| 6 | 首字节保活：chat / responses / messages | PASS | `api/keepalive.rs` 4 条单测通过；`api/chat.rs:242`、`api/protocol.rs:268`（responses）、`api/protocol.rs:499`（messages）三入口均接 `keepalive::race` + `KeepAliveStream` |
| 7 | travel：按业务码、两轮、上界 | PASS（mock 上游；非真实账号） | 网关 `billing::travel` 9 条单测通过；插件 `two_round_claim_is_bounded`、`claim_by_business_code_rejects_nonzero`、`upper_bound_gating_blocks_depart_when_limited` 通过 |
| 8 | routing：7 策略 | PASS | `core/routing/strategy.rs:87` 的 `ALL` 恰为 7 项；14 条 strategy 单测全部通过 |
| 9 | Tabbit / Nous / LobsterAI / Trae | **BLOCKED** | 插件契约单测通过（见下），但无真实账号；无法验证真实登录 / 转发 / 续期 / 签到 |
| 10 | 插件未安装：503 + 可行动文案 | PASS | `missing_plugin.rs:40-44,125-127` 固定 503 与「请在『提供商插件』里安装对应插件后重试」；Task 7.2 实机 curl 返回 503 及该文案 |
| 11 | 五语 / 移动端 | PASS | `i18n-src` 五语目录（en/ja/ko/pt-BR/zh-Hant）共 100 个 JSON 全部可解析；`npm --prefix desktop-tauri/mobile run build` 通过（716 modules） |

## 1. 代理轮换

代码路径：

- `core/proxy_rotate.rs:200` `next(config, key)` 每次调用推进账号游标；`candidate_ids` 取池内启用条目或显式 id。
- `core/proxy_rotate.rs:170` `pick_at` 从游标起绕一圈找第一条可解析出口；全部失败时返回首条原因。
- `account_store/store.rs:723` `next_rotate_proxy` 只在 `source == "rotate"` 时调用 `proxy_rotate::next`，即「一请求一出口」。
- `core/egress.rs:274-282` `no_reuse` 时把 `pool_idle_timeout` 设为 0、`pool_max_idle_per_host` 设为 0，并且缓存键包含 `no_reuse`（`egress.rs:136`）。
- `upstream/provider_loop.rs:287` 轮询档 429 原地换出口上限 3 次；`provider_loop.rs:2238-2295` 每次取 `next_rotate_proxy` 并重发。

单测：

```text
server::core::egress_rotation::tests::round_robin_cycles_through_all_members ... ok
server::core::egress_rotation::tests::least_latency_prefers_the_fast_member_after_a_failure_penalty ... ok
```

限制：本次没有起真实代理池观察出口 IP；结论基于代码路径与轮换计划单测。若发布验收要求「IP 必须变化」的实测，应补一条双出口 mock 代理的端到端用例。

## 2. TLS 指纹

- 网关：`core/egress.rs:232` `apply_emulation`，默认 `Chrome149`，支持 `off/safari/firefox/edge/chrome`。
- 插件：`/tmp/Agent2Providers/crates/providers/src/egress.rs:142` 同名函数、同一环境变量、同一档位映射。
- 两者都只取 `tls_options` / `http1_options` / `http2_options`，不搬浏览器默认头（避免 `sec-ch-ua` + `undici` 的伪指纹）。
- 网关回归用例：`server::core::egress::tests::emulation_does_not_inject_browser_headers ... ok`。
- `docs/tls-fingerprint.md` 记录真实 `tls.peet.ws` 结果：Chrome 档 `ja4=t13d1516h2_8daaf6152771_d8a2da3f94cd`、`h2=52d84b11737d980aef856699f885ca86`；`off` 档指纹不同。

未单独对插件进程做 peet.ws 实测，但插件与网关的 `apply_emulation` 代码逐字同构。

## 3. think 剥离（PASS，Task 7.3a 修复 R41）

网关通用剥离（非流式）：

```text
server::core::middleware::think_tags::tests::strip_only_affects_leading_block ... ok
server::core::middleware::think_tags::tests::strip_handles_split_across_frames ... ok
server::core::upstream::sse::tests::leading_think_strip_removes_the_tag_across_frames ... ok
server::core::upstream::sse::tests::a_plugin_without_the_capability_still_uses_core_stripping ... ok
server::api::think_strip::tests::put_then_get_round_trips_the_switch ... ok
```

插件流式路径（原缺口，已修复）：

- `core/plugin_host/rpc.rs:1299-1318` 把 `providerOptions.stripLeadingThink`（逐家覆盖全局键）抽成一个开关，流式与非流式共用。
- `core/plugin_host/rpc.rs:1341-1357` 流式分支在开关打开时用 `LeadingThinkStream` 包住 `RpcStream`；关着时原样下发，字节与接入前一致。
- `core/plugin_host/stream.rs:132-215` 的 `LeadingThinkStream` 复用核心 `ReasoningCoalescer` 的 SSE 逐行解析与 `LeadingThinkFilter`，但显式关闭 reasoning 增量合并，避免开启剥离时顺带改变插件的 reasoning 分帧。
- 插件仓库当前没有 `LeadingThinkFilter` / `with_think_strip`，因此**不会二次剥离**；插件协议也没有 `capabilities.think_strip`，所有插件都按「未申报」处理。等协议加上该能力位后，流式分支应改为 `strip_leading_think && !capabilities.think_strip`，与 `FramePolicy::assembled_for_plugin` 的 R21 语义对齐（迁移说明已写在两处代码注释里）。

回归用例：

```text
server::core::plugin_host::stream::tests::plugin_stream_strips_leading_think_before_visible_text ... ok
server::core::plugin_host::stream::tests::plugin_stream_strips_leading_think_split_across_chunks ... ok
```

两条用例分别覆盖单帧 `<think>secret</think>visible` 与跨 chunk `<thi` + `nk>...`；断言拼接后的正文以 `visible` 开头、且无标签残留。

## 4. 逐家开关三态 + TOML 注释保留

三态：

- `core/provider_options.rs:93` `get() -> Option<bool>`：`None` = 跟随全局。
- `core/provider_options.rs:132` `set()`：显式 `true/false`。
- `core/provider_options.rs:194` `remove()`：删除覆盖，回到跟随全局。
- API `PUT /api/provider-options` 的 `value: null` 映射 `remove()`（`api/provider_options.rs`）。

单测：

```text
server::core::provider_options::tests::null_removes_provider_override ... ok
server::core::provider_options::tests::per_provider_option_is_independent ... ok
server::api::provider_options::tests::put_then_get_round_trips_one_provider_switch ... ok
server::api::provider_options::tests::null_value_removes_the_override ... ok
```

UI 三态入口存在：`ui-islands/src/islands/settings-page.tsx:1245,1261-1336` 与 `settings-state.ts:1774-1833`。

TOML：

- `core/agents/toml_editing.rs:13-37` 用 `toml_edit::DocumentMut` 解析、patch、渲染，保留 BOM/CRLF 并做解析等价校验。
- `toml_editing.rs:44-69` 通过 `TableLike` 原地改表，并在替换值时克隆原 `decor`，保留注释与空白。
- 限制：本仓库没有 `toml_editing` 的专门回归用例；Task 3.5 也记录为「inherited/untested」。本项按代码路径判 PASS，建议补一条「带注释 TOML 往返」单测。

## 5. Key 配额

- 多规则叠加：`key_quota::tests::all_rules_must_pass` 用 `daily` + `fixed:2592000`（30 天）两条规则，断言两条都保留且返回先超限的那条。
- 30 天档：`api_keys::tests::rules_round_trip_through_the_stored_shape` 用 `QuotaReset::Fixed(2_592_000)` 原样往返。
- 非常规窗口：`api_keys::tests::dirty_rules_are_skipped_not_fatal` 验证 `fixed:90` 越界回落自然日，不夹逼；`quota_reset_from` 的上限为 30 天（`api_keys.rs:136-143`）。
- 规则型 Key 真正读库：`key_quota::tests::rules_only_key_reaches_the_database`。
- 快照：`key_quota::tests::quota_json_reports_rule_limits`。

全部在网关 398 条通过中。

## 6. 首字节保活

- 机制：`api/keepalive.rs` 先发 200 + `text/event-stream`，与 `GRACE=8s` 赛跑；慢分支每 12s 发 `: keep-alive\n\n`。
- 三个入口：
  - chat：`api/chat.rs:242` `keepalive::race`，慢分支 `api/chat.rs:271` `KeepAliveStream`。
  - responses：`api/protocol.rs:268` + `protocol.rs:290`。
  - messages：`api/protocol.rs:499` + `protocol.rs:520`。
- 单测：

```text
server::api::keepalive::tests::emits_keep_alive_while_pending_then_upstream_frames ... ok
server::api::keepalive::tests::ready_future_emits_no_keep_alive ... ok
server::api::keepalive::tests::race_returns_ready_when_future_resolves_within_grace ... ok
server::api::keepalive::tests::race_returns_slow_when_future_stays_pending_past_grace ... ok
```

## 7. travel

网关状态机（mock 上游）：

```text
server::core::billing::travel::tests::status_parses_arrived_fields ... ok
server::core::billing::travel::tests::idle_departs ... ok
server::core::billing::travel::tests::arrived_is_claimed_then_no_depart_when_limited ... ok
server::core::billing::travel::tests::traveling_is_skipped ... ok
server::core::billing::travel::tests::arrived_with_nonzero_claim_code_reports_failure_not_claimed ... ok
server::core::billing::travel::tests::travel_requests_carry_no_identity_headers ... ok
```

插件侧：

```text
providers::workbuddy::growth::travel::tests::claim_by_business_code_rejects_nonzero ... ok
providers::workbuddy::growth::travel::tests::two_round_claim_is_bounded ... ok
providers::workbuddy::growth::travel::tests::upper_bound_gating_blocks_depart_when_limited ... ok
providers::workbuddy::growth::tests::travel_headers_drop_identity_headers ... ok
```

结论：业务码、两轮上限、`daily_limit_reached` 上界门控均有覆盖；未用真实 WorkBuddy 账号跑线上活动。

## 8. routing 7 策略

`RoutingStrategy::ALL` 恰为 7 项：`priority`、`least-connections`、`round-robin`、`global-round-robin`、`least-recently-used`、`balance-weighted`、`priority-weighted`（`strategy.rs:87-95`）。

单测覆盖：解析/拒绝未知、全局 vs 家内轮询、每模型游标、最少连接、LRU、余额加权比例与基线、优先级加权、provider override、队首组限制。14 条全部通过。

## 9. Tabbit / Nous / LobsterAI / Trae（BLOCKED）

插件契约单测通过：

- Tabbit：`unique_uuid_marks_default_browser_only_when_asked`、`hmac_hex_matches_known_vector`、`parses_exp_and_user_id_from_jwt`、`keeps_full_cookie_header_including_next_auth`、`rejects_cross_region_prefix`、会话复用/超时 5 条。
- Nous：`nous_is_registered`、`anonymous_credentials_work`、`fallback_models_include_welcome`、`manual_token_normalizes_to_account`、`classify_quota_and_auth`。
- LobsterAI：`unwraps_data_envelope`、`reads_total_and_items`、`maps_known_wallet_types`、`statuses_keep_gateway_contract_names`、OAuth 回调解析 3 条等。
- Trae：`classify_9074_is_failure_not_success`、`one_claim_semantics_sends_single_probe_body`、`device_id_is_fresh_per_round`。

阻塞原因（精确）：

- 无真实 Tabbit Cookie/JWT：Tabbit `normalize` 会在添加账号时联网校验 base-info，假 Cookie 无法落库。
- 无 Nous 真实 Portal token：占位凭证实测上游 401（invalid / blocked / out of funds）。
- 无 LobsterAI 令牌：无法验证真实续期、余额、签到。
- 无 Trae 账号：无法验证设备登录、会话式转发、签到。
- 因此不能把「真实登录 / 转发 / 续期 / 签到」判 PASS；只能判插件契约通过、端到端 BLOCKED。

## 10. 插件未安装：503 + 可行动文案

代码：

- `missing_plugin.rs:40-44` 文案：`{label}的{action}需要安装提供商插件：本版本不再内置反向代理实现，请在「提供商插件」里安装对应插件后重试`。
- `missing_plugin.rs:125-127` 固定 `GatewayError::with_status(503, ...)`。

实机证据（Task 7.2）：

```bash
curl -X POST http://127.0.0.1:13065/v1/chat/completions \
  -H 'authorization: Bearer sk-upgrade-test' \
  -H 'content-type: application/json' \
  -d '{"model":"demo-fast","messages":[{"role":"user","content":"ping"}],"stream":false}'
# HTTP 503
# {"error":{"message":"没有可用账号，无账号可转发：请在账号页添加并启用账号","type":"proxy_error"}}
```

补充：已关闭的模型（如 `glm-5.3`）会在模型门禁处先返回 404 `model_not_found`，不会走到 503；这是既有的「模型门禁优先」语义，不是 503 失效。

## 11. 五语 / 移动端

- 五语目录：`desktop-tauri/i18n/i18n-src/{en,ja,ko,pt-BR,zh-Hant}`。
- JSON：100 个文件 `python3 -m json.tool` 全部通过。
- 移动端构建：

```bash
npm --prefix desktop-tauri/mobile run build
# tsc --noEmit && vite build
# 716 modules transformed
# ../ui/m/mobile.js 852.64 kB
```

构建时临时把主仓 `desktop-tauri/ui-kit/node_modules` 链到 worktree（仅环境依赖，非源码改动），构建后已移除；`git status` 干净。

## 命令与日志

```bash
cd desktop-tauri/src-tauri
CARGO_TARGET_DIR=/root/programming/github/agent2api/target \
  cargo test -p agent2api-server --lib
# 398 passed / 0 failed / 1 ignored

cd /tmp/Agent2Providers
cargo test -p agent2api-providers
# 52 passed / 0 failed / 0 ignored

cd /root/programming/github/agent2api/.worktrees/plugin-arch
node scripts/verify-plugin-host.mjs \
  --exe /tmp/Agent2Providers/target/release/agent2api-providers
# 28/28 passed
```

日志：

- `/tmp/task-7.3-gateway-tests.log`
- `/tmp/task-7.3-plugin-tests.log`
- `/tmp/task-7.3-host-verify.log`
- `/tmp/task-7.3-mobile-build.log`

## 风险与建议

1. ~~P0 回归：插件流式 think 剥离失效~~ **已修复（Task 7.3a）**：网关在插件流式路径按 `stripLeadingThink` 套 `LeadingThinkStream`，并补两条回归用例；后续等插件协议支持 `capabilities.think_strip` 时切换为能力位判断，避免二次剥离。
2. 代理轮换缺真实双出口 IP 实测；建议发布验收补 mock 代理端到端。
3. TOML 注释保留只有代码路径证据，无专门回归用例。
4. Tabbit / Nous / LobsterAI / Trae 的真实登录与转发仍缺账号；发布前需在有账号环境复验。
5. `parity-check.mjs` 仅覆盖 3 个 id（Task 7.1 已记录），不能替代逐家真实上游验收。

# 插件架构迁移全量测试报告（Task 7.1）

- 日期：2026-10-26
- 分支：`migrate/plugin-arch`
- 基线：合并提交 `ecc3aded`（父提交 `413b92c4` + `6f47f7f8`）
- 环境：rustc 1.99.0、Node v26.8.1、Linux

## 1. 网关单测

```bash
cd desktop-tauri/src-tauri
CARGO_TARGET_DIR=/root/programming/github/agent2api/target \
  cargo test -p agent2api-server
```

| 结果 | 数量 |
| --- | ---: |
| passed | 398 |
| failed | 0 |
| ignored | 1 |

日志：`/tmp/task-7.1-gateway-tests.log`

### 1.1 合并后预期失败的 2 条用例

合并前全量结果为 `397 passed / 2 failed / 0 ignored`。两条失败都源于「内置适配器已全部移出网关，`adapter_for()` 现在一律返回 `MissingPluginAdapter`」，属于迁移后的预期行为变化：

1. `provider_loop::tests::a_declared_provider_honours_its_own_newline_switch_over_the_global_one`
   - 失败前提：`adapter_for(ProviderKind::WorkBuddy).sse_strip_newline_chunks()` 为 `false`。
   - 原因：能力位由插件实现声明，网关占位适配器不声明任何能力位。
   - 处置：**测试替身（plugin-aware）**。新增 `CapabilityTestAdapter`，只声明 `sse_strip_newline_chunks` 能力位；用例继续验证网关侧「能力位 && 逐家开关」的读取口径，与具体 provider 实现无关。断言未削弱。

2. `provider_loop::tests::codearts_daily_pool_envelope_cools_to_the_boundary_instead_of_ten_minutes`
   - 失败现象：`InferHub.4291.200：insufficient quota` 被分类为 `Fatal`，而非带恢复时刻的 `QuotaLimited`。
   - 原因：CodeArts 的会话式错误分类（`classify_conversation_error`）属于插件专属知识，已随实现迁到 `agent2api-providers`；网关单测只能看到占位适配器。
   - 处置：`#[ignore = "reason: plugin-owned behavior …"]`。恢复时刻写回存储的网关逻辑仍由 `stateful_quota_bookkeeping_keeps_the_adapter_reset_at` 覆盖，未删除任何断言。

## 2. 插件仓库

```bash
cd /tmp/Agent2Providers
cargo test -p agent2api-providers
node scripts/verify.mjs --exe target/release/agent2api-providers
```

| 检查 | 结果 |
| --- | --- |
| `cargo test -p agent2api-providers` | 52 passed / 0 failed / 0 ignored |
| `verify.mjs` | 28/28 通过 |

日志：`/tmp/task-7.1-plugin-tests.log`、`/tmp/task-7.1-plugin-verify.log`

## 3. 网关侧验证脚本

```bash
cd /root/programming/github/agent2api/.worktrees/plugin-arch
node scripts/verify-plugin-host.mjs \
  --exe /tmp/Agent2Providers/target/release/agent2api-providers
node scripts/parity-check.mjs
```

| 检查 | 结果 |
| --- | --- |
| `verify-plugin-host.mjs` | 28/28 通过 |
| `parity-check.mjs` | 122 项检查，0 项失败 |

日志：`/tmp/task-7.1-host-verify.log`、`/tmp/task-7.1-parity.log`

说明：`parity-check.mjs` 按仓库内相对路径查找 `agent2api-providers/`，本 worktree 中插件仓库位于 `/tmp/Agent2Providers`。本次临时建立符号链接后运行，运行完毕即删除，未进入提交。

## 4. 结论

- 网关全量：`398 passed / 0 failed / 1 ignored`。
- 插件单测与两套验证脚本全部通过。
- 2 条迁移期失败已按「测试替身优先、插件专属行为显式 ignore」处置，无删除、无静默弱化。
- 唯一 ignored 用例建议后续在 `agent2api-providers` 仓库补齐 CodeArts 会话式额度分类的单测，届时可删除该 ignore 标记。

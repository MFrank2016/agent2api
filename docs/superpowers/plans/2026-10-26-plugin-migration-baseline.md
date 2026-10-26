# 插件架构迁移基线

- **日期**：2026-10-26
- **关联计划**：`.omo/plans/2026-10-26-plugin-architecture-migration.md` Task 0.2

## 1. 冻结的 ref

| 名称 | SHA | 说明 |
|---|---|---|
| `main` | `a98a2bf4f7d5d64f8042ae0b547671bd26462ed6` | 迁移基线 |
| `backup/main-pre-plugin-migration` | `a98a2bf4` | 迁移前备份分支 |
| `migrate/plugin-arch` | `a98a2bf4` | 迁移工作分支 |
| `upstream/beta` | `6f47f7f868466196e9edafc0b4980382cd19e838` | 目标上游（v3.3.0-beta.4） |
| `feat/tabbit-provider` | `d95a0916fc6bbaa41aebab28d6653a0401d78b43` | Tabbit 参考分支，不并入迁移基线 |

## 2. 工作树

```text
/root/programming/github/agent2api                            a98a2bf4 [main]
/root/programming/github/agent2api/.worktrees/plugin-arch     a98a2bf4 [migrate/plugin-arch]
/root/programming/github/agent2api/.worktrees/tabbit-provider d95a0916 [feat/tabbit-provider]
```

`.worktrees/` 由 `.git/info/exclude` 忽略，不会污染 `git status`。

## 3. 冲突快照

命令：

```bash
git merge-tree --write-tree --name-only --messages main upstream/beta > /tmp/plugin-merge-tree.txt 2>&1 || true
```

结果：

| 指标 | 值 |
|---|---|
| 合并树 OID | `7b182accbbd62975ffa2d3f591aba3cfecb30c24` |
| 冲突文件 | **369** |
| 冲突 hunk | **1,586**（来自探索代理的逐文件统计） |
| 内容冲突 | 226 文件 / 1,091 hunk |
| add/add | 99 文件 / 495 hunk |
| modify/delete | 44 文件 / 0 hunk |
| 机械冲突 | 157 文件 / 799 hunk（i18n / lockfile / Android） |
| 语义冲突 | 212 文件 / 787 hunk |

冲突文件按顶层目录：

| 目录 | 冲突文件数 |
|---|---:|
| `desktop-tauri/src-tauri` | 105 |
| `desktop-tauri/i18n` | 76 |
| `desktop-tauri/mobile` | 71 |
| `desktop-tauri/ui` | 68 |
| `desktop-tauri/ui-islands` | 28 |
| `desktop-tauri/ui-kit` | 2 |
| 根文件（README / package.json / LICENSE / .gitignore 等） | 5+ |

完整清单：`/tmp/plugin-merge-tree.txt`（文件名在第 2–370 行）。

## 4. 基线测试

命令：

```bash
cd desktop-tauri/src-tauri
CARGO_TARGET_DIR=/root/programming/github/agent2api/target \
  cargo test -p agent2api-server
```

日志：`/tmp/plugin-baseline-tests.log`

状态：**完成**。

结果：**821 passed / 1 failed / 11 ignored**。

唯一失败：`server::api::think_strip::tests::put_then_get_round_trips_the_switch`。日志显示「数据库不可用（本次改动仅内存生效）」；单跑该用例通过（`1 passed`），判定为全量并行下的测试环境/共享状态偶发，不是产品缺陷，不作为迁移阻塞。

`codearts::session::renewal_keeps_beating_until_stop` 本轮未失败（main 已含 `dc298c60` 的 20ms/200ms/≥4 修复）。

## 5. 结论

- 迁移分支与备份已就绪，`main` 未被改动。
- 369 个冲突文件确认了「不能一次性 merge 后直接提交」；必须按计划 Phase 1–4 分阶段解决。
- 许可证仍是 Phase 1 的阻断门禁；见 `docs/superpowers/specs/2026-10-26-plugin-migration-license.md`。

# ADR：插件架构迁移的许可证决策

- **日期**：2026-10-26
- **状态**：Accepted（用户已确认 A：AGPL-3.0-only）
- **决策人**：用户
- **关联计划**：`.omo/plans/2026-10-26-plugin-architecture-migration.md` Task 0.1

## 背景

本 fork 计划完整跟进上游 `aimod-cc/Agent2API` 的插件架构（`upstream/beta` = `6f47f7f8`，v3.3.0-beta.4）。上游把 provider 实现移入独立插件仓库，网关通过 JSON-RPC 子进程加载插件。

这条迁移路线会把上游代码合入本仓库，因此必须先确认许可证是否兼容。

## 事实

| 项 | 上游 | 本 fork |
|---|---|---|
| 根 `LICENSE` | **GNU AGPL-3.0** | **MIT License + 使用声明** |
| `desktop-tauri/src-tauri/server/Cargo.toml` | `license = "AGPL-3.0-only"` | `license-file = "../../LICENSE"` |
| 插件实现仓库 `ProvidersPlugin/Agent2Providers` | **AGPL-3.0**（`LICENSE` 与 `license-file`） | 计划依赖其 `provider-protocol` / `provider-sdk` crate |
| 现有使用声明 | 无 | LICENSE 正文后附第 3 条：禁止商业用途、禁止二次分发牟利、禁止批量账号运营 |

AGPL-3.0 的关键义务：

- 分发修改后的程序时，必须以 AGPL 提供完整对应源码；
- 如果用户通过网络与修改后的程序交互，必须向这些用户提供对应源码；
- 不得对 AGPL 授予的权利追加额外限制（例如禁止商业使用）。

本 fork 现有使用声明中的「禁止商业用途 / 二次分发牟利」与 AGPL 允许商业使用的条款存在直接冲突。AGPL 代码也不能被单独重新许可为 MIT。

## 选项

### A. 采纳 AGPL-3.0-only（与上游一致）

**做法**
- 根 `LICENSE` 改为 AGPL-3.0；
- `server/Cargo.toml` 改为 `license = "AGPL-3.0-only"`；
- `package.json` / `desktop-tauri/package.json` 的 `license` 字段同步；
- README / 安装文档更新许可证说明；
- 重新审视并删除与 AGPL 冲突的「禁止商业用途」附加限制（或由法律顾问确认可保留的边界）；
- 在 UI / 文档中提供「获取源码」入口，满足 network-use 义务。

**优点**
- 与上游、插件仓库许可证一致，可以直接 merge / 依赖；
- 技术工作量最小，计划的任务列表按此选项编写。

**缺点**
- 整个衍生作品变成 AGPL，现有 MIT + 附加限制的授权模式结束；
- 商业使用、二次分发、托管服务等场景的合规要求显著提高；
- 需要法律/产品确认，不能由工程单方面决定。

### B. Clean-room 重实现插件宿主与协议

**做法**
- 不复制上游 `plugin_host/*`、`provider-protocol`、`provider-sdk` 或插件实现；
- 由本仓库独立设计 JSON-RPC 契约、manifest、签名与插件生命周期；
- 保留 MIT + 使用声明；
- 需要独立的法律意见与完整的协议设计文档。

**优点**
- 保持现有许可证与使用声明；
- 不被 AGPL 的 network-use / 源码提供义务约束。

**缺点**
- 工作量远大于 A：协议、SDK、宿主、21 家 provider 插件都要重写或重新实现；
- 无法直接使用上游插件仓库与 pack，生态兼容性下降；
- 仍有「独立实现是否受上游专利/合同约束」的法律风险，需要专业评估；
- 本计划的 39 个 Task 需要重写，不能按现有任务列表执行。

### C. 停止跟进插件架构

**做法**
- 保持当前内置 provider 架构；
- 只做必要的安全/兼容修复，不再同步上游插件架构。

**优点**
- 零许可证变更、零迁移风险；
- 现有功能与发布流程不变。

**缺点**
- 与上游架构持续分叉，后续同步成本越来越高；
- 无法获得插件隔离、第三方 provider、插件更新等能力；
- 上游 bug 修复与新 provider 需要手工移植。

## 影响面

- **代码**：A 可以直接 merge；B 需要重写插件宿主/协议/SDK；C 不 merge。
- **发布**：A 需要在 Release / 文档中提供源码与 AGPL 声明；B/C 保持现有发布方式。
- **商业/托管**：A 对网络服务提供者有源码提供义务；B/C 按现有使用声明执行。
- **贡献者**：A 需要贡献者接受 AGPL；B/C 保持 MIT + 使用声明。

## 建议

- 如果目标是**尽快、完整地跟进上游插件架构**，选 **A**；这是唯一能直接复用上游宿主、协议与插件生态的选项。
- 如果**必须保持 MIT + 使用声明**，选 **B**，但需要把本计划重写为 clean-room 计划，并接受显著更大的工作量。
- 如果两者都不可接受，选 **C**。

工程侧不替用户做许可证决策；在 `Decision` 填写并确认之前，不得进入 Phase 1（merge 上游代码）。

## 决策

```
Decision: A（采纳 AGPL-3.0-only）
Confirmed by: 用户（2026-10-26 会话确认）
Date: 2026-10-26
Notes: 执行方式 subagent-driven-development；从 Phase 1 起按 AGPL 处理，LICENSE / Cargo.toml / package.json / README 在 Task 6.4 统一收口。
```

## 参考

- 上游 `LICENSE`：`git show upstream/beta:LICENSE`
- 上游 `server/Cargo.toml`：`license = "AGPL-3.0-only"`
- 插件仓库 `ProvidersPlugin/Agent2Providers` `LICENSE`：AGPL-3.0
- 本 fork `LICENSE`：MIT + 使用声明
- 本 fork `README.md`「许可」一节

# 发布拓扑决策：in-repo 还是三仓库

- **日期**：2026-10-26
- **状态**：Proposed（待用户确认）
- **关联计划**：`.omo/plans/2026-10-26-plugin-architecture-migration.md` Task 0.4 / Task 6.2

## 背景

上游插件架构把发布拆成三个仓库：

1. **网关仓库**（本仓库）：源码、tag、Release 说明；
2. **构建仓库** `Agent2Builder/agent2api-releases`：实际编译安装包 / 镜像；
3. **provider 插件仓库** `ProvidersPlugin/Agent2Providers`：插件二进制与 pack；
4. **索引仓库** `Agent2plugin/Agent2plugin`：`index.json` 与 `plugins/<id>.json`。

本 fork 目前是 in-repo 构建：`.github/workflows/build.yml` 在 tag 上直接出安装包，`scripts/release.sh` 也在本仓库执行。

## 事实

| 项 | 上游 | 本 fork |
|---|---|---|
| `scripts/release.sh` 默认构建仓库 | `BUILD_REPO=Agent2Builder/agent2api-releases` | 无 `BUILD_REPO`，in-repo |
| tag 触发 | 网关 tag 只发一条构建请求 | `build.yml` / `docker.yml` 直接在 tag 上构建 |
| 插件发布 | `ProvidersPlugin/Agent2Providers` 独立 tag / Release | 无 |
| 插件索引 | `Agent2plugin/Agent2plugin` | 无 |

## 选项

### 选项 1：保持 in-repo 构建

**做法**
- 继续用 `.github/workflows/build.yml` / `docker.yml` 在 tag 上构建；
- 插件 pack 作为额外产物附在同一个 Release，或单独发；
- `scripts/release.sh` 不引入 `BUILD_REPO`。

**优点**
- 改动最小；
- 现有 secrets / 权限 / 发布习惯不变；
- 不需要额外仓库权限。

**缺点**
- 与上游 `release.sh` 持续分叉，每次同步都要解冲突；
- 网关与插件版本耦合在一个 Release，无法独立更新插件；
- 上游的「网关 tag 只发构建请求」模型无法复用。

### 选项 2：接入三仓库模型

**做法**
- 网关仓库只打 tag，`scripts/release.sh` 默认 `BUILD_REPO=Agent2Builder/agent2api-releases`；
- 插件仓库独立 tag / Release，产出 5 平台 pack；
- 索引仓库更新 `plugins/<id>.json` 与 `index.json`；
- 本 fork 需要自己的构建仓库 / 插件仓库 / 索引仓库，或获得上游仓库权限。

**优点**
- 与上游发布流程一致，后续同步冲突少；
- 插件可独立更新，不必等网关发版；
- 支持第三方插件与签名校验。

**缺点**
- 需要新增仓库、secrets、权限与发布文档；
- 发布链路更长，排错面更大；
- 如果使用上游仓库，需要上游授权；如果自建，需要维护三套 CI。

## 需要的权限 / secrets（选项 2）

- 构建仓库：`contents: write`、`actions: write`、Release 上传权限；
- 插件仓库：tag / Release / 5 平台构建矩阵；
- 索引仓库：`contents: write`；
- 签名私钥：ed25519，存 GitHub Actions secret，不得入库；
- 网关仓库：tag 推送权限、跨仓库 `workflow_dispatch` token。

## 建议

- 如果目标是**与上游长期同步**，选 **选项 2**；这是上游 `release.sh` 的默认模型，能避免每次同步都改发布脚本。
- 如果短期只想把插件架构跑通、暂不重构发布，选 **选项 1**，但在 Phase 6 必须把 `release.sh` 的分叉点隔离成一个小 patch，降低后续冲突。
- 无论选哪个，插件签名私钥都不得入库；签名流程按 `Agent2Providers/scripts/sign.mjs`。

## 决策

```
Decision: 待用户确认（in-repo / three-repo）
Confirmed by: 待填
Date: 待填
Notes:
```

## 回滚

- 选项 1 → 选项 2：保留现有 workflow，新增构建仓库触发；失败时回退到 tag 直接构建。
- 选项 2 → 选项 1：删除 `BUILD_REPO` 分支，恢复 `build.yml` / `docker.yml` 的 tag 触发。
- 插件发布失败：网关 Release 不挂插件包；索引仓库回退到上一个 `index.json`；已安装用户不受影响（插件独立更新）。

# AGENTS.md

> **涉及分支、发版、版本号的操作，一律按 Skill `release-flow` 执行：**
> **`.agents/skills/release-flow/SKILL.md`** —— 动手前先读它，细则一律以该文件为准，本文件只作入口指路。
>
> 覆盖范围：切分支 / 给 PR 选 base / 合并 PR / 回复 Issue、PR / 改版本号 / 写更新日志 /
> 打 tag / 挂 GitHub Release / 发 beta 预发布版 / 发版验收 / 排查发版失败。

## 两条红线（先问人，不得自行推进）

1. **发版**：改版本号、推 `main` / `beta`、打 tag、挂 Release 之前，先把版本号与「这次要发什么」讲清楚，等用户确认；**未经确认不得推任何分支或 tag** —— tag 一旦推出去就会触发构建、产出公开的安装包与镜像，收不回来。
2. **更新日志**：写好的日志（= 发布提交信息 = Release 正文）必须先给用户逐条过目；按反馈改完**再确认一次**才能提交。

其余细则（分支落脚点、署名与 AI 落款、六处版本号、正式版 / beta 完整流程、验收清单、已知坑与排查）见上述 Skill。

## 三仓库发布拓扑（2026-10 起）

网关 tag 只发起构建，实际发布分散在网关 / 构建 / 插件 / 索引四类仓库；细则见 Skill 第 0.5 节。

| 仓库 | 作用 |
|---|---|
| 本仓库（网关） | 源码、tag、更新日志；`build.yml` 只派发 |
| `Agent2Builder/agent2api-releases`（`BUILD_REPO`） | 编译安装包 / 镜像；网关 Release 在这里 |
| `ProvidersPlugin/Agent2Providers`（`PLUGIN_REPO`） | 插件独立 tag，产出 5 平台 pack |
| `Agent2Builder/agent2api-providers-releases`（`PLUGIN_RELEASE_REPO`） | 插件 Release（5 平台 zip + signature） |
| `Agent2plugin/Agent2plugin`（`INDEX_REPO`） | `app.json` / `plugins/<id>.json` / `index.json` |

- 网关 tag 只触发构建仓库；插件与索引不随网关 tag 自动更新，各自走发布流程。
- 网关版本号同步六处：根 `package.json`、`desktop-tauri/package.json`、`desktop-tauri/src-tauri/Cargo.toml`、`desktop-tauri/src-tauri/tauri.conf.json`、`desktop-tauri/src-tauri/server/Cargo.toml`、`desktop-tauri/mobile/package.json`；插件版本在插件仓库独立维护。
- 正式版 tag 落 `main`；beta tag 落 `beta` 且带 `-beta.N`，Release 标 **Prerelease**。
- 发布前先 dry-run：`bash scripts/release.sh --dry-run vX.Y.Z`（不调用 gh、不推送、不打 tag）。
- **红线**：未经用户确认，不得推任何分支 / tag / Release；dry-run 也不得推送。

## 文档落点

安装 / 部署 / 升级 / 排错说明只在 `docs/install.md`（另有 5 份译文 `docs/install.{en,zh-Hant,ja,ko,pt-BR}.md`，六份一一对应）里维护；各语言 README 只留「快速开始」速查表 + 一个指向安装文档的链接，**不要再把安装细节写回 README**。产物名、端口、环境变量、安装脚本参数有改动时，六份一起改。

**README 语言落点：根 `README.md` 是英文**（GitHub 默认展示），简体中文在 `docs/README.zh-CN.md`，其余四份为 `docs/README.{zh-Hant,ja,ko,pt-BR}.md`。中文仍是文案源语言，改支持矩阵等内容时六份同步，别把中文写回根目录。

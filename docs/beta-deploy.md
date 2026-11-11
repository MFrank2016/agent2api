# beta 发版（本仓 fork 标准流程）

> 本仓是 `aimod-cc/Agent2API` 的定制 fork（`MFrank2016/agent2api`），**不使用上游的三仓库发布链**
> （tag → 构建仓库 → 安装包/镜像）。这里的「发版」= 把 `beta` 打成镜像、部署到 107 服务器。
> 上游官方发版流程见 Skill `release-flow`，两者互不替代。

- 仓库：`/Users/mfrank/Documents/programming/github/agent2api`
- 上游：`https://github.com/aimod-cc/Agent2API`（只取 `beta` 分支）
- 服务器：`ssh ezbookkeeping`（`107.151.246.25`），代码在 `/root/programming/github/agent2api`，
  容器 `agent2api`，面板/网关 `127.0.0.1:3065`，公网入口 `https://agent2api.061995.xyz`

---

## 0. 红线（动手前必须确认）

1. **版本号**：发版前先和用户确认这次用哪个版本号（本仓版本线是 `3.4.x`，不要沿用上游的
   `3.5.0-beta.N`，否则更新检查会混淆）。
2. **推送与部署**：推 `beta`、替换线上容器之前，先把「版本号 + 这次发了什么」讲清楚并得到确认。
3. **不打 tag、不建 Release**：fork 没有发布流水线，tag 只会误导。

## 1. 准备工作

```bash
cd /Users/mfrank/Documents/programming/github/agent2api
git status --short          # 必须干净
git branch -vv              # 确认在 beta
```

## 2. 合入上游新提交（有则做，无则跳过）

```bash
# 临时引用，避免给仓库加 remote
git fetch https://github.com/aimod-cc/Agent2API.git main:refs/tmp/up-main beta:refs/tmp/up-beta
git log --oneline HEAD..refs/tmp/up-beta        # 有没有新提交

git merge --no-ff --no-commit refs/tmp/up-beta
git diff --name-only --diff-filter=U | wc -l    # 冲突规模
```

冲突按三条线处理（并行、文件互不重叠）：

| 线     | 范围                                                  | 处理规则                                                                                                                             | 验证                                                                                             |
| ------ | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------ |
| 多语言 | `desktop-tauri/i18n/i18n-src/**`                      | 键取并集；同键一边空串取非空；两边都非空保留本仓值                                                                                   | `node desktop-tauri/i18n/build.mjs`，无 `OPENCC_MISSING`                                         |
| 前端   | `ui-islands` / `mobile` / `ui-shared` / `ui-kit` 源码 | 两边功能都保留；**产物不手改**，改源码后重新构建                                                                                     | `npm --prefix desktop-tauri/ui-islands run build`、`npm --prefix desktop-tauri/mobile run build` |
| Rust   | `desktop-tauri/src-tauri/**`                          | 保留本仓架构（`AccountEgress` 出口、脱敏、Key 身份、keepalive、wreq 客户端），并入上游新能力；上游新代码若写 `reqwest` 要改成 `wreq` | `cargo check -p agent2api-server --all-targets`                                                  |

版本号冲突一律保留本仓值（例如 `3.4.x` 对 `3.5.0-beta.N`）。
上游把某个文件拆成目录（如 `routing.rs` → `routing/`）时，把上游新增逻辑移植进本地对应文件，原文件按删除解决。

**编译环境**（macOS）：

```bash
brew install cmake                    # BoringSSL 构建需要
export CPLUS_INCLUDE_PATH=$(xcrun --show-sdk-path)/usr/include/c++/v1   # 本机 CommandLineTools 缺 C++ 头
```

## 3. 版本号递增并出产物

默认末位 +1（本次 `3.4.2` → `3.4.3`）。六处版本号一起改：

1. `package.json`
2. `desktop-tauri/package.json`
3. `desktop-tauri/src-tauri/Cargo.toml`
4. `desktop-tauri/src-tauri/tauri.conf.json`
5. `desktop-tauri/src-tauri/server/Cargo.toml`
6. `desktop-tauri/mobile/package.json`

顺带同步：

```bash
sed -i '' "s/旧版本/新版本/g" \
  desktop-tauri/mobile/package-lock.json \
  desktop-tauri/src-tauri/Cargo.lock \
  desktop-tauri/ui/index.html desktop-tauri/ui/login.html      # 静态入口 ?v= 缓存戳

npm --prefix desktop-tauri/mobile run build                    # 重建 mobile.js（版本常量）与 sw.js（缓存名）
CPLUS_INCLUDE_PATH=$(xcrun --show-sdk-path)/usr/include/c++/v1 \
  cargo metadata --manifest-path desktop-tauri/src-tauri/Cargo.toml --format-version 1 > /dev/null
```

提交信息（= 本次发布说明）：

```
chore(release): 版本号与产物对齐 X.Y.Z
```

## 4. 提交与推送

```bash
git add -A && git commit -m "chore(release): 版本号与产物对齐 X.Y.Z"
git push origin beta
```

## 5. 服务器部署

镜像必须在**服务器上原生构建**：本机 arm64 打不出 amd64 镜像，Dockerfile 的架构校验只允许
`amd64→amd64/arm64` 或 `arm64→arm64`。

```bash
ssh ezbookkeeping
cd /root/programming/github/agent2api
git fetch origin
git checkout --detach origin/beta        # 注意：beta 分支被 .worktrees/plugin-arch 占用，不能 checkout beta
git log --oneline -1
docker compose build                     # 冷构建约 40–60 分钟（缓存被清过时）
docker compose up -d
```

## 6. 验证清单

```bash
docker ps --filter name=agent2api --format "{{.Names}} {{.Status}}"        # Up (healthy)
docker exec agent2api /usr/local/bin/agent2api-server --version            # 与 tag/版本号一致
curl -s http://127.0.0.1:3065/health                                       # {"status":"ok"}
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:3065/            # 302（跳登录）
docker logs --since 10m agent2api 2>&1 | grep -iE "panic|fatal|error"      # 空
docker stats agent2api --no-stream
curl -s -o /dev/null -w "%{http_code}\n" https://agent2api.061995.xyz/health   # 200（本机执行）
```

面板里的真实功能（登录、签到、网关调用）不在自动化范围内，需要时人工过一眼。

## 7. 清理

```bash
docker builder prune -a -f      # 构建缓存整份回收（数 GB），下次构建变回冷启动
docker image prune -f           # 悬空镜像
df -h /
```

## 8. 回滚

当前做法：上一版镜像会被新构建替换并回收，**回滚需要切回旧提交重新构建**（冷构建 40–60 分钟）。
改进项（尚未执行）：每次部署前给旧镜像打版本标签，例如

```bash
docker tag $(docker inspect agent2api --format '{{.Image}}') agent2api:rollback-<旧版本>
```

回滚时 `docker tag agent2api:rollback-<旧版本> agent2api:local && docker compose up -d` 即可秒级回退。

## 9. 环境坑（都踩过）

- **prettier**：本机全局 opencode 配置会在写文件后按「双引号 + 分号」重排，与仓库风格（单引号、
  无分号、120 列）不符。冲突解决一律用脚本落字，不要跑格式化。
- **i18n 依赖**：`desktop-tauri/i18n` 需要 `opencc-js`，缺了 zh-Hant 会退化成只剩人工译文；
  安装可能要 `--allow-remote=all --allow-git=all`。
- **node_modules**：`ui-islands`、`mobile`、`ui-kit`、`i18n` 各自独立；typecheck 报缺模块基本是
  没装 `ui-kit` 的依赖。
- **服务器 worktree**：`beta` 分支被 `.worktrees/plugin-arch` 占用，部署用 detached HEAD。
- **服务器内存**：8 核 / 7 GB（可用约 3 GB），`docker compose build` 用 Dockerfile 默认的
  `CARGO_BUILD_JOBS=2`，别调大。

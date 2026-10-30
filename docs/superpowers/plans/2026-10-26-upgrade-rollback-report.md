# 升级 / 降级 / 卸载验证报告（Task 7.2）

- 日期：2026-10-31
- 分支：`migrate/plugin-arch`
- 基线：`ab5ee7c3`（Task 7.1 全量测试通过）
- 工具链：rustc 1.99.0 / Node v26.8.1 / Linux / Python 3.12（`sqlite3` 3.40.1）
- 验证目录：`/tmp/agent2api-upgrade-home`（升级后）
- 升级前副本：`/tmp/agent2api-upgrade-pre/agent2api.db`
- 插件包：`/tmp/Agent2Providers/dist/agent2api-providers-0.2.1-x86_64-unknown-linux-gnu.zip`
- **未修改仓库 `data/`，未使用真实账号，未 push / 未打 tag。**

## 1. 结论

| 项目 | 结果 |
| --- | --- |
| v17 → v21 升级 | **通过**：`user_version` / `dbSchemaVersion` 均为 21 |
| Key / 配额 / 用量保留 | **通过**：Key、`quotaRules`、历史请求用量、`account_usage_records` 全部保留 |
| 旧账号可读 | **通过**：2 条 v17 遗留 WorkBuddy 账号行完整读入；安装插件后正常显示 |
| 插件安装 / 启用 / 宿主加载 | **通过**：0.2.1 包安装成功，宿主 `running=true`，33 家 provider 注册 |
| 卸载插件 | **通过**：包被移除；账号数据未删；转发返回 **503** |
| 重新安装恢复 | **通过**：账号恢复显示，宿主重新运行，`demo-fast` 转发恢复 **200** |
| 回滚 | **数据库副本已演练验证**；旧二进制端到端重启步骤已记录，未在本 worktree 构建旧二进制 |

## 2. v17 fixture 的构造与覆盖范围

没有使用真实旧库。fixture 的构造方式：

1. 用新二进制在空临时目录启动一次，生成完整 v21 库；
2. 用 Python `sqlite3` 把 v18～v21 的新增内容退回，得到 **fork v17 形态**：
   - 删除 `requests.api_key_id / api_key_name / api_key_source`；
   - 删除 `request_daily.client_stats / api_key_stats / source_stats_version`；
   - 删除 `request_raw.truncated`；
   - 删除 `assistant_images`、`account_limiter_events`；
   - 加回 fork v17 的 `requests.key_id / key_name`；
   - `PRAGMA user_version = 17`；
3. 写入旧版明文数据：
   - `kv.apiKeys`：1 把 Key（`key-upgrade`，`quotaRules=[{limit:1000000, reset:"daily"}]`）；
   - `accounts`：2 条 WorkBuddy 遗留账号（明文 `data`，含 `accessToken` / `refreshToken` / `expiresAt`）；
   - `account_usage_records`：`legacy-workbuddy-1` 余额读数 `4242.5`；
   - `requests`：3 行历史请求，带 fork v17 的 `key_id` / `key_name`；
   - `request_daily`：1 天聚合行；
   - `kv.pluginUpdates`：关闭自动安装官方插件，保证卸载步骤可观察。

这**不是**从真实旧版本安装包生成的原始 v17 库，而是「v21 schema 回退到 v17 形态」的 fixture。
它覆盖了本次迁移最关键的三类内容：**Key 身份列映射（v21 回填）**、**账号数据（含明文→密文迁移）**、
**历史用量/请求/日聚合行**。真实的旧版本二进制差异不在本次覆盖范围内。

备份：

```bash
python3 - <<'PY'
import sqlite3
c = sqlite3.connect('/tmp/agent2api-upgrade-home/agent2api.db')
c.execute('PRAGMA wal_checkpoint(TRUNCATE)')
c.close()
PY
cp -a /tmp/agent2api-upgrade-home/agent2api.db /tmp/agent2api-upgrade-pre/agent2api.db
# 验证副本
python3 -c "import sqlite3;c=sqlite3.connect('/tmp/agent2api-upgrade-pre/agent2api.db');print(c.execute('pragma user_version').fetchone()[0], c.execute('select count(*) from accounts').fetchone()[0])"
# 17 2
```

## 3. v17 → v21 升级

启动命令：

```bash
setsid env \
  AGENT2API_PROXY_HOME=/tmp/agent2api-upgrade-home \
  AGENT2API_HOST=127.0.0.1 \
  AGENT2API_PROXY_PORT=13065 \
  AGENT2API_PROXY_API_KEY=sk-upgrade-test \
  /root/programming/github/agent2api/target/debug/agent2api-server \
  > /tmp/agent2api-upgrade-server.log 2>&1 < /dev/null &
```

### 3.1 schema 版本

```bash
curl -s http://127.0.0.1:13065/api/system-info \
  -H 'authorization: Bearer sk-upgrade-test'
```

关键字段：

```json
{"schemaVersion":21,"dbSchemaVersion":21,"dbFile":"/tmp/agent2api-upgrade-home/agent2api.db"}
```

SQLite 直接核对：

```text
user_version = 21
tables = account_limiter_events, account_usage_records, accounts, assistant_images,
         assistant_sessions, checkin_records, debug_traffic, kv, logs,
         request_daily, request_raw, requests, sqlite_sequence
```

### 3.2 Key / 配额 / 用量保留

`GET /api/keys`：

```json
[{"id":"key-upgrade","name":"Upgrade Key","enabled":true,
  "quotaRules":[{"limit":1000000,"reset":"daily"}],
  "usage":{"requests":2,"successful":2,"tokens":200}}]
```

`GET /api/accounts/usage/snapshot`（旧账号余额记录）：

```json
{"legacy-workbuddy-1":[{"conditions":[{"known":true,"value":4242.5}]}]}
```

数据库核对：

```text
usage rows:
  ('legacy-workbuddy-1', 4242.5, '{"available": 4242.5, "totalLeft": 4242.5}')
  ('legacy-workbuddy-2', NULL, NULL)

requests（v21 回填）:
  legacy-req-1  key_id=key-upgrade  api_key_id=key-upgrade  api_key_source=key
  legacy-req-2  key_id=key-upgrade  api_key_id=key-upgrade  api_key_source=key
  legacy-req-3  key_id=key-other    api_key_id=key-other    api_key_source=key

request_daily:
  ('2026-10-30', 3, 2, 200, source_stats_version=2)

account_limiter_events（v21 补建并回填）:
  legacy:1 / legacy:2 / legacy:3
```

### 3.3 旧账号可读

升级后、安装插件前，`GET /api/accounts` 返回：

```json
{"accounts":[],
 "hiddenAccounts":{"byProvider":{"workbuddy":2},
 "providers":[{"id":"workbuddy","count":2,"reason":"not_installed"}],"total":2}}
```

说明：两条 v17 遗留账号行已完整读入，没有丢失；由于 WorkBuddy 是内置 provider id，
插件未安装时界面按设计将其归入 `hiddenAccounts`（`reason=not_installed`），而不是删除。
安装插件后同一接口返回 2 条完整账号（见 4.2）。

## 4. 插件安装 / 启用 / 卸载 / 恢复

### 4.1 安装并启用

```bash
curl -X POST http://127.0.0.1:13065/api/plugins/install \
  -H 'authorization: Bearer sk-upgrade-test' \
  -H 'content-type: application/zip' \
  --data-binary @/tmp/Agent2Providers/dist/agent2api-providers-0.2.1-x86_64-unknown-linux-gnu.zip

curl -X POST http://127.0.0.1:13065/api/plugins/provider \
  -H 'authorization: Bearer sk-upgrade-test' \
  -H 'content-type: application/json' \
  -d '{"id":"agent2api-providers","provider":"workbuddy","enabled":true}'
```

宿主状态（`GET /api/plugins`）：

```json
{"packs":[{"id":"agent2api-providers","version":"0.2.1","enabled":true,"running":true}],
 "host":{"running":true,"crashes":0,"lastError":null}}
```

注册 provider 数：**33**（含 `demo` 等默认关闭项；`workbuddy` 已启用）。

安装后账号恢复可见：

```json
{"accounts":[
  {"id":"legacy-workbuddy-1","provider":"workbuddy","name":"Legacy WorkBuddy","enabled":true},
  {"id":"legacy-workbuddy-2","provider":"workbuddy","name":"Legacy WorkBuddy Two","enabled":true}],
 "hiddenAccounts":{"total":0}}
```

### 4.2 卸载

```bash
curl -X POST http://127.0.0.1:13065/api/plugins/uninstall \
  -H 'authorization: Bearer sk-upgrade-test' \
  -H 'content-type: application/json' \
  -d '{"id":"agent2api-providers"}'
```

结果：`packs=[]`，插件文件与注册表移除。

卸载后账号状态（两次观察一致）：

```json
{"accounts":[{"id":"demo-e5993cd2e2bc8417","provider":"demo","name":"Demo Legacy","enabled":true}],
 "hiddenAccounts":{"byProvider":{"workbuddy":2},
 "providers":[{"id":"workbuddy","count":2,"reason":"not_installed"}],"total":2}}
```

- 插件专属 provider（`demo`）的账号**仍在 `accounts` 列表中可见**；
- v17 遗留的 `workbuddy` 账号进入 `hiddenAccounts`，数据仍在库中，未删除；
- 这是当前实现的精确行为：`account_visible()` 对内置 provider id 要求插件正在提供该家，
  对非内置 id 则直接可见。brief 的「账号仍显示」对内置家是「数据仍可读、重装后恢复显示」，
  不是卸载期间继续出现在主列表。

转发 503：

```bash
curl -X POST http://127.0.0.1:13065/v1/chat/completions \
  -H 'authorization: Bearer sk-upgrade-test' \
  -H 'content-type: application/json' \
  -d '{"model":"demo-fast","messages":[{"role":"user","content":"ping"}],"stream":false}'
# HTTP 503
# {"error":{"message":"没有可用账号，无账号可转发：请在账号页添加并启用账号","type":"proxy_error"}}
```

补充：点名 `glm-5.3` 时先被模型门禁拦下，返回 **404 `model_not_found`**
（`模型已在网关中关闭`），这是 `pipeline::resolve_model` 在转发前的预期分支；
`demo-fast` / 未知模型 / `auto` 才会进入转发层并得到 **503**。

### 4.3 重新安装恢复

重新安装并启用 `demo` 后：

```json
{"packs":[{"id":"agent2api-providers","enabled":true,"running":true}],
 "host":{"running":true,"crashes":0,"lastError":null}}
```

账号恢复：

```json
{"accounts":[
  {"id":"legacy-workbuddy-1","provider":"workbuddy"},
  {"id":"legacy-workbuddy-2","provider":"workbuddy"},
  {"id":"demo-e5993cd2e2bc8417","provider":"demo"}],
 "hiddenAccounts":{"total":0}}
```

转发恢复（临时 mock 上游 `127.0.0.1:28080`）：

```bash
curl -X POST http://127.0.0.1:13065/v1/chat/completions \
  -H 'authorization: Bearer sk-upgrade-test' \
  -H 'content-type: application/json' \
  -H 'x-demo-upstream: http://127.0.0.1:28080' \
  -d '{"model":"demo-fast","messages":[{"role":"user","content":"ping"}],"stream":false}'
# HTTP 200
```

`GET /v1/models` 同时恢复 `demo-fast` / `demo-slow`。

## 5. 回滚程序

### 5.1 生产/用户环境（按 brief 的 `data.pre-plugin`）

```bash
# 1) 先停服务（headless / systemd / Docker 按实际部署方式）
kill <agent2api-server PID>
#   或：systemctl --user stop agent2api
#   或：docker compose stop

# 2) 恢复升级前库副本（升级前必须已经做过 cp -a data data.pre-plugin）
#    注意：先删掉 WAL/SHM，避免旧库和升级后 WAL 混用
rm -f <config_dir>/agent2api.db-wal <config_dir>/agent2api.db-shm
cp -a data.pre-plugin/agent2api.db <config_dir>/agent2api.db
#   如果 data.pre-plugin 是整个目录：cp -a data.pre-plugin/. <config_dir>/

# 3) 切回旧二进制 / 旧分支
git switch main          # main 的 SCHEMA_VERSION = 17
#   或安装升级前的 release 包

# 4) 重启
AGENT2API_PROXY_HOME=<config_dir> ./agent2api-server
```

关键点：**必须同时恢复数据库**。v14 起旧程序会拒绝打开 schema 高于自身的库
（`schema::migrate` 的拒绝分支），只切二进制而不恢复库会直接启动失败。

### 5.2 本次临时环境演练

```bash
# 停止新网关后
cp -a /tmp/agent2api-upgrade-pre/agent2api.db /tmp/agent2api-rollback-home/agent2api.db
python3 - <<'PY'
import sqlite3
c = sqlite3.connect('/tmp/agent2api-rollback-home/agent2api.db')
print('user_version', c.execute('pragma user_version').fetchone()[0])
print('accounts', c.execute('select id,provider from accounts order by priority').fetchall())
print('requests', c.execute('select count(*) from requests').fetchone()[0])
print('has api_key_id', 'api_key_id' in [r[1] for r in c.execute('pragma table_info(requests)')])
print('has key_id', 'key_id' in [r[1] for r in c.execute('pragma table_info(requests)')])
PY
```

演练结果：

```text
user_version 17
accounts [('legacy-workbuddy-1','workbuddy'), ('legacy-workbuddy-2','workbuddy')]
requests 3
has api_key_id False
has key_id True
```

并确认 `main` 分支的 `SCHEMA_VERSION = 17`，与恢复后的库一致。

**未执行**：在本 worktree 构建/启动 `main` 旧二进制做端到端回滚。原因：本任务只要求记录回滚程序，
且旧二进制不在当前构建产物中；数据库副本恢复与版本兼容性已演练。若需要端到端演练，可在独立
worktree 执行 `git worktree add /tmp/agent2api-main main` 后构建并指向 `/tmp/agent2api-rollback-home`。

## 6. 阻塞 / 未覆盖项

1. **没有真实 v17 安装包生成的原生旧库**：fixture 由 v21 回退构造，覆盖 schema 迁移与数据保留，
   但不覆盖旧版本二进制自身的写入差异。
2. **没有真实上游账号**：转发验证使用 `demo` + 本地 mock 上游；真实 provider 的升级后转发未测
   （与 Task 7.1 parity 报告的账号阻塞一致）。
3. **旧二进制端到端回滚未执行**：见 5.2；步骤与版本兼容性已验证。
4. **卸载期间内置家账号的 UI 可见性**：按当前设计进入 `hiddenAccounts`（`not_installed`），
   数据不丢、重装恢复；若产品要求卸载期间仍在主列表显示，需要改 `account_visible()` 的判定，
   不属于本次验证范围。

## 7. 清理与产物

- 临时网关、mock 上游均已停止；
- 临时目录：`/tmp/agent2api-upgrade-home`、`/tmp/agent2api-upgrade-pre`、`/tmp/agent2api-rollback-home`；
- 日志：`/tmp/agent2api-upgrade-server.log`、`/tmp/agent2api-upgrade-init.log`；
- 仓库 `data/` 未被复制或修改；所有验证均使用 `/tmp` 下的临时数据目录。

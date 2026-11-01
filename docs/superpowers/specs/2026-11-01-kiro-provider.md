# Kiro 上游协议规格（2026-11-01 调研）

> 本文是 `docs/superpowers/plans/2026-11-01-kiro-provider.md` 的依据。
> 所有事实来自三份独立实现交叉验证，来源标注在每节末尾。
> **凡本文没写「已实测」的条目，实现时必须先打一次真接口确认，不得凭推测实现。**

## 0. 一句话

Kiro（AWS CodeWhisperer / Amazon Q Developer）不是 OpenAI 兼容协议：
请求体是自有 JSON，响应是 **AWS Event Stream 二进制帧**，
鉴权走 AWS SSO OIDC 或 `ksk_` API Key。

## 1. 推理端点（按认证方式分派）

| 认证方式                  | URL                                                                      | 额外必备头                                                                                                              |
| ------------------------- | ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| Social / Builder ID / IdC | `https://q.{region}.amazonaws.com/generateAssistantResponse`             | —                                                                                                                       |
| IdC / External IdP        | `https://codewhisperer.{region}.amazonaws.com/generateAssistantResponse` | `x-amz-target: AmazonCodeWhispererStreamingService.GenerateAssistantResponse`、`TokenType: EXTERNAL_IDP`                |
| API Key（`ksk_…`）        | `https://runtime.{region}.kiro.dev/`                                     | `x-amz-target: AmazonCodeWhispererStreamingService.GenerateAssistantResponse`、`TokenType: API_KEY`、`Origin: KIRO_CLI` |

共同必备头：

```
content-type: application/json
accept: application/vnd.amazon.eventstream
authorization: Bearer {accessToken}
x-amzn-kiro-agent-mode: vibe
user-agent: aws-sdk-js/3.738.0 KiroIDE
```

**`user-agent` 是协议常量，不是标识位。** 不点名官方 IDE 的 UA 会被拒，
返回「Your subscription does not support this application」。
来源：`dat-lequoc/dsh-kiro` `src/adapter.ts`、`Quorinex/Kiro-Go` `proxy/kiro_api.go`。

区域改写规则：`codewhisperer` REST 主机只在 `us-east-1` 存在，其它 region
一律走 `q.{region}`。来源：`Kiro-Go` `regionalizeURL`。

## 2. 响应信封：AWS Event Stream

不是 SSE。帧结构：

```
[4B 总长][4B 头长][4B prelude CRC] [headers] [payload] [4B message CRC]
```

- 头：`[1B 名长][名][1B 类型][2B 值长][值]`，Kiro 只发类型 7（UTF-8 string）
- 帧上限 16 MiB；超过视为流错位（不要缓冲等它）
- CRC 不校验（TLS 已保护；坏帧会在 JSON 解析处失败）
- 流在帧中途结束 = 截断，必须报错而不是 flush 半帧

payload 是 JSON，关键事件（已实测字段名）：

| 事件键                           | 含义                                          |
| -------------------------------- | --------------------------------------------- |
| `assistantResponseEvent.content` | 正文增量                                      |
| `toolUseEvent`                   | 工具调用（含 `toolUseId` / `name` / `input`） |
| `messageMetadataEvent`           | 收尾元数据（usage 在这里）                    |
| `metadataEvent`                  | 会话元数据                                    |

来源：`dsh-kiro` `src/eventstream.ts`、`Kiro-Go` `proxy/translator.go`。

## 3. 请求体（`generateAssistantResponse`）

四条硬约束，全部实测：

1. **没有 system 槽位。** 系统提示词拼进第一条 user turn 的内容。
2. **最后一轮 user 是 `currentMessage`，不进 history。**
   `conversationState.history` 必须严格 user/assistant 交替；缺档用中性
   padding 补齐，常量照抄官方客户端：user 用 `"Continue"`，assistant 用
   `"understood"`。**不要**用形似系统消息的标记，模型会模仿并输出到正文。
3. **history 上限 198 条。** 实测 198 过、200 报 `Input is too long.`，
   与 token 数无关。超限时把最老的折成一条文本 turn。
4. **工具结果挂 user turn。** `toolUseId` 必须是 history 里真实发出过的；
   对不上的结果降级成文本。工具名需匹配 `/^[A-Za-z][A-Za-z0-9_]{0,63}$/`。

来源：`dsh-kiro` `src/serialize.ts`（模块头逐条注释了实测结论）。

## 4. 模型目录

```
GET https://q.{region}.amazonaws.com/ListAvailableModels
      ?origin=AI_EDITOR&maxResults=50[&profileArn=…][&nextToken=…]
```

- 分页。**上游每页都返回空 `nextToken`**，必须有防死循环：token 为空 /
  token 已用过 / 本页无新增，三者任一即停。
- 每个模型带 `supportedInputTypes: ["TEXT","IMAGE"]` ——
  **图片能力读字段，不能按模型名猜**（同一账号里 `glm-5` / `minimax-m2.5`
  只支持文本，其余 17 个支持图片）。

profileArn 来源：

```
POST https://q.{region}.amazonaws.com/ListAvailableProfiles
body: {"maxResults": 50[, "nextToken": …]}
```

region 从 ARN 第 4 段（`arn:aws:codewhisperer:{region}:…`）解析。
API Key 账号**不需要** profileArn。

来源：`dsh-kiro` `src/discovery.ts`、`Kiro-Go` `listKiroProfilesInRegionContext`。

## 5. 余额 / 配额

```
GET https://codewhisperer.{region}.amazonaws.com/getUsageLimits
      ?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST&isEmailRequired=true
```

响应四类额度，全部要归一化：

| 字段                   | 含义                                                                  |
| ---------------------- | --------------------------------------------------------------------- |
| `usageBreakdownList[]` | 订阅额度（`usageLimit` / `usageLimitWithPrecision` + `currentUsage`） |
| `freeTrialInfo`        | 新手试用（仅 `status == "ACTIVE"` 时算数）                            |
| 具名 bonus grants      | 赠送额度                                                              |
| `overage`              | 购买的加量包                                                          |

`nextDateReset` / `resetDate` 给重置时刻；`subscriptionInfo.subscriptionTitle` 给套餐名。
**无上限的额度要标 unbounded，不得折算成百分比。**

来源：`dsh-kiro` `src/usage.ts`。

## 6. 认证

| 形态                                        | 获取                                                                                                                                                                                                                                                                                     | 续期                                                             |
| ------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| **API Key** `ksk_…`（可带 `\|region` 后缀） | 用户粘贴                                                                                                                                                                                                                                                                                 | **不续期**                                                       |
| Builder ID（AWS SSO device code）           | `POST https://oidc.{region}.amazonaws.com/client/register`（`clientName:"Kiro"`, `clientType:"public"`, `grantTypes:["urn:ietf:params:oauth:grant-type:device_code","refresh_token"]`, `issuerUrl:"https://view.awsapps.com/start"`）→ `POST /device_authorization` → 轮询 `POST /token` | `POST /token`（`grant_type=refresh_token`）                      |
| Kiro 原生 refresh token                     | 用户粘贴                                                                                                                                                                                                                                                                                 | `POST https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken` |

来源：`Kiro-Go` `auth/builderid.go`、`auth/oidc.go`；`dsh-kiro` `src/auth.ts`。

## 7. 错误分类（映射到网关四档）

| 上游信号                                          | 网关档位                 | 附加动作                                   |
| ------------------------------------------------- | ------------------------ | ------------------------------------------ |
| `429` / 文案含 `quota`                            | `QuotaLimited`           | 冷却 + 换账号                              |
| `402` + 文案含 `overage`                          | `QuotaLimited`           | 区分「加量包耗尽」与「订阅耗尽」，文案不同 |
| `temporarily_suspended` / `account suspended`     | `Fatal`                  | **账号级禁用**（封号，不是限额）           |
| `401` / `403` / `invalid_grant` / `token expired` | `TokenExpired`           | 刷新后同账号重试                           |
| profile ARN 解析失败                              | `QuotaLimited`（短冷却） | **绝不自动禁用**                           |
| history 超 198 条 / `Input is too long.`          | `Fatal`                  | **不罚账号**（是客户端上下文太长）         |

来源：`Kiro-Go` `proxy/account_failover.go`；`dsh-kiro` `src/adapter.ts`。

## 8. README 六列判定

| 列             | 结论             | 依据                                                   |
| -------------- | ---------------- | ------------------------------------------------------ |
| LLM 请求       | ✓                | 三条实现均可流式 + 工具调用                            |
| Token 自动续期 | ✓                | OIDC `/token` 与 `refreshToken` 两条路                 |
| 模型列表       | ✓                | `ListAvailableModels`                                  |
| 余额查询       | ✓                | `getUsageLimits`                                       |
| 签到           | ✗ **待实测确认** | 三份实现都没有签到；`getUsageLimits` 是查询不是签到    |
| 领取类         | ✗ **待实测确认** | 三份实现都没有领取；`freeTrialInfo` / bonus 是被动发放 |

后两列在实现阶段必须自己打一遍接口确认（Skill 1.3），
确认后把「怎么排除的」写进 `providers/kiro/mod.rs` 的模块头。

## 9. 与网关架构的接法

- 网关 `adapter_for()` 只返回 `missing_plugin::placeholder`，**不得**在网关里写实现。
- 插件 `conversation` 自己解码 Event Stream、自己产出 OpenAI Chat 帧，
  报 `UpstreamKind::Chat`（与 `kuku` 同款范式：上游非 SSE，插件负责翻译）。
- `is_stateful = true` → 必须实现 `classify`，否则限额错误按 `Fatal` 透传，
  福利池耗尽后每个请求都从头撞一遍全部账号。

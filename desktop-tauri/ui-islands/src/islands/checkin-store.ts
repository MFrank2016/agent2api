/**
 * 签到中心的数据层：读、写、以及「这一页当前在做什么」的状态。
 *
 * ── 为什么是模块级 store 而不是 React state ────────────────
 * 三个标签页共读同一份快照（`daily` / `auto` / `keepalive` / `travel` 都在
 * `/api/checkin-center` 一条响应里）。放进 React state 会让每个标签页各拉一次，
 * 而且切页就丢。这里是「一次拉全、三个标签页订阅」—— 与账号页
 * `accounts-store.ts` 同一取向。
 *
 * ── 为什么不用 useSyncExternalStore ────────────────────────
 * 页面的重绘粒度是「整页」，数据量小（十几家、几十个账号），订阅后整体重渲
 * 比逐字段选择器简单得多。`useSyncExternalStore` 的价值在细粒度选择，这里
 * 用不上。
 *
 * ── 错误处理 ────────────────────────────────────────────────
 * 所有写动作都走 `run()`：它统一管 `saving` 标记、错误文案、成功后的重拉。
 * 各个按钮不必各写一遍 try/catch —— 那正是旧 iframe 页里 40 多处重复的来源。
 */

import type {
  AutoState, CheckinCenter, CheckinSection, KeepaliveState, OnboardingTask, ProviderGroup, RecordsPage, RecordRow,
  TravelRunResult, TravelState,
} from './checkin-types'
import { t } from '../i18n'
import { DEFAULT_PAGE_SIZE, SERVER_PAGE_SIZES, readPageSize } from './table-shell'
import type { CheckinResultRow } from '../../../ui-shared/checkin'
import { growthTaskRunnable } from '../../../ui-shared/checkin'
import { rowCompleted, rowPending, rowMessage as resultMessage } from '../../../ui-shared/checkin-result'
import { callJson } from './assistant-api'
import { onboardingFromCenter } from './checkin-onboarding-cache'

/** 新手福利的按账号缓存（明细要打上游，查一次存一次） */
export type OnboardingCache = {
  status: 'idle' | 'loading' | 'loaded' | 'error'
  tasks: OnboardingTask[]
  earned: number
  claiming: boolean
  settled: boolean
  settledAt?: number
  checkedAt?: number
  error?: string
}

/**
 * 成长任务的按账号缓存（WorkBuddy 国内版的成长中心）。
 *
 * 快照只给「有哪几个账号能查」，明细（任务树 / 能量 / 连登 / 旅行 / 开学季）
 * 要打一次上游 —— 与新手福利同一取舍（快照必须零上游请求）。
 */
export type GrowthCache = {
  status: 'idle' | 'loading' | 'loaded' | 'error'
  checkedAt?: number
  /** 成长能量（null = 本次没取到） */
  energy: number | null
  /** 连登天数（null = 本次没取到） */
  streak: number | null
  tasks: GrowthTask[]
  travel: unknown
  school: unknown
  running: boolean
  runningSchool: boolean
  /** 逐条执行的结果（按 code 索引，界面在任务行上显示） */
  results: { code: string; outcome?: string; message?: string }[]
  error?: string
}

/** 成长任务树里的一条 */
export type GrowthTask = {
  code: string
  title: string
  description: string
  status: string
  current: number
  target: number
  locked: boolean
  supported: boolean
  /** 无法伪造的任务（需真实操作），界面标「需真实操作」且不自动执行 */
  unforgeable: boolean
  kind: string
  source: string
}

/* ─── 桥 ───────────────────────────────────── */

/** 本页用到的壳侧接口（`bridge.rs` 的签到那一段） */
type CheckinBridge = {
  getCheckinCenter?: () => Promise<CheckinCenter | null | undefined>
  checkinAllAccounts?: (id?: string) => Promise<CheckinRunResult | null | undefined>
  runCheckinActivity?: (id: string, mode: string) => Promise<CheckinRunResult | null | undefined>
  getCheckinRecords?: (params: Record<string, unknown>) => Promise<RecordsPage | null | undefined>
  clearCheckinRecords?: (params: Record<string, unknown>) => Promise<unknown>
  getCheckinSchedules?: () => Promise<AutoState | null | undefined>
  saveCheckinSchedule?: (patch: Record<string, unknown>) => Promise<AutoState | null | undefined>
  deleteCheckinSchedule?: (id: string) => Promise<unknown>
  runCheckinSchedule?: (id: string) => Promise<CheckinRunResult | null | undefined>
  saveAutoCheckin?: (patch: Record<string, unknown>) => Promise<AutoState | null | undefined>
  getCheckinKeepalive?: () => Promise<KeepaliveState | null | undefined>
  saveCheckinKeepalive?: (models: string[], apiKeyId?: string | null) => Promise<KeepaliveState | null | undefined>
  getTravelSchedule?: () => Promise<TravelState | null | undefined>
  saveTravelSchedule?: (patch: Record<string, unknown>) => Promise<TravelState | null | undefined>
  runTravelScheduleNow?: () => Promise<TravelRunResult | null | undefined>
  getOnboardingTasks?: (id: string, refresh?: boolean) => Promise<OnboardingPayload | null | undefined>
  claimOnboardingTasks?: (id: string) => Promise<OnboardingPayload | null | undefined>
  getQoderUmid?: () => Promise<UmidState | null | undefined>
  installQoderUmid?: () => Promise<unknown>
  getGrowthStatus?: (id: string) => Promise<GrowthPayload | null | undefined>
  runGrowthTask?: (id: string, code: string, mode: string) => Promise<GrowthRunRow | null | undefined>
  runGrowthSchool?: (id: string, lottery: boolean) => Promise<{ message?: string } | null | undefined>
  runGrowthTravel?: (id: string) => Promise<{ message?: string } | null | undefined>
}

/** `/api/accounts/{id}/growth` 的返回（任务 / 能量 / 连登 / 旅行 / 开学季五块） */
type GrowthPayload = {
  energy?: number | null
  streak?: number | null
  tasks?: { tasks?: unknown; error?: string }
  travel?: unknown
  school?: unknown
}

/** 单条成长任务的执行结果 */
type GrowthRunRow = { code?: string; outcome?: string; message?: string }

/** 新手任务的返回（查询与领取同形） */
type OnboardingPayload = {
  tasks?: unknown
  earned?: number
  total?: number
  settled?: boolean
  settledAt?: number | null
  failed?: number
  message?: string
}

/** Qoder 国际版的 UMID 组件状态（机器级，与有没有账号无关） */
export type UmidState = {
  available?: boolean
  installing?: boolean
  installSupported?: boolean
}

/** 批量签到返回汇总与 results，活动分档接口直接返回账号行。 */
type CheckinRunResult = CheckinResultRow & {
  succeeded?: number
  active?: number
  total?: number
  skipped?: number
  failedCount?: number
  failed?: string[]
  results?: CheckinResultRow[]
  reason?: string
  state?: AutoState
}

type SharedWindow = {
  workbuddyDesktop?: CheckinBridge
  wbApp?: {
    /** 切页 / 刷新时由壳调用；本页只在自己可见时轮询 */
    readonly currentPage?: string
    toast?: (message: string, tone?: string) => void
    refresh?: () => void
  }
}

function bridge(): CheckinBridge {
  return (window as unknown as SharedWindow).workbuddyDesktop ?? {}
}

/** 给页面层用的桥（`checkin-center-tab` 的惰性查询要用） */
export function bridgeApi(): CheckinBridge {
  return bridge()
}

function app(): SharedWindow['wbApp'] {
  return (window as unknown as SharedWindow).wbApp
}

/** 提示：优先走壳的 toast（与其它页同一套），没有就退回 `console` */
export function toast(message: string, tone: 'ok' | 'err' | 'warn' = 'ok'): void {
  const host = app()
  if (host?.toast) {
    host.toast(message, tone)
    return
  }
  // 壳没提供 toast 时不该静默 —— 用户点了按钮什么都没发生是最糟的反馈
  if (tone === 'err') console.error('[checkin]', message)
  else console.info('[checkin]', message)
}

/** 错误 → 可读文案（桥抛的是 `Error`，但偶尔是字符串 / 对象） */
export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message
  if (typeof error === 'string') return error
  if (error && typeof error === 'object' && 'message' in error) {
    return String((error as { message: unknown }).message)
  }
  return String(error)
}

/* ─── 状态 ─────────────────────────────────── */

export type Store = {
  /** 聚合快照（null = 还没读到） */
  center: CheckinCenter | null
  centerSection: CheckinSection
  loaded: boolean
  /** 首屏 / 刷新的错误（页面顶部错误条） */
  loadError: string
  /** 通用「保存中」：所有写动作共用（一次只该有一个在跑） */
  saving: boolean
  /** 正在执行的定时条目 id */
  runningScheduleId: string | null
  /** 展开编辑的条目 id；`'new'` = 新建表单 */
  editingId: string | null
  /** 单账号动作在途：`<accountId>:<mode>` */
  signing: Set<string>
  /** 正在跑的整批签到 */
  runningAll: boolean
  /** 保活模型链草稿（null = 跟随快照） */
  keepaliveDraft: string[] | null
  /** 猫猫旅行时刻草稿（null = 跟随快照） */
  travelTimesDraft: string | null
  /** LobsterAI 保活时刻草稿（null = 跟随后端值） */
  lobsteraiTimeDraft: string | null
  /** 签到记录（独立于聚合快照：它有分页与筛选） */
  records: RecordRow[]
  recordsTotal: number
  recordsLoaded: boolean
  recordsError: string
  recordsLoading: boolean
  recordsQuery: {
    limit: number
    offset: number
    outcome: string
    /** 签到类型：'' = 全部，manual / schedule / makeup */
    kind: string
    provider: string
    keyword: string
    /** 时间下界（毫秒；undefined = 不限） */
    start?: number
  }
  /** 新手福利的任务缓存：账号 id → 缓存（切标签页不丢） */
  onboarding: Map<string, OnboardingCache>
  onboardingBatch: string[] | null
  plansBatch: 'query' | 'claim' | null
  /** 成长任务的缓存：账号 id → 缓存 */
  growth: Map<string, GrowthCache>
  growthBatch: { mode: 'full' | 'claim'; accountIds: string[] } | null
  /** 派猫猫旅行的按账号缓存：账号 id → 缓存（展开时惰性查询） */
  travel: Map<string, TravelCache>
}

const recordsPageSize = readPageSize('checkin-records', SERVER_PAGE_SIZES)

export const store: Store = {
  center: null,
  centerSection: 'daily',
  loaded: false,
  loadError: '',
  saving: false,
  runningScheduleId: null,
  editingId: null,
  signing: new Set(),
  runningAll: false,
  keepaliveDraft: null,
  travelTimesDraft: null,
  lobsteraiTimeDraft: null,
  records: [],
  recordsTotal: 0,
  recordsLoaded: false,
  recordsError: '',
  recordsLoading: false,
  recordsQuery: {
    limit: typeof recordsPageSize === 'number' ? recordsPageSize : DEFAULT_PAGE_SIZE,
    offset: 0, outcome: '', kind: '', provider: '', keyword: '',
  },
  onboarding: new Map(),
  onboardingBatch: null,
  plansBatch: null,
  growth: new Map(),
  growthBatch: null,
  travel: new Map(),
}

const listeners = new Set<() => void>()

export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

/** 改 store 并通知（所有写路径都走这里） */
export function patch(next: Partial<Store>): void {
  Object.assign(store, next)
  for (const notify of listeners) notify()
}

/* ─── 读 ───────────────────────────────────── */

let centerRequest = 0

/** 拉聚合快照（页面打开 / 任何写动作之后） */
export async function loadCenter(): Promise<void> {
  const request = ++centerRequest
  try {
    const data = await bridge().getCheckinCenter?.()
    if (request !== centerRequest) return
    if (!data) {
      patch({ loaded: true, loadError: '后端未返回签到中心数据' })
      return
    }
    patch({
      center: data,
      onboarding: onboardingFromCenter(data, store.onboarding),
      loaded: true,
      loadError: '',
      // 快照到手 = 草稿该跟着它走（写动作成功后也会清，两处不冲突）
      keepaliveDraft: null,
      travelTimesDraft: null,
    })
  } catch (error) {
    if (request === centerRequest) patch({ loaded: true, loadError: errorMessage(error) })
  }
}

let recordsRequest = 0

/** 拉签到记录（分页 + 筛选）。 */
export async function loadRecords(): Promise<void> {
  const request = ++recordsRequest
  const query = store.recordsQuery
  patch({ recordsLoading: true })
  try {
    const data = await bridge().getCheckinRecords?.({
      limit: query.limit,
      offset: query.offset,
      outcome: query.outcome || undefined,
      kind: query.kind || undefined,
      provider: query.provider || undefined,
      keyword: query.keyword || undefined,
      start: query.start,
    })
    if (request !== recordsRequest) return
    const total = Math.max(0, Number(data?.total) || 0)
    const lastOffset = Math.max(0, (Math.ceil(total / query.limit) - 1) * query.limit)
    if (query.offset > lastOffset) {
      patch({ recordsQuery: { ...query, offset: lastOffset } })
      await loadRecords()
      return
    }
    patch({
      records: Array.isArray(data?.records) ? data.records : [],
      recordsTotal: total,
      recordsLoaded: true,
      recordsError: '',
    })
  } catch (error) {
    if (request === recordsRequest) patch({ recordsLoaded: true, recordsError: errorMessage(error) })
  } finally {
    if (request === recordsRequest) patch({ recordsLoading: false })
  }
}

/* ─── 写 ───────────────────────────────────── */

/**
 * 写动作的统一壳。
 *
 * `run` 负责三件事：置 `saving`、把异常变成一条提示、成功后重拉快照。
 * 调用方只写「调哪个接口」，不再各写一遍 try/catch/finally —— 旧 iframe 页
 * 里 40 多处重复的正是这三件事，且漏写 `finally` 就会永久卡在 saving。
 */
async function run<T>(
  action: () => Promise<T>,
  options: { success?: string; reload?: boolean; failure?: string } = {},
): Promise<T | undefined> {
  patch({ saving: true })
  try {
    const result = await action()
    if (options.reload !== false) await loadCenter()
    if (options.success) toast(options.success)
    return result
  } catch (error) {
    toast(`${options.failure ?? '操作失败'}：${errorMessage(error)}`, 'err')
    return undefined
  } finally {
    patch({ saving: false })
  }
}

/**
 * 签到成功之后的跟进钩子：自动查询 / 补领这些账号的新手福利。
 *
 * 由 `checkin-onboarding` 在加载时注册（见那个模块的 `setAfterSignHook`）——
 * 它依赖本模块的 store，静态反向 import 会成环，这与 `assistant-api` 抽出直连
 * 请求层是同一类理由。没注册时静默跳过：签到本身不依赖这一步。
 */
let afterSignHook: ((ids: string[]) => void) | null = null

export function setAfterSignHook(hook: (ids: string[]) => void): void {
  afterSignHook = hook
}

/**
 * 快照的新手福利清单里，与这些账号 id 相交的那部分（`null` = 清单全部）。
 *
 * 清单外的家在这里被滤掉 —— 那是「发一次注定没有结果的查询」的来源。
 */
function onboardingIds(candidates: string[] | null): string[] {
  const rows = store.center?.extras.onboarding ?? []
  if (!candidates) return rows.map(row => row.id)
  const wanted = new Set(candidates)
  return rows.filter(row => wanted.has(row.id)).map(row => row.id)
}

/** 全部签到（`id` 为空 = 签全部可签账号） */
export async function runAllCheckin(accountId?: string): Promise<void> {
  if (store.runningAll) return
  patch({ runningAll: true })
  let signed = false
  try {
    const result = await bridge().checkinAllAccounts?.(accountId)
    reportRunResult(result)
    signed = result !== null && result !== undefined
  } catch (error) {
    toast(`签到失败：${errorMessage(error)}`, 'err')
  } finally {
    patch({ runningAll: false })
    await loadCenter()
  }
  // 签到成功后自动跟进新手福利（失败不触发；见 `checkin-onboarding` 的说明）
  if (signed) afterSignHook?.(onboardingIds(accountId ? [accountId] : null))
}

/**
 * 单账号动作（`mode` = 插件申报的档位）。
 *
 * `silent` 给「整组签到」的循环用：那时逐账号弹一条提示会把屏幕刷满、逐账号各查
 * 一次新手福利也会与后面几个账号的签到请求叠在一起 —— 两件事都交给调用方在末尾
 * 统一收尾。返回值同样给调用方做汇总。
 */
export async function signSingle(
  accountId: string, mode: string, silent = false,
): Promise<CheckinRunResult | null | undefined> {
  const key = `${accountId}:${mode}`
  if (store.signing.has(key)) return undefined
  const next = new Set(store.signing)
  next.add(key)
  patch({ signing: next })
  let result: CheckinRunResult | null | undefined
  try {
    result = mode === 'checkin'
      ? await bridge().checkinAllAccounts?.(accountId)
      : await bridge().runCheckinActivity?.(accountId, mode)
    if (!silent) reportRunResult(result, true)
  } catch (error) {
    if (!silent) toast(`签到失败：${errorMessage(error)}`, 'err')
  } finally {
    const after = new Set(store.signing)
    after.delete(key)
    patch({ signing: after })
    await loadCenter()
  }
  // 签到成功后自动跟进这个账号的新手福利（失败不触发；整组签到由调用方统一收尾）
  if (!silent && result !== null && result !== undefined) afterSignHook?.(onboardingIds([accountId]))
  return result
}

/**
 * 按提供商整组签到（每日签到右栏选中某一家时的动作）。
 *
 * ── 为什么是逐个账号串行 ────────────────────────────────────
 * 端点只有「单账号」与「全部账号」两档，没有「按提供商」这一档 —— 所以复用单账号
 * 签到动作逐个串行：串行与批量签到是同一条防风控口径（多账号同时打上游会触发
 * 风控），结果按账号粒度聚合，一次点击只播报一句（逐账号各弹一条会把屏幕刷满）。
 *
 * 在途键是 `group:${id}`：与单账号的 `${id}:${mode}` 共用同一份 `signing`，前缀
 * 区分、互不牵连（账号 id 与 `group:` 不会撞）。单个账号的在途标记照旧生效，所以
 * 整组执行时行上的按钮也一起禁用。
 */
export async function signProviderGroup(group: ProviderGroup): Promise<void> {
  const key = `group:${group.id}`
  if (store.signing.has(key) || group.accounts.length === 0) return
  const next = new Set(store.signing)
  next.add(key)
  patch({ signing: next })
  const total = group.accounts.length
  let done = 0
  let failed = 0
  try {
    for (const account of group.accounts) {
      const result = await signSingle(account.id, 'checkin', true)
      const row: CheckinResultRow | null = result?.results?.[0] ?? result ?? null
      // 与后端 run_checkin 同一口径：真领取与「今日已领取」都算今天已完成
      // （判据复用共享的 `rowCompleted`，不在这里另立一套）
      if (row && rowCompleted(row)) done += 1
      else failed += 1
    }
    toast(failed > 0
      ? t('{label} 签到完成：{done}/{total} 个账号已签到，{failed} 个失败', {
        label: group.label, done, total, failed,
      })
      : t('✅ {label} 签到完成：{done}/{total} 个账号已签到', { label: group.label, done, total }),
    failed > 0 ? 'err' : 'ok')
  } finally {
    const after = new Set(store.signing)
    after.delete(key)
    patch({ signing: after })
  }
  // 整组只跟进一次新手福利（与逐账号签到的收尾同一口径；一个都没签上就不查）
  if (done > 0) afterSignHook?.(onboardingIds(group.accounts.map(account => account.id)))
}

function rowMessage(row: CheckinResultRow): string {
  return resultMessage(row) || t('未完成，请查看签到记录')
}

/** 提示以逐账号执行结果为准，HTTP 成功仅表示整轮执行完毕。 */
function reportRunResult(result: CheckinRunResult | null | undefined, single = false): void {
  if (!result) return
  const rows = Array.isArray(result.results) ? result.results
    : result.claim || result.activity || result.error ? [result] : []
  if (single && rows.length === 1) {
    const row = rows[0]
    const completed = rowCompleted(row)
    const message = completed ? row.claim?.msg?.trim() || result.reason || '签到完成' : rowMessage(row)
    toast(completed ? `✅ ${message}` : message, completed ? 'ok' : rowPending(row) ? 'warn' : 'err')
    return
  }
  const succeeded = Number(result.succeeded) || 0
  const active = Number(result.active) || 0
  const total = Number(result.total) || rows.length
  const failedRows = rows.filter(row => !rowCompleted(row) && !rowPending(row))
  const failed = result.failedCount ?? result.failed?.length ?? failedRows.length
  const activeText = active > 0 ? `，日活保活 ${active} 个` : ''
  if (failed > 0) {
    const failures = result.failed?.length ? result.failed : failedRows.map(rowMessage)
    const names = failures.length ? `（${failures.slice(0, 5).join('、')}）` : ''
    toast(`签到完成：${succeeded}/${total} 成功${activeText}，${failed} 个失败${names}`, 'err')
    return
  }
  if (single) {
    const completed = succeeded > 0 || active > 0
    toast(completed ? `✅ ${result.reason || '签到完成'}` : t('未完成，请查看签到记录'), completed ? 'ok' : 'warn')
    return
  }
  toast(`✅ 签到完成：${succeeded}/${total} 个账号成功领取${activeText}`, succeeded > 0 || active > 0 ? 'ok' : 'warn')
}

/** 定时签到总开关 */
export async function toggleMaster(enabled: boolean): Promise<void> {
  await run(
    async () => {
      const data = await bridge().saveAutoCheckin?.({ enabled })
      if (data) patch({ center: withAuto(store.center, data) })
    },
    { success: enabled ? '✅ 定时签到已开启' : '定时签到已关闭', reload: false, failure: '保存失败' },
  )
  void loadCenter()
}

/** 保存一条定时条目（`closeEditor: false` = 行上的快捷开关，不收表单不弹提示） */
export async function saveScheduleEntry(
  payload: Record<string, unknown>,
  options: { closeEditor?: boolean; silent?: boolean } = {},
): Promise<boolean> {
  const saved = await run(
    async () => {
      const data = await bridge().saveCheckinSchedule?.(payload)
      if (!data) throw new Error(t('后端未返回定时签到数据'))
      patch({
        center: withAuto(store.center, data),
        ...(options.closeEditor === false ? {} : { editingId: null }),
      })
      return true
    },
    { success: options.silent ? undefined : '✅ 已保存', reload: false, failure: '保存失败' },
  )
  if (saved) void loadCenter()
  return saved === true
}

/** 删除一条定时条目 */
export async function removeScheduleEntry(id: string): Promise<void> {
  await run(
    async () => {
      await bridge().deleteCheckinSchedule?.(id)
    },
    { success: '已删除', failure: '删除失败' },
  )
}

/** 立即执行一条定时条目 */
export async function runScheduleEntry(id: string): Promise<void> {
  if (store.runningScheduleId) return
  patch({ runningScheduleId: id })
  try {
    const result = await bridge().runCheckinSchedule?.(id)
    reportRunResult(result)
  } catch (error) {
    toast(`执行失败：${errorMessage(error)}`, 'err')
  } finally {
    patch({ runningScheduleId: null })
    await loadCenter()
  }
}

/** 保活模型链 */
export async function saveKeepaliveModels(models: string[], apiKeyId?: string | null): Promise<void> {
  await run(
    async () => {
      const data = await bridge().saveCheckinKeepalive?.(models, apiKeyId)
      if (data && store.center) {
        patch({ center: { ...store.center, keepalive: data }, keepaliveDraft: null })
      }
    },
    { success: '✅ 模型链已保存', reload: false, failure: '保存失败' },
  )
}

/** 猫猫旅行排程（开关 / 时刻） */
export async function saveTravel(body: Record<string, unknown>): Promise<void> {
  await run(
    async () => {
      const data = await bridge().saveTravelSchedule?.(body)
      if (data && store.center) {
        patch({
          center: { ...store.center, travel: data },
          ...(body.times ? { travelTimesDraft: null } : {}),
        })
      }
    },
    { success: '✅ 已保存', reload: false, failure: '保存失败' },
  )
}

/**
 * LobsterAI 令牌保活（有道龙虾）：开关 / 保活时刻。
 *
 * 与旅行同构：独立于多条目定时签到的一条链路，走同一条 `POST /api/auto-checkin`
 * （只带 `lobsteraiKeepalive` 子对象，其它字段不动）。
 */
export async function saveLobsteraiKeepalive(body: { enabled?: boolean; time?: string }): Promise<void> {
  await run(
    async () => {
      const data = await bridge().saveAutoCheckin?.({ lobsteraiKeepalive: body })
      if (data) patch({ center: withAuto(store.center, data), lobsteraiTimeDraft: null })
    },
    { success: '✅ 已保存', reload: false, failure: '保存失败' },
  )
}

/** 猫猫旅行：立即巡检全部 */
export async function runTravelNow(): Promise<void> {
  await run(
    async () => {
      const result = await bridge().runTravelScheduleNow?.()
      if (!result) throw new Error(t('后端未返回巡检结果'))
      if (result.outcome === 'skipped') {
        toast(t('本轮已跳过：{reason}', { reason: result.message ?? '' }))
        return
      }
      const failed = Number(result.summary?.failed) || 0
      toast(t('猫猫旅行：{n} 个动作，+{credit} 积分，{failed} 个失败', {
        n: result.summary?.actions?.length ?? 0,
        credit: Number(result.summary?.credit) || 0,
        failed,
      }), failed > 0 ? 'err' : 'ok')
    },
    { failure: '执行失败' },
  )
}

/** 清空签到记录（`all` 为真 = 全清，否则按当前筛选） */
export async function clearRecords(all: boolean): Promise<void> {
  const query = store.recordsQuery
  await run(
    async () => {
      await bridge().clearCheckinRecords?.(all
        ? { all: '1' }
        : {
          outcome: query.outcome || undefined,
          kind: query.kind || undefined,
          provider: query.provider || undefined,
          keyword: query.keyword || undefined,
        })
    },
    { success: '签到记录已清空', reload: false, failure: '清空失败' },
  )
  await loadRecords()
}

/* ─── 成长任务（WorkBuddy 国内版）───────────── */

/** 读一个账号的成长缓存（没有就给空态） */
export function growthOf(id: string): GrowthCache {
  return store.growth.get(id) ?? {
    status: 'idle',
    energy: null,
    streak: null,
    tasks: [],
    travel: null,
    school: null,
    running: false,
    runningSchool: false,
    results: [],
    error: '',
  }
}

function patchGrowth(id: string, next: Partial<GrowthCache>): void {
  const map = new Map(store.growth)
  map.set(id, { ...growthOf(id), ...next })
  patch({ growth: map })
}

/** 任务树里的一条归一化（字段缺失逐个兜底，与旧实现同一口径） */
function normalizeGrowthTask(raw: unknown): GrowthTask | null {
  const row = (raw ?? {}) as Record<string, unknown>
  const code = typeof row.code === 'string' ? row.code : ''
  if (!code) return null
  return {
    code,
    title: typeof row.title === 'string' && row.title ? row.title : code,
    description: typeof row.description === 'string' ? row.description : '',
    status: typeof row.status === 'string' ? row.status : '',
    current: Number(row.current) || 0,
    target: Number(row.target) || 0,
    locked: row.locked === true,
    supported: row.supported === true,
    unforgeable: row.unforgeable === true,
    kind: typeof row.kind === 'string' ? row.kind : '',
    source: typeof row.source === 'string' ? row.source : '',
  }
}

/** 查询一个账号的成长任务盘点（只读，打一次上游） */
export async function queryGrowth(id: string): Promise<void> {
  const current = growthOf(id)
  if (current.status === 'loading' || current.running || current.runningSchool) return
  patchGrowth(id, { status: 'loading', error: '' })
  try {
    const data = await bridge().getGrowthStatus?.(id)
    const tasksBlock = data?.tasks
    patchGrowth(id, {
      status: 'loaded',
      checkedAt: Date.now(),
      energy: data?.energy === null || data?.energy === undefined ? null : Number(data.energy),
      streak: data?.streak === null || data?.streak === undefined ? null : Number(data.streak),
      tasks: (Array.isArray(tasksBlock?.tasks) ? tasksBlock.tasks : [])
        .map(normalizeGrowthTask)
        .filter((task): task is GrowthTask => task !== null),
      travel: data?.travel ?? null,
      school: data?.school ?? null,
      // 任务列表本身的失败挂在行级文案上（其余几块有数据可看，不该整行变红）
      error: typeof tasksBlock?.error === 'string' ? tasksBlock.error : '',
    })
  } catch (error) {
    patchGrowth(id, { status: 'error', error: errorMessage(error) })
  }
}

/**
 * 执行成长任务（`full` = 点亮 + 领奖；`claim` = 只领已完成奖励）。
 *
 * ── 为什么逐条 await 单任务端点 ────────────────────────────
 * 全量一次请求会超过管理 API 的 60 秒转发超时：后端逐条之间留 1s+ 的写间隔
 * （见网关 `api::growth` 的模块头）。所以这里在本地循环，每做完一条就把结果
 * 推给界面 —— 用户能看到逐条进展，而不是一个转很久的「执行中」。
 */
export async function runGrowth(id: string, mode: 'full' | 'claim'): Promise<void> {
  const current = growthOf(id)
  if (current.status !== 'loaded' || current.running || current.runningSchool) return
  const targets = current.tasks.filter(task => growthTaskRunnable(task, mode))
  if (targets.length === 0) {
    toast('没有需要执行的任务')
    return
  }
  patchGrowth(id, { running: true, results: [], error: '' })
  let failed = 0
  const results: GrowthCache['results'] = []
  for (const task of targets) {
    try {
      const row = await bridge().runGrowthTask?.(id, task.code, mode)
      results.push({
        code: task.code,
        outcome: row?.outcome ?? 'done',
        message: row?.message ?? '',
      })
      if (row?.outcome === 'failed') failed += 1
    } catch (error) {
      failed += 1
      results.push({ code: task.code, outcome: 'failed', message: errorMessage(error) })
    }
    patchGrowth(id, { results: [...results] })
  }
  patchGrowth(id, { running: false })
  toast(failed > 0 ? `执行完成，${failed} 项失败` : '✅ 执行完成', failed > 0 ? 'err' : 'ok')
  await queryGrowth(id)
}

export async function runGrowthBatch(accountIds: string[], mode: 'full' | 'claim'): Promise<void> {
  if (store.growthBatch) return
  const ids = [...new Set(accountIds)].filter(id => {
    const cache = growthOf(id)
    return cache.status !== 'loading' && !cache.running && !cache.runningSchool
  })
  if (ids.length === 0) return
  patch({ growthBatch: { mode, accountIds: ids } })
  try {
    for (const id of ids) {
      await queryGrowth(id)
      const cache = growthOf(id)
      if (cache.status !== 'loaded' || cache.error) {
        toast(t('查询失败：{error}', { error: cache.error || id }), 'err')
        continue
      }
      await runGrowth(id, mode)
    }
  } catch (error) {
    toast(t('成长任务执行失败：{error}', { error: errorMessage(error) }), 'err')
  } finally {
    patch({ growthBatch: null })
  }
}

/** 开学季任务 + 抽奖 */
export async function runSchool(id: string, lottery: boolean): Promise<void> {
  const current = growthOf(id)
  if (current.runningSchool) return
  patchGrowth(id, { runningSchool: true })
  try {
    const data = await bridge().runGrowthSchool?.(id, lottery)
    const message = typeof data?.message === 'string' && data.message
      ? data.message
      : (lottery ? '✅ 开学季任务已执行并抽奖' : '✅ 开学季任务已执行')
    toast(message)
  } catch (error) {
    toast(`执行失败：${errorMessage(error)}`, 'err')
  } finally {
    patchGrowth(id, { runningSchool: false })
    await queryGrowth(id)
  }
}

/** 猫猫旅行：跑一趟（单账号） */
export async function runTravelOnce(id: string): Promise<void> {
  try {
    const data = await bridge().runGrowthTravel?.(id)
    toast(typeof data?.message === 'string' && data.message ? data.message : '✅ 旅行巡检完成')
  } catch (error) {
    toast(`执行失败：${errorMessage(error)}`, 'err')
  } finally {
    await queryGrowth(id)
  }
}

/* ─── 派猫猫旅行（WorkBuddy 国内版）─────────── */

/**
 * `GET /api/accounts/{id}/travel` 的 `status` 块（字段可能缺，读时逐个兜底）。
 *
 * 旅行状态刻意不进 `checkin-center` 快照（那必须「零上游请求」，见模块头），
 * 所以卡片展开时才惰性拉一次。
 */
export type TravelStatus = {
  available: boolean
  /** idle | traveling | arrived（无活动时为空串） */
  state: string
  rewardCredit: number
  dailyLimitReached: boolean
  /** 到达时刻（秒）；traveling 时 `arriveAt - serverNow` 即剩余秒数 */
  arriveAt: number
  /** 上游的当前时刻（秒）—— 倒计时的基准，不拿本机时钟直接减 arriveAt */
  serverNow: number
}

/** 一个账号的旅行缓存（没拉过时给一份空壳） */
export type TravelCache = {
  loading: boolean
  status: TravelStatus | null
  /** 同响应里的旅行配置：`enabled` 是总开关，关着时卡片只读、不给按钮 */
  auto: { enabled?: boolean } | null
  error?: string
}

/** `GET /api/accounts/{id}/travel` 的返回体（`callJson` 已剥掉 `data` 信封） */
type TravelPayload = { status?: TravelStatus | null; auto?: TravelCache['auto'] }

/** 读一个账号的旅行缓存（没拉过时给空壳） */
export function travelOf(id: string): TravelCache {
  return store.travel.get(id) ?? { loading: false, status: null, auto: null }
}

function patchTravel(id: string, next: Partial<TravelCache>): void {
  const map = new Map(store.travel)
  map.set(id, { ...travelOf(id), ...next })
  patch({ travel: map })
}

/**
 * 拉一次旅行状态（卡片展开时惰性调用；只读，零写）。
 *
 * 走 `callJson` 而不是裸 `fetch`：桌面端的页面来自 `tauri://` 协议，裸 fetch
 * 打的是 tauri asset，根本到不了后端（见 `assistant-api` 的模块头）。
 */
export async function fetchTravel(id: string): Promise<void> {
  patchTravel(id, { loading: true })
  try {
    const data = await callJson<TravelPayload>('GET', `/api/accounts/${encodeURIComponent(id)}/travel`)
    patchTravel(id, { loading: false, status: data?.status ?? null, auto: data?.auto ?? null, error: '' })
  } catch (error) {
    patchTravel(id, { loading: false, error: errorMessage(error) })
  }
}

/** 手动跑一次 travel_auto（派发 / 领取二合一，上游幂等），跑完重拉状态 */
export async function runTravel(id: string): Promise<void> {
  patchTravel(id, { loading: true })
  try {
    await callJson<unknown>('POST', `/api/accounts/${encodeURIComponent(id)}/travel`)
  } catch (error) {
    toast(`旅行失败：${errorMessage(error)}`, 'err')
  } finally {
    await fetchTravel(id)
  }
}

/* ─── 局部替换 ─────────────────────────────── */

/**
 * 把一份新的 `AutoState` 并进快照。
 *
 * 写接口返回的就是「整份定时状态」，直接覆盖比再拉一次 `checkin-center`
 * 快一整个往返，而且不会让界面在中间闪一下旧值。
 */
function withAuto(center: CheckinCenter | null, auto: AutoState): CheckinCenter | null {
  if (!center) return center
  return { ...center, auto }
}

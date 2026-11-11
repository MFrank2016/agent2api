import { t } from '../i18n'
import {
  bridgeApi, errorMessage, loadCenter, patch, setAfterSignHook, store, toast, type OnboardingCache,
} from './checkin-store'
import { currentOnboardingCache, normalizeOnboardingTasks } from './checkin-onboarding-cache'

export function onboardingOf(id: string): OnboardingCache {
  const provider = store.center?.extras.onboarding.find(row => row.id === id)?.provider
  return currentOnboardingCache(provider, store.onboarding.get(id)) ?? {
    status: 'idle', tasks: [], earned: 0, claiming: false, settled: false,
  }
}

function patchOnboarding(id: string, next: Partial<OnboardingCache>): void {
  const map = new Map(store.onboarding)
  map.set(id, { ...onboardingOf(id), ...next })
  patch({ onboarding: map })
}

/** 手动查询可强制刷新；批量查询跳过已结清的账号。 */
export async function queryOnboarding(id: string, refresh = false): Promise<void> {
  const cached = onboardingOf(id)
  if (cached.status === 'loading' || cached.claiming) return
  if (!refresh && cached.settled) return
  patchOnboarding(id, { status: 'loading', error: '' })
  try {
    const data = await bridgeApi().getOnboardingTasks?.(id, refresh)
    if (!data) throw new Error(t('后端未返回新手福利数据'))
    patchOnboarding(id, {
      status: 'loaded',
      tasks: normalizeOnboardingTasks(data.tasks),
      earned: Number(data.earned) || 0,
      settled: data.settled === true,
      settledAt: Number(data.settledAt) || undefined,
      checkedAt: Date.now(),
    })
    await loadCenter()
  } catch (error) {
    patchOnboarding(id, { status: 'error', error: errorMessage(error) })
  }
}

export function onboardingClaimable(id: string): boolean {
  const cache = onboardingOf(id)
  if (cache.status !== 'loaded' || cache.claiming || cache.settled) return false
  const provider = store.center?.extras.onboarding.find(row => row.id === id)?.provider
  return provider === 'officeace' || cache.tasks.some(task => !task.done && !task.blocked)
}

export async function claimOnboardingBatch(accountIds: string[]): Promise<void> {
  if (store.onboardingBatch) return
  const ids = [...new Set(accountIds)].filter(onboardingClaimable)
  if (ids.length === 0) return
  patch({ onboardingBatch: ids })
  try {
    for (const id of ids) {
      if (onboardingClaimable(id)) await claimOnboarding(id)
    }
  } finally {
    patch({ onboardingBatch: null })
  }
}

/**
 * 一键领取（服务端幂等，重复点不会重复加分）。
 *
 * `silent` 只给签到后的自动补领用：那时由签到结果自己播报，这里的成功 / 失败都
 * 不弹提示（失败仍然落在这个账号的行上，见 `autoProcessOnboarding`）。
 */
export async function claimOnboarding(id: string, silent = false): Promise<void> {
  const cached = onboardingOf(id)
  if (cached.status !== 'loaded' || cached.claiming || cached.settled) return
  patchOnboarding(id, { claiming: true, error: '' })
  try {
    const data = await bridgeApi().claimOnboardingTasks?.(id)
    if (!data) throw new Error(t('后端未返回新手福利数据'))
    const failed = Number(data.failed) || 0
    patchOnboarding(id, {
      claiming: false,
      status: 'loaded',
      tasks: Array.isArray(data.tasks) ? normalizeOnboardingTasks(data.tasks) : cached.tasks,
      earned: Number.isFinite(data.earned) ? Number(data.earned) : cached.earned,
      settled: data.settled === true,
      settledAt: Number(data.settledAt) || undefined,
      checkedAt: Date.now(),
    })
    await loadCenter()
    if (silent) return
    const message = typeof data.message === 'string' ? data.message.trim() : ''
    toast(message || (failed > 0
      ? t('新手福利领取失败 {n} 项', { n: failed })
      : t('✅ 新手福利领取完成')), failed > 0 ? 'err' : 'ok')
  } catch (error) {
    patchOnboarding(id, { claiming: false, status: 'error', error: errorMessage(error) })
    if (!silent) toast(t('领取失败：{error}', { error: errorMessage(error) }), 'err')
  }
}

/**
 * 签到成功之后的自动跟进：查询这些账号的新手福利，有可领取项就领取。
 *
 * ── 为什么值得自动化 ────────────────────────────────────────
 * 一次性新手福利大多「先做任务、再回来领」；用户点完签到就走，那一笔就一直躺在
 * 上游没人领。所以签到成功之后顺手查一次、能领就领 —— 与旧版同一取舍，没有开关。
 *
 * ── 只处理刚签的账号 ────────────────────────────────────────
 * 传进来的就是刚签成功的账号 id（不在新手福利清单里的家传进来是空数组）。快照带
 * 结算记忆、或缓存里 `settled` 已置位的账号不发查询（`queryOnboarding` 自己会
 * 跳过）—— 这正是本功能要消灭的那次多余上游请求；真出了新一期活动，用户手点
 * 「查询任务」时会实查发现。
 *
 * ── 失败不盖住签到结果 ──────────────────────────────────────
 * 查询与领取都走静默路径：失败只落在这个账号的行上（`status: 'error'`），不弹
 * 提示 —— 后台跟进不该盖住刚播报的签到结果。串行执行：与其它多账号动作同一口径
 * （同时打上游会触发风控）。
 */
export async function autoProcessOnboarding(ids: string[]): Promise<void> {
  const targets = [...new Set(ids)]
  for (const id of targets) await queryOnboarding(id)
  for (const id of targets) {
    if (onboardingClaimable(id)) await claimOnboarding(id, true)
  }
}

/** 加载即把跟进钩子挂到签到动作上（见 `checkin-store` 的 `setAfterSignHook`） */
setAfterSignHook(ids => { void autoProcessOnboarding(ids) })

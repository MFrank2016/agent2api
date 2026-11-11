import * as React from 'react'
import { Badge, Button, Spinner } from '@ui'
import { fetchTravel, runTravel, signSingle, store, travelOf } from './checkin-store'
import { MODE_LABEL, MODE_TITLE, formatShort, modesOf, type CheckinAccount, type ProviderGroup } from './checkin-types'
import { ProviderLogo } from './checkin-row'
import { DetailLine, StatusBadge } from './checkin-shared'
import { t } from '../i18n'
import { accountCheckinStatus, checkinActionLabel, checkinPlanProgress } from '../../../ui-shared/checkin-result'

/**
 * 「签到中心 → 每日签到」的账号明细（右栏主体）。
 *
 * ── 为什么不再是「每家一行、点开展开」 ─────────────────────
 * 提供商分类挪到左栏之后，右栏直接列账号：左栏选「全部」时按家分组显示（每组
 * 一个小标题），选某一家时只列这一家的账号。少一层展开，一屏能多看几个账号。
 *
 * ── 一行只放这个账号自己的差异 ─────────────────────────────
 * 状态、上次签到、收益与操作都在行上；这一家的通用规则由右栏标题下的说明条
 * 承担（见 CenterTab）。
 */

export function AccountLine({ account, group }: {
  account: CheckinAccount
  group: ProviderGroup
}): React.ReactElement {
  const busy = (mode: string) => store.signing.has(`${account.id}:${mode}`)
  const status = accountCheckinStatus(account)
  const daily = account.dailyCheckin
  const modes = modesOf(group)
  // 分档执行由插件申报：keepalive / claim / full 各一颗按钮 —— 领取才加积分、
  // 保活只维持活跃，分开是刻意的
  const dailyActivity = modes.some(mode => mode === 'keepalive' || mode === 'claim' || mode === 'full')
  const manualPlans = group.manualPlans === true
  // 派猫猫旅行只有 WorkBuddy 国内版一家有（国际版没有这个活动，打过去是 400）
  const canTravel = group.id === 'workbuddy'
  const [travelOpen, setTravelOpen] = React.useState(false)
  const progress = checkinPlanProgress(daily) && daily
    ? t('已领 {done} / {total} 份套餐', { done: daily.completedCount, total: daily.totalCount })
    : ''
  return (
    <React.Fragment>
      <DetailLine>
        <span className='min-w-0 flex-1 truncate'>{account.name}
          {account.streakDays > 0
            ? <span className='text-muted-foreground'> · {t('连登 {days} 天', { days: account.streakDays })}</span> : null}
        </span>
        {account.available ? null : <Badge variant='outline' shape='tag'>{t('凭据不可用')}</Badge>}
        {/* 异常信息优先于进度读数：它是「为什么还没签上」的答案 */}
        {daily?.message && status !== 'done' ? (
          <span className='max-w-[320px] flex-1 truncate text-[11.5px] text-muted-foreground'
            title={t(daily.message)}>{t(daily.message)}</span>
        ) : progress ? (
          <span className='shrink-0 text-[11.5px] text-muted-foreground'>{progress}</span>
        ) : null}
        {daily?.kind === 'rolling' && daily.until ? <span className='shrink-0 tabular-nums text-muted-foreground'>
          {t('下次检查：{time}', { time: formatShort(daily.until) })}
        </span> : null}
        <span className='shrink-0 tabular-nums text-muted-foreground'>
          {account.checkinAt ? t('上次 {time}', { time: formatShort(account.checkinAt) }) : t('从未签到')}
        </span>
        <StatusBadge status={status} />
        <span className='flex shrink-0 items-center gap-0.5'>
          {dailyActivity
            ? modes.map(mode => (
              <Button
                key={mode}
                size='xs'
                variant='ghost'
                disabled={busy(mode) || !account.available}
                title={MODE_TITLE[mode]}
                onClick={() => void signSingle(account.id, mode)}
              >
                {busy(mode) ? `${MODE_LABEL[mode] ?? mode}中…` : (MODE_LABEL[mode] ?? mode)}
              </Button>
            ))
            : (
              <Button
                size='xs'
                variant='ghost'
                disabled={busy('checkin') || !account.available}
                onClick={() => {
                  if (manualPlans && window.wbZcodePlans?.open) {
                    window.wbZcodePlans.open({ id: account.id, name: account.name })
                  } else {
                    void signSingle(account.id, 'checkin')
                  }
                }}
              >
                {manualPlans ? t('套餐明细') : busy('checkin')
                  ? t(daily?.queryOnly ? '查询中…' : '签到中…') : t(checkinActionLabel(account))}
              </Button>
            )}
          {canTravel ? (
            <Button
              size='xs'
              variant='outline'
              title={t('派猫猫旅行：查看状态，派发 / 领取')}
              onClick={() => setTravelOpen(open => !open)}
            >
              {t('旅行')}
            </Button>
          ) : null}
        </span>
      </DetailLine>
      {canTravel && travelOpen ? <TravelPanel accountId={account.id} /> : null}
    </React.Fragment>
  )
}

/** 每日签到的账号列表（右栏）：左栏选全部时按家分组，选一家时只列这一家。 */
export function DailyAccountList({ entries, showProvider }: {
  entries: Array<{ group: ProviderGroup; accounts: CheckinAccount[] }>
  showProvider: boolean
}): React.ReactElement {
  const rows = entries.filter(entry => entry.accounts.length > 0)
  if (!rows.length) return <p className='ck-list-empty'>{t('没有符合条件的账号。')}</p>
  return (
    <div className='ck-account-list'>
      {rows.map(entry => (
        <React.Fragment key={entry.group.id}>
          {showProvider ? <div className='ck-account-group'>
            <ProviderLogo id={entry.group.id} label={entry.group.label} />
            <span>{t(entry.group.label)}</span>
            <span className='text-muted-foreground'>{t('{done} / {total} 个账号已签', {
              done: entry.group.doneCount, total: entry.group.totalCount,
            })}</span>
          </div> : null}
          {entry.accounts.map(account => (
            <AccountLine key={account.id} account={account} group={entry.group} />
          ))}
        </React.Fragment>
      ))}
    </div>
  )
}

/* ─── 派猫猫旅行（WorkBuddy 国内版）────────── */

/**
 * 账号行下方的旅行区域（行上的「旅行」按钮展开时才挂载）。
 *
 * ── 惰性查询 ────────────────────────────────────────────────
 * 展开即打一次 `GET /api/accounts/{id}/travel` —— 旅行状态刻意不进
 * `checkin-center` 快照（快照必须「零上游请求」），所以只有展开的账号才打上游。
 *
 * ── 只读 vs 可操作 ──────────────────────────────────────────
 * 旅行总开关 `auto.enabled` 关着时这里只读（不给派发 / 领取按钮），开着才给。
 * 倒计时把「剩余秒数」按 `arriveAt - serverNow` 换算成本地绝对时刻，再用本地时钟
 * 每秒推进 —— 上游时钟与本机不一定同步，直接拿本机时间减 arriveAt 会偏。
 */
function TravelPanel({ accountId }: { accountId: string }): React.ReactElement {
  const cache = travelOf(accountId)
  const status = cache.status
  const [deadline, setDeadline] = React.useState<number | null>(null)
  const [now, setNow] = React.useState(() => Date.now())

  React.useEffect(() => { void fetchTravel(accountId) }, [accountId])

  // 拿到 traveling 状态那一刻，把「剩余秒数」锚成本地绝对时刻
  React.useEffect(() => {
    if (status?.state === 'traveling' && status.arriveAt > 0) {
      setDeadline(Date.now() + Math.max(0, status.arriveAt - status.serverNow) * 1000)
    } else {
      setDeadline(null)
    }
  }, [status])

  // 仅在旅行中每秒推进本地时钟
  React.useEffect(() => {
    if (deadline === null) return
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => { window.clearInterval(timer) }
  }, [deadline])

  const remaining = deadline === null ? 0 : Math.max(0, Math.round((deadline - now) / 1000))
  const countdown = `${Math.floor(remaining / 60)}:${String(remaining % 60).padStart(2, '0')}`
  const enabled = cache.auto?.enabled === true

  return (
    <div className='flex flex-wrap items-center gap-2 px-3.5 pb-2 text-[12px] text-muted-foreground'>
      <span>{t('旅行')}</span>
      {!status ? (
        cache.error
          ? <span>{t('读取旅行状态失败：{error}', { error: cache.error })}</span>
          : <Spinner className='size-3.5' />
      ) : !status.available ? (
        <Badge variant='outline' shape='tag'>{t('暂无旅行活动')}</Badge>
      ) : status.state === 'traveling' ? (
        <>
          <Badge variant='brand' shape='tag'>{t('旅行中')}</Badge>
          <span className='font-mono tabular-nums'>{countdown}</span>
        </>
      ) : status.state === 'arrived' ? (
        <>
          <Badge variant='success' shape='tag'>{t('可领 {n} 积分', { n: status.rewardCredit })}</Badge>
          {enabled ? (
            <Button size='xs' variant='default' disabled={cache.loading}
              onClick={() => void runTravel(accountId)}>
              {t('领取')}
            </Button>
          ) : null}
        </>
      ) : (
        <>
          <Badge variant='outline' shape='tag'>{t('空闲')}</Badge>
          {enabled ? (
            <Button size='xs' variant='outline' disabled={cache.loading}
              onClick={() => void runTravel(accountId)}>
              {t('派发')}
            </Button>
          ) : null}
        </>
      )}
      {status?.dailyLimitReached ? <span>{t('今日派遣已用完')}</span> : null}
    </div>
  )
}

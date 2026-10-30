import { t } from '../i18n'

/**
 * 后端 `RoutingStrategy::ALL` 的七档（`as_str()` 的连字符形态，GET 直接返回这组值）。
 *
 * 同时兼容 Task 3.3 的 snake_case canonical id 与上游旧 id：
 *   - `order`    → `priority`
 *   - `rotate`   → `global-round-robin`（整表轮转）
 *   - `provider` → `round-robin`（家内轮询）
 *   - `usage`    → `balance-weighted`
 *   - `round_robin` / `lru` / `least_connections` / `balance_weighted` /
 *     `priority_weighted` → 对应连字符档位
 *
 * 界面统一用连字符档位作为内部值（与 GET 返回、PUT 接受的 `as_str()` 一致），
 * 别名只在读入时归一，避免把旧 id 写回配置。
 */
export const ROUTING_STRATEGIES = [
  {
    value: 'priority',
    label: () => t('顺序优先'),
    hint: () => t('优先使用队首账号，不可用时顺延。'),
  },
  {
    value: 'least-connections',
    label: () => t('最少连接'),
    hint: () => t('优先挑当前在途请求数最少的账号，把并发摊平。'),
  },
  {
    value: 'round-robin',
    label: () => t('家内轮询'),
    hint: () => t('提供商保持优先顺序，同一提供商的账号轮流承接。'),
  },
  {
    value: 'global-round-robin',
    label: () => t('负载均衡'),
    hint: () => t('可用账号按请求轮换；开启会话粘性时同一会话优先留在原账号。'),
  },
  {
    value: 'least-recently-used',
    label: () => t('最近最少使用'),
    hint: () => t('优先挑最久没有被选中的账号。'),
  },
  {
    value: 'balance-weighted',
    label: () => t('余额加权'),
    hint: () => t('按账号余额加权轮询：余额越多分到的请求越多。'),
  },
  {
    value: 'priority-weighted',
    label: () => t('优先级加权'),
    hint: () => t('按优先级加权轮询：优先级数字越小权重越高。'),
  },
] as const

export type RoutingStrategyId = (typeof ROUTING_STRATEGIES)[number]['value']

const ALIASES: Record<string, RoutingStrategyId> = {
  // 后端 as_str()（GET 返回值）
  priority: 'priority',
  'least-connections': 'least-connections',
  'round-robin': 'round-robin',
  'global-round-robin': 'global-round-robin',
  'least-recently-used': 'least-recently-used',
  'balance-weighted': 'balance-weighted',
  'priority-weighted': 'priority-weighted',
  // Task 3.3 canonical（snake_case）
  least_connections: 'least-connections',
  round_robin: 'global-round-robin',
  global_round_robin: 'global-round-robin',
  lru: 'least-recently-used',
  least_recently_used: 'least-recently-used',
  balance_weighted: 'balance-weighted',
  priority_weighted: 'priority-weighted',
  provider: 'round-robin',
  // 上游旧 id
  order: 'priority',
  rotate: 'global-round-robin',
  usage: 'balance-weighted',
}

/** 旧 id / snake_case / 连字符 id → 界面内部值；认不出时回落后端默认档 `priority`。 */
export function normalizeRoutingStrategy(value?: string): RoutingStrategyId {
  const key = String(value ?? '').trim().toLowerCase()
  return ALIASES[key] ?? 'priority'
}

/** 策略短名：后端随行给的 `strategyLabel` 优先，其次按 id 归一后查表。 */
export function routingStrategyLabel(value?: string, backendLabel?: string): string {
  if (backendLabel) return backendLabel
  const id = normalizeRoutingStrategy(value)
  return ROUTING_STRATEGIES.find(item => item.value === id)?.label() ?? t('顺序优先')
}

/** 策略说明（设置弹窗里显示在控件下方）。 */
export function routingStrategyHint(value?: string): string {
  const id = normalizeRoutingStrategy(value)
  return ROUTING_STRATEGIES.find(item => item.value === id)?.hint() ?? t('优先使用队首账号，不可用时顺延。')
}

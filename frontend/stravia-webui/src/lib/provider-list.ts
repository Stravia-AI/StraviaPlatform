import * as m from '#lib/paraglide/messages.js'
import { formatCostPlanPrice, formatLowerBoundMoney, formatValueMultiple, hasMoneyRecords } from '#lib/money.js'
import type { CustomCurrency, Provider, ProviderStats, Route } from '#lib/types.js'

/** 供应商列表的一行：供应商本身加上列表单独加载的统计与关联数据，排序与展示共用同一份数据。 */
export interface ProviderListRow {
  provider: Provider
  /** 通过 Target 关联到该供应商的模型数；模型清单未加载时为 null。 */
  linkedModelCount: number | null
  /** 近 30 天统计行；窗口内没有该供应商的记录，或统计未加载时为 null。 */
  stats: ProviderStats | null
}

/** 至少有一个 Target 指向该供应商的模型数；同一模型指向该供应商多次只计一次。 */
export function countLinkedModels(providerId: string, routes: readonly Route[]): number {
  return routes.filter((route) => route.targets.some((target) => target.provider_id === providerId)).length
}

export type ProviderInventoryProblem = 'failure' | 'partial_success'

export type ProviderInventoryState =
  /** 列表响应没有携带清单概况。 */
  | { kind: 'unknown' }
  /** 没有任何模型，渠道也不提供模型发现。 */
  | { kind: 'none' }
  /** 渠道提供模型发现，但从未检查过。 */
  | { kind: 'unsynced' }
  | {
      kind: 'inventory'
      available: number
      total: number
      /** 最近一次同步检查的时间；手动模型从未同步过时为 null。 */
      checkedAt: string | null
      status: 'ok' | 'syncing' | ProviderInventoryProblem
    }

/** `canDiscover`：渠道声明了 `model_discovery` 能力。 */
export function providerInventoryState(provider: Provider, canDiscover: boolean): ProviderInventoryState {
  const inventory = provider.model_inventory
  if (!inventory) return { kind: 'unknown' }
  const check = inventory.last_check
  if (inventory.total === 0 && !check && !inventory.in_progress) {
    return canDiscover ? { kind: 'unsynced' } : { kind: 'none' }
  }
  const status = inventory.in_progress
    ? 'syncing'
    : check?.outcome === 'failure' || check?.outcome === 'partial_success'
      ? check.outcome
      : 'ok'
  return {
    kind: 'inventory',
    available: inventory.available,
    total: inventory.total,
    checkedAt: check?.checked_at ?? null,
    status,
  }
}

/** 排序用：可用模型数；未知排在最前。 */
export function providerInventorySortValue(provider: Provider): number {
  return provider.model_inventory?.available ?? -1
}

export type ProviderCostState =
  | { kind: 'loading' }
  | { kind: 'unavailable' }
  /** 供应商成本方式为“不计算”。 */
  | { kind: 'untracked' }
  /** `amount` 为 null 表示窗口内没有已计价的用量。 */
  | { kind: 'usage'; amount: string | null }
  | { kind: 'subscription'; price: string; multiple: string | null }

export interface ProviderCostContext {
  statsStatus: 'pending' | 'error' | 'success'
  currencyStatus: 'pending' | 'error' | 'success'
  customCurrency: CustomCurrency | null
}

/**
 * 成本列的展示状态。订阅制显示订阅价，价值倍数来自同一窗口的统计；按量显示窗口内分摊成本。
 * 加载中与加载失败是未知状态，不显示成 0。
 */
export function providerCostState(row: ProviderListRow, context: ProviderCostContext): ProviderCostState {
  const plan = row.provider.cost_plan
  const basis = plan?.basis ?? 'usage'
  if (basis === 'none') return { kind: 'untracked' }

  if (basis === 'subscription') {
    if (!plan?.price) return { kind: 'unavailable' }
    // 自定义币种的符号来自币种设置；设置没读到时不能用美元符号冒充。
    if (plan.price.currency === 'custom') {
      if (context.currencyStatus === 'pending') return { kind: 'loading' }
      if (context.currencyStatus === 'error') return { kind: 'unavailable' }
    }
    const unit = plan.period === 'year' ? m.cost_plan_per_year() : m.cost_plan_per_month()
    const subscription = row.stats?.subscription
    return {
      kind: 'subscription',
      price: `${formatCostPlanPrice(plan.price, context.customCurrency)} ${unit}`,
      multiple: subscription?.multiple == null ? null : formatValueMultiple(subscription),
    }
  }

  if (context.statsStatus === 'pending') return { kind: 'loading' }
  if (context.statsStatus === 'error') return { kind: 'unavailable' }
  const cost = row.stats?.allocated_cost
  return {
    kind: 'usage',
    amount: hasMoneyRecords(cost) && row.stats ? formatLowerBoundMoney(cost, row.stats.currency) : null,
  }
}

/** 排序用：窗口内分摊成本；没有记录或未加载排在最前。 */
export function providerCostSortValue(row: ProviderListRow): number {
  const amount = Number(row.stats?.allocated_cost?.amount)
  return Number.isFinite(amount) ? amount : -1
}

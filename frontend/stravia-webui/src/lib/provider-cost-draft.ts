import * as m from '#lib/paraglide/messages.js'
import type { CostBasis, CurrencyKind, NewProviderCostPlan, ProviderCostPlan, SubscriptionPeriod } from '#lib/types.js'

/**
 * Provider 成本方式的表单草稿。与连接字段同属一份表单草稿，
 * 但比较对象是当前生效的成本记录，而不是上次保存时的表单快照。
 */
export interface ProviderCostDraftFields {
  basis: CostBasis
  period: SubscriptionPeriod
  amount: string
  currency: CurrencyKind
}

/** 与后端一致的十进制金额写法：非负、无指数、无正负号。 */
const costAmountPattern = /^(?:0|[1-9]\d*)(?:\.\d+)?$/

export function isCostAmountValid(amount: string): boolean {
  const value = amount.trim()
  return costAmountPattern.test(value) && Number(value) >= 0
}

/** 没有记录即按量；订阅记录没有价格时保持可编辑的空值。 */
export function costDraftFromPlan(plan: ProviderCostPlan | null | undefined): ProviderCostDraftFields {
  if (plan?.basis === 'subscription') {
    return {
      basis: 'subscription',
      period: plan.period ?? 'month',
      amount: plan.price?.amount ?? '',
      currency: plan.price?.currency ?? 'usd',
    }
  }
  return { basis: plan?.basis ?? 'usage', period: 'month', amount: '', currency: 'usd' }
}

/**
 * 草稿是否与当前生效记录相同。只有订阅方式比较周期、金额与币种；
 * 不计算与按量没有其他可编辑字段。金额按字面比较：服务端保存原始写法，
 * 用户改动小数写法也视为一次显式变更，避免静默忽略输入。
 */
export function costDraftMatchesPlan(
  draft: ProviderCostDraftFields,
  plan: ProviderCostPlan | null | undefined,
): boolean {
  if (draft.basis !== (plan?.basis ?? 'usage')) return false
  if (draft.basis !== 'subscription') return true
  return (
    draft.period === (plan?.period ?? 'month') &&
    draft.amount.trim() === (plan?.price?.amount ?? '') &&
    draft.currency === (plan?.price?.currency ?? 'usd')
  )
}

/** 订阅价格校验错误；其余方式没有可校验的金额。 */
export function costDraftIssue(draft: ProviderCostDraftFields): string {
  return draft.basis === 'subscription' && !isCostAmountValid(draft.amount) ? m.cost_amount_invalid() : ''
}

/** 追加记录用的请求体；订阅价格无效时返回 null，不发出半成品。 */
export function costPlanInput(draft: ProviderCostDraftFields): NewProviderCostPlan | null {
  if (draft.basis !== 'subscription') return { basis: draft.basis }
  if (!isCostAmountValid(draft.amount)) return null
  return {
    basis: 'subscription',
    period: draft.period,
    price: { amount: draft.amount.trim(), currency: draft.currency },
  }
}

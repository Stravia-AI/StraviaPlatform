import { convertUsdToCustom, roundToCents } from '#lib/money.js'
import * as m from '#lib/paraglide/messages.js'
import type {
  CostBasis,
  CurrencyKind,
  CustomCurrency,
  NewProviderCostPlan,
  ProviderAllowanceBilling,
  ProviderCostPlan,
  ProviderCostPlanPrice,
  SubscriptionPeriod,
} from '#lib/types.js'

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

/** 供应商插件识别出的计费方式，整理为可展示、可一键带入编辑表单的成本方式建议。 */
export interface CostSuggestion {
  kind: 'usage' | 'subscription'
  planLabel: string | null
  /** 插件未给出周期时为 null，带入表单时按月付。 */
  period: SubscriptionPeriod | null
  /** 一个周期的官方标价，已换算成展示币种：配置了自定义币种时为自定义币种，否则为美元。插件没有给出美元标价时为 null。 */
  price: ProviderCostPlanPrice | null
}

/** 插件标价一律是美元；配置了自定义币种且汇率可用时按当前汇率换算，否则保持美元，由 Core 负责展示换算。 */
function suggestedPrices(
  billing: ProviderAllowanceBilling,
  customCurrency: CustomCurrency | null | undefined,
): { usd: ProviderCostPlanPrice; display: ProviderCostPlanPrice } | null {
  if (billing.price?.currency !== 'USD') return null
  const usd: ProviderCostPlanPrice = { amount: billing.price.amount, currency: 'usd' }
  const converted = customCurrency ? convertUsdToCustom(usd.amount, customCurrency.usd_rate) : null
  return { usd, display: converted === null ? usd : { amount: converted, currency: 'custom' } }
}

/**
 * 插件识别结果只是建议，不自动写入：成本方式只追加，一次误判会截断前一段订阅的摊销。
 * 用户明确选择「不计算」后不再提示；已是订阅时只有标价不同才值得提示，没有标价的识别结果不带来新信息。
 */
export function costSuggestionFromAllowance(
  billing: ProviderAllowanceBilling | null | undefined,
  planLabel: string | null | undefined,
  plan: ProviderCostPlan | null | undefined,
  customCurrency: CustomCurrency | null | undefined,
): CostSuggestion | null {
  if (!billing) return null
  const current = plan?.basis ?? 'usage'
  if (current === 'none') return null
  if (billing.kind === 'usage') {
    return current === 'subscription' ? { kind: 'usage', planLabel: null, period: null, price: null } : null
  }
  const prices = suggestedPrices(billing, customCurrency)
  const suggestion: CostSuggestion = {
    kind: 'subscription',
    planLabel: planLabel?.trim() || null,
    period: billing.period ?? null,
    price: prices?.display ?? null,
  }
  if (current !== 'subscription') return suggestion
  if (!prices) return null
  // 当前价格用什么币种写的，就在什么币种下比较，两侧都按分取整。
  const comparable = plan?.price?.currency === 'usd' ? prices.usd : prices.display
  const sameAsCurrent =
    plan?.price?.currency === comparable.currency &&
    roundToCents(plan.price.amount) === roundToCents(comparable.amount) &&
    (plan.period ?? 'month') === (suggestion.period ?? 'month')
  return sameAsCurrent ? null : suggestion
}

/** 采用建议：订阅且有标价时直接带入，否则金额留空等用户补全；展示币种优先于 USD 供用户填写。 */
export function costDraftFromSuggestion(
  suggestion: CostSuggestion,
  customCurrencyConfigured: boolean,
): ProviderCostDraftFields {
  if (suggestion.kind === 'usage') return { basis: 'usage', period: 'month', amount: '', currency: 'usd' }
  return {
    basis: 'subscription',
    period: suggestion.period ?? 'month',
    amount: suggestion.price?.amount ?? '',
    currency: suggestion.price?.currency ?? (customCurrencyConfigured ? 'custom' : 'usd'),
  }
}

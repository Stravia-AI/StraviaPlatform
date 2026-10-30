import * as m from '#lib/paraglide/messages.js'
import { formatPercent } from '#lib/format.js'
import { getLocale } from '#lib/paraglide/runtime.js'
import type {
  CurrencyKind,
  CustomCurrency,
  DisplayAmount,
  DisplayCurrency,
  MoneyAmount,
  ProviderCostPlanPrice,
  SubscriptionValue,
} from '#lib/types.js'

/** USD is always the fallback; the UI never converts, it only chooses the symbol to label API amounts. */
export function currencyForKind(kind: CurrencyKind, custom: CustomCurrency | null | undefined): DisplayCurrency {
  return kind === 'custom' ? { kind, symbol: custom?.symbol?.trim() || '$' } : { kind: 'usd', symbol: '$' }
}

export function displayCurrency(custom: CustomCurrency | null | undefined): DisplayCurrency {
  return custom ? { kind: 'custom', symbol: custom.symbol.trim() || '$' } : { kind: 'usd', symbol: '$' }
}

/** 保留原精度的数值格式，用于汇率和图表刻度；成本读数使用 {@link formatCostNumber}。 */
export function formatAmountNumber(amount: string, locale = getLocale()): string {
  const text = String(amount)
  const value = Number(text)
  if (!Number.isFinite(value)) return text
  const fractionDigits = text.split('.')[1]?.length ?? 0
  const maximumFractionDigits = Math.min(8, Math.max(2, fractionDigits))
  const formatted = new Intl.NumberFormat(locale, { minimumFractionDigits: 2, maximumFractionDigits }).format(value)
  // Avoid presenting a real but tiny amount as zero; Core already limited precision.
  const rounded = Number(formatted.replace(/[^\d.-]/g, ''))
  return value !== 0 && rounded === 0 ? text : formatted
}

export function formatAmount(amount: string, symbol: string, locale = getLocale()): string {
  return `${symbol}${formatAmountNumber(amount, locale)}`
}

/**
 * 十进制字符串按「中点远离零」取整到两位小数，返回以分为单位的整数。
 * 在字符串上取整，避免 `toFixed` 对二进制浮点（如 1.005）的误判；Core 只输出普通十进制，不含科学计数法。
 */
export function roundToCents(amount: string): bigint | null {
  const match = /^(-?)(\d+)(?:\.(\d+))?$/.exec(String(amount).trim())
  if (!match) return null
  const [, sign, whole, fraction = ''] = match
  const digits = `${fraction}000`
  let cents = BigInt(whole + digits.slice(0, 2))
  if (digits.charCodeAt(2) >= 0x35) cents += 1n
  return sign ? -cents : cents
}

/** 成本读数：统一四舍五入到两位小数。无法解析时原样返回，不伪造数值。 */
export function formatCostNumber(amount: string, locale = getLocale()): string {
  const cents = roundToCents(amount)
  if (cents === null) return String(amount)
  return new Intl.NumberFormat(locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 }).format(
    Number(cents) / 100,
  )
}

export function formatCostAmount(amount: string, symbol: string, locale = getLocale()): string {
  return `${symbol}${formatCostNumber(amount, locale)}`
}

export function isMoneyUnknown(money: MoneyAmount): boolean {
  // Subscription amortization contributes an amount without adding known attempts.
  return money.known_count === 0 && money.unknown_count > 0 && Number(money.amount) === 0
}

/** 没有任何已知、未知尝试且金额为零时，说明没有被计算成本的用量（例如全部供应商都不计算成本）。 */
export function hasMoneyRecords(money: MoneyAmount | null | undefined): money is MoneyAmount {
  return !!money && (money.known_count + money.unknown_count > 0 || Number(money.amount) !== 0)
}

export function formatMoney(money: MoneyAmount | null | undefined, currency: DisplayCurrency | undefined): string {
  if (!money) return '—'
  if (isMoneyUnknown(money)) return m.billing_unknown()
  return formatCostAmount(money.amount, currency?.symbol ?? '$')
}

/**
 * 订阅价值倍数（订阅 Provider 的等价按量成本 ÷ 订阅费用）。Core 精确计算；这里只取整展示。
 * 存在成本未知的尝试时等价按量成本只是下限，倍数同样只是下限，用「≥」标出。
 */
export function formatValueMultiple(subscription: SubscriptionValue | null | undefined): string {
  if (subscription?.multiple == null) return '—'
  const lowerBound = subscription.usage_equivalent_cost.unknown_count > 0
  return `${lowerBound ? '≥' : ''}${formatCostNumber(subscription.multiple)}×`
}

export function formatDisplayAmount(amount: DisplayAmount | null | undefined): string {
  if (!amount) return m.billing_unknown()
  return formatAmount(amount.amount, amount.currency.symbol || '$')
}

export function formatCostPlanPrice(price: ProviderCostPlanPrice | null, custom: CustomCurrency | null): string {
  if (!price) return '—'
  return formatAmount(price.amount, currencyForKind(price.currency, custom).symbol)
}

/** Known share of all attempts that produced an amount; null when no attempt was recorded. */
export function moneyCoverage(money: MoneyAmount | null | undefined): number | null {
  if (!money) return null
  const total = money.known_count + money.unknown_count
  return total > 0 ? money.known_count / total : null
}

export function formatMoneyCoverage(money: MoneyAmount | null | undefined): string {
  const coverage = moneyCoverage(money)
  return coverage == null ? '—' : formatPercent(coverage)
}

export function moneyCoverageDetail(money: MoneyAmount | null | undefined): string | undefined {
  if (!money || money.known_count + money.unknown_count === 0) return undefined
  return m.billing_coverage_detail({ known: money.known_count, unknown: money.unknown_count })
}

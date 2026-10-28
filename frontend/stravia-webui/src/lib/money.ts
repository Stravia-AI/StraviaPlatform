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
} from '#lib/types.js'

/** USD is always the fallback; the UI never converts, it only chooses the symbol to label API amounts. */
export function currencyForKind(kind: CurrencyKind, custom: CustomCurrency | null | undefined): DisplayCurrency {
  return kind === 'custom' ? { kind, symbol: custom?.symbol?.trim() || '$' } : { kind: 'usd', symbol: '$' }
}

export function displayCurrency(custom: CustomCurrency | null | undefined): DisplayCurrency {
  return custom ? { kind: 'custom', symbol: custom.symbol.trim() || '$' } : { kind: 'usd', symbol: '$' }
}

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

export function isMoneyUnknown(money: MoneyAmount): boolean {
  // Subscription amortization contributes an amount without adding known attempts.
  return money.known_count === 0 && money.unknown_count > 0 && Number(money.amount) === 0
}

export function formatMoney(money: MoneyAmount | null | undefined, currency: DisplayCurrency | undefined): string {
  if (!money) return '—'
  if (isMoneyUnknown(money)) return m.billing_unknown()
  return formatAmount(money.amount, currency?.symbol ?? '$')
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

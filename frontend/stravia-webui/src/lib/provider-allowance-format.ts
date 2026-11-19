import { formatNumber, formatPercent } from '#lib/format.js'
import * as m from '#lib/paraglide/messages.js'
import type { Locale } from '#lib/paraglide/runtime.js'
import type { AllowanceAmount } from '#lib/types.js'

const creditFormatters: Record<Locale, Intl.NumberFormat> = {
  'en-US': new Intl.NumberFormat('en-US', { maximumSignificantDigits: 12 }),
  'zh-CN': new Intl.NumberFormat('zh-CN', { maximumSignificantDigits: 12 }),
}

function amountNumber(amount: AllowanceAmount | undefined, locale: Locale): string {
  if (!amount || !Number.isFinite(amount.value)) return '–'
  return amount.unit === 'credits' ? creditFormatters[locale].format(amount.value) : formatNumber(amount.value, locale)
}

export function formatAllowanceAmount(
  amount: AllowanceAmount | undefined,
  locale: Locale,
  limit?: AllowanceAmount,
): string {
  const denomination = limit ?? amount
  if (!denomination || !Number.isFinite(denomination.value)) return '–'
  const unit = denomination.currency?.trim() || localizedAllowanceUnit(denomination.unit, locale)
  const value = limit
    ? `${amountNumber(amount, locale)} / ${amountNumber(limit, locale)}`
    : amountNumber(amount, locale)
  return unit ? `${value} ${unit}` : value
}

export function formatAllowancePercent(value: number | undefined, locale: Locale): string {
  return value == null || !Number.isFinite(value) ? '–' : formatPercent(value / 100, locale)
}

function localizedAllowanceUnit(unit: string, locale: Locale): string {
  const options = { locale }
  switch (unit.trim()) {
    case 'tokens':
      return m.allowances_unit_tokens({}, options)
    case 'requests':
      return m.allowances_unit_requests({}, options)
    case 'credits':
      return m.allowances_unit_credits({}, options)
    case 'units':
      return m.allowances_unit_units({}, options)
    case 'currency':
      return m.allowances_unit_currency({}, options)
    default:
      return unit.trim()
  }
}

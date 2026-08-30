import { formatNumber, formatPercent } from '$lib/format'
import * as m from '$lib/paraglide/messages.js'
import type { Locale } from '$lib/paraglide/runtime.js'
import type { AllowanceAmount } from '$lib/types'

export function formatAllowanceAmount(amount: AllowanceAmount | undefined, locale: Locale): string {
  if (!amount || !Number.isFinite(amount.value)) return '–'
  const unit = amount.currency?.trim() || localizedAllowanceUnit(amount.unit, locale)
  const value = formatNumber(amount.value, locale)
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

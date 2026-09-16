import type { Allowance, AllowanceCondition } from '$lib/types'

export type ForecastBucket = 'exhausted' | 'will_exhaust' | 'no_risk' | 'unknown'

export function effectiveAllowanceCondition(
  allowance: Pick<Allowance, 'condition' | 'reset_at'>,
): AllowanceCondition | undefined {
  return allowance.condition === 'exhausted' && allowance.reset_at == null ? undefined : allowance.condition
}

export function forecastBucket(
  allowance: Pick<Allowance, 'condition' | 'reset_at' | 'forecast'>,
): ForecastBucket {
  if (effectiveAllowanceCondition(allowance) === 'exhausted') return 'exhausted'
  switch (allowance.forecast.status) {
    case 'will_exhaust':
      return 'will_exhaust'
    case 'no_risk':
      return 'no_risk'
    default:
      return 'unknown'
  }
}

export function remainingPercent(
  allowance: Pick<Allowance, 'used_percent' | 'remaining' | 'limit'>,
): number | undefined {
  if (allowance.used_percent != null && Number.isFinite(allowance.used_percent)) {
    return Math.max(0, 100 - allowance.used_percent)
  }
  if (
    allowance.remaining &&
    allowance.limit &&
    Number.isFinite(allowance.remaining.value) &&
    allowance.limit.value > 0
  ) {
    return Math.max(0, (allowance.remaining.value / allowance.limit.value) * 100)
  }
  return undefined
}

export function usableRemainingPercent(allowance: Allowance): number | undefined {
  if (effectiveAllowanceCondition(allowance) === 'exhausted') return undefined
  return remainingPercent(allowance)
}

export function projectedRemainingPercent(allowance: Allowance): number | undefined {
  if (forecastBucket(allowance) === 'exhausted') return undefined
  const value = allowance.forecast.projected_remaining_percent
  return value != null && Number.isFinite(value) ? value : undefined
}

export function worstAllowanceCondition(
  conditions: (AllowanceCondition | undefined)[],
): AllowanceCondition | undefined {
  let result: AllowanceCondition | undefined
  const rank = { normal: 1, tight: 2, exhausted: 3 } satisfies Record<AllowanceCondition, number>
  for (const condition of conditions) {
    if (condition && (!result || rank[condition] > rank[result])) result = condition
  }
  return result
}

export function exhaustedAllowances(allowances: readonly Allowance[]): Allowance[] {
  return allowances.filter((allowance) => effectiveAllowanceCondition(allowance) === 'exhausted')
}

function soonestResetAt(allowances: readonly Allowance[]): number | undefined {
  const resets = allowances
    .map((allowance) => allowance.reset_at)
    .filter((value): value is number => value != null)
  return resets.length ? Math.min(...resets) : undefined
}

export function nextRelevantResetAt(allowances: readonly Allowance[]): number | undefined {
  return (
    soonestResetAt(exhaustedAllowances(allowances)) ??
    soonestResetAt(allowances.filter((allowance) => forecastBucket(allowance) === 'will_exhaust')) ??
    soonestResetAt(allowances)
  )
}

export function summarizeForecast(allowances: readonly Allowance[]): {
  exhausted: number
  willExhaust: number
  noRisk: number
  unknown: number
  lowestProjected: number | undefined
} {
  let exhausted = 0
  let willExhaust = 0
  let noRisk = 0
  let unknown = 0
  const projected: number[] = []
  for (const allowance of allowances) {
    switch (forecastBucket(allowance)) {
      case 'exhausted':
        exhausted += 1
        break
      case 'will_exhaust':
        willExhaust += 1
        break
      case 'no_risk':
        noRisk += 1
        break
      case 'unknown':
        unknown += 1
        break
    }
    const remaining = projectedRemainingPercent(allowance)
    if (remaining != null) projected.push(remaining)
  }
  return {
    exhausted,
    willExhaust,
    noRisk,
    unknown,
    lowestProjected: projected.length ? Math.min(...projected) : undefined,
  }
}

import { describe, expect, test } from 'bun:test'

import {
  effectiveAllowanceCondition,
  forecastBucket,
  nextRelevantResetAt,
  projectedRemainingPercent,
  remainingPercent,
  summarizeForecast,
  usableRemainingPercent,
} from '../src/lib/provider-allowance-summary'
import type { Allowance } from '../src/lib/types'

function allowance(overrides: Partial<Allowance>): Allowance {
  return { key: 'weekly', label: 'Weekly', kind: 'quota_window', forecast: { status: 'unknown' }, ...overrides }
}

describe('allowance forecast presentation', () => {
  test('does not treat an exhausted balance without a reset as exhausted', () => {
    const item = allowance({
      key: 'credits_balance',
      kind: 'balance',
      remaining: { value: 0, unit: 'currency', currency: 'USD' },
      condition: 'exhausted',
      forecast: { status: 'unknown' },
    })

    expect(effectiveAllowanceCondition(item)).toBeUndefined()
    expect(forecastBucket(item)).toBe('unknown')
  })

  test('classifies an empty window as exhausted even if the forecast still says it will exhaust', () => {
    const item = allowance({
      used_percent: 100,
      reset_at: 1_788_796_800_000,
      condition: 'exhausted',
      forecast: { status: 'will_exhaust', projected_remaining_percent: 0, exhausts_at: 1_788_700_000_000 },
    })

    expect(forecastBucket(item)).toBe('exhausted')
    expect(projectedRemainingPercent(item)).toBeUndefined()
    expect(usableRemainingPercent(item)).toBeUndefined()
    expect(remainingPercent(item)).toBe(0)
  })

  test('does not call a flat empty window no-risk', () => {
    const item = allowance({
      used_percent: 100,
      reset_at: 1_788_796_800_000,
      condition: 'exhausted',
      forecast: { status: 'no_risk', projected_remaining_percent: 0 },
    })

    expect(forecastBucket(item)).toBe('exhausted')
    expect(summarizeForecast([item])).toEqual({
      exhausted: 1,
      willExhaust: 0,
      noRisk: 0,
      unknown: 0,
      lowestProjected: undefined,
    })
  })

  test('keeps a still-available window that will run out as will-exhaust', () => {
    const item = allowance({
      used_percent: 80,
      reset_at: 1_788_883_200_000,
      condition: 'tight',
      forecast: { status: 'will_exhaust', projected_remaining_percent: 0, exhausts_at: 1_788_850_000_000 },
    })

    expect(forecastBucket(item)).toBe('will_exhaust')
    expect(projectedRemainingPercent(item)).toBe(0)
    expect(usableRemainingPercent(item)).toBe(20)
  })

  test('ignores exhausted windows when summarizing remaining and next reset', () => {
    const healthy = allowance({
      key: 'billing_cycle',
      used_percent: 53,
      reset_at: 1_788_000_000_000,
      condition: 'normal',
      forecast: { status: 'no_risk', projected_remaining_percent: 40 },
    })
    const empty = allowance({
      used_percent: 100,
      reset_at: 1_789_000_000_000,
      condition: 'exhausted',
      forecast: { status: 'will_exhaust', projected_remaining_percent: 0, exhausts_at: 1_788_500_000_000 },
    })
    const upcoming = allowance({
      key: '5h',
      used_percent: 70,
      reset_at: 1_788_200_000_000,
      condition: 'tight',
      forecast: { status: 'will_exhaust', projected_remaining_percent: 5, exhausts_at: 1_788_100_000_000 },
    })

    expect(summarizeForecast([healthy, empty, upcoming])).toEqual({
      exhausted: 1,
      willExhaust: 1,
      noRisk: 1,
      unknown: 0,
      lowestProjected: 5,
    })
    expect(nextRelevantResetAt([healthy, empty, upcoming])).toBe(empty.reset_at)
    expect(nextRelevantResetAt([healthy, upcoming])).toBe(upcoming.reset_at)
    expect(nextRelevantResetAt([healthy])).toBe(healthy.reset_at)
  })
})

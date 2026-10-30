import { describe, expect, test } from 'bun:test'

import {
  costDraftFromPlan,
  costDraftIssue,
  costDraftMatchesPlan,
  costPlanInput,
  isCostAmountValid,
} from '../src/lib/provider-cost-draft'
import type { ProviderCostPlan } from '../src/lib/types'

function plan(overrides: Partial<ProviderCostPlan> = {}): ProviderCostPlan {
  return {
    id: 'plan-1',
    provider_id: 'provider-1',
    effective_from: 1,
    basis: 'usage',
    period: null,
    price: null,
    created_at: '2026-01-01T00:00:00Z',
    ...overrides,
  }
}

describe('provider cost draft', () => {
  test('a provider without records starts from the usage basis', () => {
    const draft = costDraftFromPlan(null)
    expect(draft).toEqual({ basis: 'usage', period: 'month', amount: '', currency: 'usd' })
    expect(costDraftMatchesPlan(draft, null)).toBe(true)
    expect(costDraftMatchesPlan(draft, plan({ basis: 'usage' }))).toBe(true)
  })

  test('none is a distinct basis with no editable price', () => {
    const draft = costDraftFromPlan(plan({ basis: 'none' }))
    expect(draft).toEqual({ basis: 'none', period: 'month', amount: '', currency: 'usd' })
    expect(costDraftMatchesPlan(draft, plan({ basis: 'none' }))).toBe(true)
    expect(costDraftMatchesPlan(draft, plan({ basis: 'usage' }))).toBe(false)
    expect(costPlanInput(draft)).toEqual({ basis: 'none' })
  })

  test('subscription keeps period, price and currency and compares each field', () => {
    const subscription = plan({
      basis: 'subscription',
      period: 'year',
      price: { amount: '120.50', currency: 'custom' },
    })
    const draft = costDraftFromPlan(subscription)
    expect(draft).toEqual({ basis: 'subscription', period: 'year', amount: '120.50', currency: 'custom' })
    expect(costDraftMatchesPlan(draft, subscription)).toBe(true)

    expect(costDraftMatchesPlan({ ...draft, basis: 'usage' }, subscription)).toBe(false)
    expect(costDraftMatchesPlan({ ...draft, period: 'month' }, subscription)).toBe(false)
    expect(costDraftMatchesPlan({ ...draft, amount: '120.51' }, subscription)).toBe(false)
    expect(costDraftMatchesPlan({ ...draft, currency: 'usd' }, subscription)).toBe(false)
  })

  test('subscription input trims the amount and refuses invalid prices', () => {
    const draft = costDraftFromPlan(plan({ basis: 'subscription', period: 'month' }))
    expect(costPlanInput({ ...draft, amount: ' 12.30 ' })).toEqual({
      basis: 'subscription',
      period: 'month',
      price: { amount: '12.30', currency: 'usd' },
    })
    expect(costPlanInput({ ...draft, amount: '' })).toBeNull()
    expect(costPlanInput({ ...draft, amount: '-1' })).toBeNull()
  })

  test('decimal amounts follow the backend grammar', () => {
    for (const amount of ['0', '10', '10.5', '0.0001', ' 7 ']) {
      expect(isCostAmountValid(amount), amount).toBe(true)
    }
    for (const amount of ['', ' ', '-1', '+1', '01', '1.', '.5', '1e3', '1,000']) {
      expect(isCostAmountValid(amount), amount).toBe(false)
    }
  })

  test('only subscription prices produce an issue', () => {
    expect(costDraftIssue({ basis: 'none', period: 'month', amount: 'bad', currency: 'usd' })).toBe('')
    expect(costDraftIssue({ basis: 'usage', period: 'month', amount: 'bad', currency: 'usd' })).toBe('')
    expect(costDraftIssue({ basis: 'subscription', period: 'month', amount: 'bad', currency: 'usd' })).not.toBe('')
  })
})

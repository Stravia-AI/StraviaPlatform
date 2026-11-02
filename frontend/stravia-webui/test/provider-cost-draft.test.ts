import { describe, expect, test } from 'bun:test'

import {
  costDraftFromPlan,
  costDraftFromSuggestion,
  costDraftIssue,
  costDraftMatchesPlan,
  costPlanInput,
  costSuggestionFromAllowance,
  isCostAmountValid,
} from '../src/lib/provider-cost-draft'
import type { CustomCurrency, ProviderAllowanceBilling, ProviderCostPlan } from '../src/lib/types'

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

describe('plugin billing suggestion', () => {
  const monthly100: ProviderAllowanceBilling = {
    kind: 'subscription',
    period: 'month',
    price: { amount: '100', currency: 'USD' },
  }
  const yuan: CustomCurrency = {
    symbol: '¥',
    usd_rate: '7.25',
    first_rate: '7.00',
    configured_at: '2026-01-01T00:00:00Z',
  }

  test('a subscription with a USD list price is adoptable as is', () => {
    const suggestion = costSuggestionFromAllowance(monthly100, ' Max 5x ', null, null)
    expect(suggestion).toEqual({
      kind: 'subscription',
      planLabel: 'Max 5x',
      period: 'month',
      price: { amount: '100', currency: 'usd' },
    })
    expect(costDraftFromSuggestion(suggestion!, false)).toEqual({
      basis: 'subscription',
      period: 'month',
      amount: '100',
      currency: 'usd',
    })
    expect(costDraftIssue(costDraftFromSuggestion(suggestion!, false))).toBe('')
  })

  test('with a custom currency the list price is converted at the current rate and adopted in that currency', () => {
    const suggestion = costSuggestionFromAllowance(
      { ...monthly100, price: { amount: '19.99', currency: 'USD' } },
      null,
      null,
      yuan,
    )
    // 19.99 × 7.25 = 144.9275 → 144.93
    expect(suggestion?.price).toEqual({ amount: '144.93', currency: 'custom' })
    const draft = costDraftFromSuggestion(suggestion!, true)
    expect(draft).toEqual({ basis: 'subscription', period: 'month', amount: '144.93', currency: 'custom' })
    expect(costPlanInput(draft)).toEqual({
      basis: 'subscription',
      period: 'month',
      price: { amount: '144.93', currency: 'custom' },
    })
  })

  test('an unusable rate keeps the USD list price instead of guessing a conversion', () => {
    for (const usd_rate of ['0', '', 'abc', '1e3']) {
      const suggestion = costSuggestionFromAllowance(monthly100, null, null, { ...yuan, usd_rate })
      expect(suggestion?.price, usd_rate).toEqual({ amount: '100', currency: 'usd' })
    }
  })

  test('a plan without a USD price leaves the amount for the user and prefers the display currency', () => {
    for (const billing of [
      { kind: 'subscription' } satisfies ProviderAllowanceBilling,
      {
        kind: 'subscription',
        period: 'year',
        price: { amount: '99', currency: 'CNY' },
      } satisfies ProviderAllowanceBilling,
    ]) {
      const suggestion = costSuggestionFromAllowance(billing, 'Pro', null, yuan)
      expect(suggestion?.price).toBeNull()
      const draft = costDraftFromSuggestion(suggestion!, true)
      expect(draft).toMatchObject({ basis: 'subscription', amount: '', currency: 'custom' })
      expect(costDraftIssue(draft)).not.toBe('')
      expect(costPlanInput(draft)).toBeNull()
    }
    expect(
      costDraftFromSuggestion(costSuggestionFromAllowance({ kind: 'subscription' }, null, null, null)!, false).currency,
    ).toBe('usd')
  })

  test('an explicit not-tracked choice is never second-guessed', () => {
    expect(costSuggestionFromAllowance(monthly100, 'Max 5x', plan({ basis: 'none' }), null)).toBeNull()
    expect(costSuggestionFromAllowance({ kind: 'usage' }, null, plan({ basis: 'none' }), null)).toBeNull()
  })

  test('an existing subscription is only challenged by a different list price', () => {
    const current = plan({ basis: 'subscription', period: 'month', price: { amount: '100.00', currency: 'usd' } })
    expect(costSuggestionFromAllowance(monthly100, 'Max 5x', current, null)).toBeNull()
    expect(costSuggestionFromAllowance({ kind: 'subscription' }, 'Max 5x', current, null)).toBeNull()
    expect(
      costSuggestionFromAllowance(
        { ...monthly100, price: { amount: '200', currency: 'USD' } },
        'Max 20x',
        current,
        null,
      ),
    ).toMatchObject({ price: { amount: '200', currency: 'usd' } })
    expect(costSuggestionFromAllowance({ ...monthly100, period: 'year' }, null, current, null)).toMatchObject({
      period: 'year',
    })
  })

  test('an existing subscription is compared in the currency it was priced in', () => {
    // 美元价格在配置了自定义币种后仍与美元标价比较，不因展示币种不同而重复提示。
    const inUsd = plan({ basis: 'subscription', period: 'month', price: { amount: '100', currency: 'usd' } })
    expect(costSuggestionFromAllowance(monthly100, null, inUsd, yuan)).toBeNull()
    expect(
      costSuggestionFromAllowance({ ...monthly100, price: { amount: '200', currency: 'USD' } }, null, inUsd, yuan),
    ).toMatchObject({ price: { amount: '1450.00', currency: 'custom' } })
    // 自定义币种价格与换算后的标价比较；同价不提示，价格不同或汇率变了才提示。
    const inYuan = plan({ basis: 'subscription', period: 'month', price: { amount: '725', currency: 'custom' } })
    expect(costSuggestionFromAllowance(monthly100, null, inYuan, yuan)).toBeNull()
    expect(costSuggestionFromAllowance(monthly100, null, inYuan, { ...yuan, usd_rate: '7.3' })).toMatchObject({
      price: { amount: '730.00', currency: 'custom' },
    })
    // 自定义币种已不存在时，无法再比较这笔自定义币种价格。
    expect(costSuggestionFromAllowance(monthly100, null, inYuan, null)).not.toBeNull()
  })

  test('pay-as-you-go is only suggested against a recorded subscription', () => {
    expect(costSuggestionFromAllowance({ kind: 'usage' }, null, null, null)).toBeNull()
    expect(costSuggestionFromAllowance({ kind: 'usage' }, null, plan({ basis: 'usage' }), null)).toBeNull()
    const subscribed = plan({ basis: 'subscription', period: 'month', price: { amount: '20', currency: 'usd' } })
    const suggestion = costSuggestionFromAllowance({ kind: 'usage' }, null, subscribed, null)
    expect(suggestion).toMatchObject({ kind: 'usage' })
    expect(costPlanInput(costDraftFromSuggestion(suggestion!, false))).toEqual({ basis: 'usage' })
  })

  test('without plugin billing there is nothing to suggest', () => {
    expect(costSuggestionFromAllowance(undefined, 'Pro', null, null)).toBeNull()
  })
})

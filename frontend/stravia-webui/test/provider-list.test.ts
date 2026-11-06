import { describe, expect, test } from 'bun:test'
import {
  countLinkedModels,
  providerCostSortValue,
  providerCostState,
  providerInventorySortValue,
  providerInventoryState,
  type ProviderCostContext,
  type ProviderListRow,
} from '../src/lib/provider-list'
import type { Provider, ProviderModelInventory, ProviderStats, Route, Target } from '../src/lib/types'
import type { MoneyAmount, ProviderCostPlan } from '../src/lib/types/money'

const target = (providerId: string): Target => ({
  id: `target-${providerId}`,
  output_modality_mismatch: false,
  route_output_modalities: ['text'],
  model_id: 'model',
  provider_id: providerId,
  model: 'upstream',
  enabled: true,
  priority: 0,
  first_token_timeout_ms: 0,
  target_retry_budget: 0,
  target_cooldown_ms: 0,
  created_at: '2026-01-01T00:00:00Z',
  thinking_level_map: [],
})

const route = (id: string, ...providerIds: string[]): Route => ({
  id,
  output_modality: 'text',
  image_generation_bound: false,
  model_id: id,
  balance: 'traffic_equalization',
  target_provider: providerIds[0] ?? '',
  target_model: null,
  is_enabled: true,
  created_at: '2026-01-01T00:00:00Z',
  supported_thinking_levels: [],
  supports_remote_compaction: false,
  targets: providerIds.map(target),
})

const inventory = (overrides: Partial<ProviderModelInventory> = {}): ProviderModelInventory => ({
  total: 10,
  available: 7,
  in_progress: false,
  last_check: { checked_at: '2026-01-02T00:00:00Z', outcome: 'success' },
  ...overrides,
})

const costPlan = (overrides: Partial<ProviderCostPlan>): ProviderCostPlan => ({
  id: 'plan',
  provider_id: 'p1',
  effective_from: 0,
  basis: 'usage',
  period: null,
  price: null,
  created_at: '2026-01-01T00:00:00Z',
  ...overrides,
})

const provider = (overrides: Partial<Provider> = {}): Provider => ({
  id: 'p1',
  name: 'Provider',
  protocol: 'openai',
  base_url: 'https://example.invalid',
  use_proxy: false,
  is_enabled: true,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
  ...overrides,
})

const money = (amount: string, known: number, unknown = 0): MoneyAmount => ({
  amount,
  known_count: known,
  unknown_count: unknown,
})

const stats = (overrides: Partial<ProviderStats> = {}): ProviderStats => ({
  provider_id: 'p1',
  deleted: false,
  provider: 'Provider',
  upstream_request_count: 0,
  upstream_error_count: 0,
  avg_duration_ms: null,
  avg_output_tps: null,
  currency: { kind: 'usd', symbol: '$' },
  cost: null,
  subscription: null,
  allocated_cost: null,
  ...overrides,
})

const row = (providerOverrides: Partial<Provider> = {}, rowStats: ProviderStats | null = null): ProviderListRow => ({
  provider: provider(providerOverrides),
  linkedModelCount: null,
  stats: rowStats,
})

const loaded: ProviderCostContext = { statsStatus: 'success', currencyStatus: 'success', customCurrency: null }

describe('countLinkedModels', () => {
  test('counts each model once even when several targets point at the provider', () => {
    const routes = [route('a', 'p1', 'p1', 'p2'), route('b', 'p2'), route('c', 'p1')]
    expect(countLinkedModels('p1', routes)).toBe(2)
    expect(countLinkedModels('p2', routes)).toBe(2)
    expect(countLinkedModels('missing', routes)).toBe(0)
  })
})

describe('providerInventoryState', () => {
  test('is unknown when the list response carries no inventory', () => {
    expect(providerInventoryState(provider(), true)).toEqual({ kind: 'unknown' })
    expect(providerInventoryState(provider({ model_inventory: null }), true)).toEqual({ kind: 'unknown' })
  })

  test('an empty never-checked provider is unsynced only if the channel can discover models', () => {
    const empty = provider({ model_inventory: inventory({ total: 0, available: 0, last_check: null }) })
    expect(providerInventoryState(empty, true)).toEqual({ kind: 'unsynced' })
    expect(providerInventoryState(empty, false)).toEqual({ kind: 'none' })
  })

  test('reports available out of total with a healthy status after a successful check', () => {
    expect(providerInventoryState(provider({ model_inventory: inventory() }), true)).toEqual({
      kind: 'inventory',
      available: 7,
      total: 10,
      checkedAt: '2026-01-02T00:00:00Z',
      status: 'ok',
    })
  })

  test('manual models without a sync check still show their counts', () => {
    const state = providerInventoryState(provider({ model_inventory: inventory({ last_check: null }) }), false)
    expect(state).toMatchObject({ kind: 'inventory', available: 7, total: 10, checkedAt: null, status: 'ok' })
  })

  test.each([
    ['failure', 'failure'],
    ['partial_success', 'partial_success'],
  ] as const)('a %s check surfaces as a problem status', (outcome, status) => {
    const p = provider({ model_inventory: inventory({ last_check: { checked_at: '2026-01-02T00:00:00Z', outcome } }) })
    expect(providerInventoryState(p, true)).toMatchObject({ kind: 'inventory', status })
  })

  test('an in-progress sync wins over the previous failure and keeps the stale counts', () => {
    const p = provider({
      model_inventory: inventory({
        in_progress: true,
        last_check: { checked_at: '2026-01-02T00:00:00Z', outcome: 'failure' },
      }),
    })
    expect(providerInventoryState(p, true)).toMatchObject({ kind: 'inventory', available: 7, status: 'syncing' })
  })

  test('first sync in progress is shown as syncing rather than unsynced', () => {
    const p = provider({ model_inventory: inventory({ total: 0, available: 0, in_progress: true, last_check: null }) })
    expect(providerInventoryState(p, true)).toMatchObject({ kind: 'inventory', total: 0, status: 'syncing' })
  })

  test('sorts by available models with unknown first', () => {
    expect(providerInventorySortValue(provider())).toBe(-1)
    expect(providerInventorySortValue(provider({ model_inventory: inventory({ available: 0 }) }))).toBe(0)
    expect(providerInventorySortValue(provider({ model_inventory: inventory() }))).toBe(7)
  })
})

describe('providerCostState', () => {
  test('providers that do not track cost say so instead of showing zero', () => {
    expect(providerCostState(row({ cost_plan: costPlan({ basis: 'none' }) }), loaded)).toEqual({ kind: 'untracked' })
  })

  test('usage cost is the allocated cost in the window', () => {
    const state = providerCostState(row({}, stats({ allocated_cost: money('12.5', 4) })), loaded)
    expect(state).toEqual({ kind: 'usage', amount: '$12.50' })
  })

  test('usage cost is marked as a lower bound when some attempts have unknown cost', () => {
    const state = providerCostState(row({}, stats({ allocated_cost: money('12.5', 4, 2) })), loaded)
    expect(state).toEqual({ kind: 'usage', amount: '≥$12.50' })
  })

  test('a usage provider without recorded attempts has no amount rather than zero', () => {
    expect(providerCostState(row({}, null), loaded)).toEqual({ kind: 'usage', amount: null })
    expect(providerCostState(row({}, stats({ allocated_cost: money('0', 0) })), loaded)).toEqual({
      kind: 'usage',
      amount: null,
    })
  })

  test('loading and failed statistics are unknown, never zero', () => {
    expect(providerCostState(row(), { ...loaded, statsStatus: 'pending' })).toEqual({ kind: 'loading' })
    expect(providerCostState(row(), { ...loaded, statsStatus: 'error' })).toEqual({ kind: 'unavailable' })
  })

  test('subscription shows the plan price and takes the value multiple from the window statistics', () => {
    const plan = costPlan({ basis: 'subscription', period: 'month', price: { amount: '20', currency: 'usd' } })
    const withStats = stats({
      subscription: { cost: money('20', 0), usage_equivalent_cost: money('50', 3), multiple: '2.5' },
    })
    const state = providerCostState(row({ cost_plan: plan }, withStats), loaded)
    expect(state).toMatchObject({ kind: 'subscription', multiple: '2.50×' })
    expect(state.kind === 'subscription' && state.price).toContain('$20')
  })

  test('subscription does not wait for statistics to show its price', () => {
    const plan = costPlan({ basis: 'subscription', period: 'year', price: { amount: '200', currency: 'usd' } })
    const state = providerCostState(row({ cost_plan: plan }), { ...loaded, statsStatus: 'error' })
    expect(state).toMatchObject({ kind: 'subscription', multiple: null })
  })

  test('a custom-currency subscription is not priced until the currency settings are known', () => {
    const plan = costPlan({ basis: 'subscription', period: 'month', price: { amount: '100', currency: 'custom' } })
    expect(providerCostState(row({ cost_plan: plan }), { ...loaded, currencyStatus: 'pending' })).toEqual({
      kind: 'loading',
    })
    expect(providerCostState(row({ cost_plan: plan }), { ...loaded, currencyStatus: 'error' })).toEqual({
      kind: 'unavailable',
    })
  })

  test('sorts by allocated cost with missing records first', () => {
    expect(providerCostSortValue(row())).toBe(-1)
    expect(providerCostSortValue(row({}, stats({ allocated_cost: money('0', 0) })))).toBe(0)
    expect(providerCostSortValue(row({}, stats({ allocated_cost: money('12.5', 4) })))).toBe(12.5)
  })
})

import { describe, expect, test } from 'bun:test'

import { routeDestinationSeverity, summarizeRouteDestinations } from '../src/lib/route-destinations'
import type { Provider, Route, Target } from '../src/lib/types'

function provider(id: string, overrides: Partial<Provider> = {}): Provider {
  return {
    id,
    name: `Service ${id}`,
    protocol: 'openai-compatible',
    base_url: 'http://127.0.0.1',
    use_proxy: false,
    is_enabled: true,
    created_at: '',
    updated_at: '',
    ...overrides,
  }
}

function target(providerId: string, priority: number, overrides: Partial<Target> = {}): Target {
  return {
    id: `${providerId}-${priority}`,
    model_id: 'route-id',
    provider_id: providerId,
    model: `model-${providerId}`,
    enabled: true,
    priority,
    first_token_timeout_ms: 60_000,
    target_retry_budget: 5,
    target_cooldown_ms: 120_000,
    created_at: '',
    thinking_level_map: [],
    ...overrides,
  }
}

function route(targets: Target[]): Route {
  return {
    id: 'route-id',
    model_id: 'route',
    balance: 'traffic_equalization',
    target_provider: '',
    target_model: null,
    is_enabled: true,
    created_at: '',
    supported_thinking_levels: [],
    targets,
  }
}

const invalid = { credential_status: 'invalid' }
const suspended = { allowance_suspension: { suspended_at: '', triggered_keys: ['weekly'], earliest_reset_at: null } }

describe('summarizeRouteDestinations', () => {
  test('single preferred target uses the Provider Model name and counts lower layers as fallback', () => {
    const summary = summarizeRouteDestinations(
      route([
        target('a', 10, { model_name: 'GPT Luna' }),
        target('b', 0),
        target('c', 0),
        target('d', -5),
        target('e', 99, { enabled: false }),
      ]),
      ['a', 'b', 'c', 'd', 'e'].map((id) => provider(id)),
    )

    expect(summary.preferred).toEqual({ kind: 'single', modelName: 'GPT Luna', providerName: 'Service a' })
    expect(summary.skippedLayers).toBe(0)
    expect(summary.fallbackLayers).toBe(2)
    expect(summary.fallbackTargets).toBe(3)
    expect(summary.unavailableTargets).toBe(0)
    expect(routeDestinationSeverity(summary)).toBe(0)
  })

  test('falls back to the upstream model ID when the name is not registered', () => {
    const summary = summarizeRouteDestinations(route([target('a', 0, { model_name: '  ' })]), [provider('a')])

    expect(summary.preferred).toEqual({ kind: 'single', modelName: 'model-a', providerName: 'Service a' })
  })

  test('several available targets in the preferred layer report the count and strategy', () => {
    const summary = summarizeRouteDestinations(route([target('a', 0), target('b', 0), target('c', 0)]), [
      provider('a'),
      provider('b'),
      provider('c', invalid),
    ])

    expect(summary.preferred).toEqual({ kind: 'multiple', count: 2, strategy: 'traffic_equalization' })
    expect(summary.unavailableTargets).toBe(1)
    expect(routeDestinationSeverity(summary)).toBe(1)
  })

  test('skips any number of fully unavailable leading layers as one contiguous range', () => {
    const summary = summarizeRouteDestinations(
      route([target('a', 40), target('b', 30), target('c', 20), target('d', 10), target('e', 0), target('f', -10)]),
      [
        provider('a', { is_enabled: false }),
        provider('b', invalid),
        provider('c', suspended),
        provider('d'),
        provider('e', invalid),
        provider('f'),
      ],
    )

    expect(summary.preferred).toEqual({ kind: 'single', modelName: 'model-d', providerName: 'Service d' })
    expect(summary.skippedLayers).toBe(3)
    // 第 5 层整层不可用不单独列层号，只计入不可用目标；第 6 层仍是后备。
    expect(summary.fallbackLayers).toBe(1)
    expect(summary.fallbackTargets).toBe(1)
    expect(summary.unavailableTargets).toBe(1)
    expect(routeDestinationSeverity(summary)).toBe(2)
  })

  test('without any available target counts each enabled target under one reason', () => {
    const summary = summarizeRouteDestinations(
      route([target('a', 1), target('b', 0), target('c', 0), target('d', 0, { enabled: false })]),
      [provider('a', { is_enabled: false, credential_status: 'invalid' }), provider('b', invalid), provider('c', suspended), provider('d')],
    )

    expect(summary.preferred).toBeNull()
    expect(summary.unavailableReasons).toEqual({ provider_disabled: 1, credential_invalid: 1, allowance_suspended: 1 })
    expect(routeDestinationSeverity(summary)).toBe(3)
  })
})

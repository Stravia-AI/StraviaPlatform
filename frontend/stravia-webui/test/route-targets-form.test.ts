import { describe, expect, test } from 'bun:test'

import { buildRouteTargets, createRouteTarget, createRouteTargetForms } from '../src/lib/components/route-targets-form'
import type { Route } from '../src/lib/types'

function routeWithTargets(): Route {
  return {
    id: 'route-id',
    model_id: 'route',
    display_name: null,
    balance: 'traffic_equalization',
    target_provider: 'provider-a',
    target_model: 'model-a',
    is_enabled: true,
    created_at: '',
    supported_thinking_levels: [],
    targets: [
      {
        id: 'target-b',
        model_id: 'route-id',
        provider_id: 'provider-b',
        model: 'model-b',
        priority: 100_000,
        first_token_timeout_ms: 90_000,
        target_retry_budget: 2,
        target_cooldown_ms: 180_000,
        created_at: '',
        thinking_level_map: [],
      },
      {
        id: 'target-a',
        model_id: 'route-id',
        provider_id: 'provider-a',
        model: 'model-a',
        priority: 0,
        first_token_timeout_ms: 60_000,
        target_retry_budget: 5,
        target_cooldown_ms: 120_000,
        created_at: '',
        thinking_level_map: [],
      },
    ],
  }
}

describe('route targets form', () => {
  test('restores explicit target controls without deriving priority from list order', () => {
    const targets = createRouteTargetForms(routeWithTargets(), '', '')
    const added = createRouteTarget(targets)

    expect(targets.map((target) => target.id)).toEqual(['target-b', 'target-a'])
    expect(targets[0]).toMatchObject({
      priority: 100_000,
      firstTokenTimeoutMs: 90_000,
      targetRetryBudget: 2,
      targetCooldownMs: 180_000,
    })
    expect(targets.map((target) => target.key)).toEqual(['target-1', 'target-2'])
    expect(added.key).toBe('target-3')
    expect(added).toMatchObject({
      priority: 0,
      firstTokenTimeoutMs: 60_000,
      targetRetryBudget: 5,
      targetCooldownMs: 120_000,
    })
  })

  test('submits explicit controls and only submits explicit new thinking overrides', () => {
    const targets = createRouteTargetForms(undefined, 'provider-a', ' model-a ')
    targets[0].priority = 100_000
    targets[0].thinkingLevelMap = [
      { level: 'low', control: { type: 'effort', value: 'low' }, source: 'generated' },
      { level: 'high', control: { type: 'effort', value: 'high' }, source: 'overridden' },
    ]

    const result = buildRouteTargets(targets)

    expect(result.error).toBeUndefined()
    expect(result.targets).toEqual([
      {
        id: undefined,
        provider_id: 'provider-a',
        model: 'model-a',
        priority: 100_000,
        first_token_timeout_ms: 60_000,
        target_retry_budget: 5,
        target_cooldown_ms: 120_000,
        thinking_level_map: [{ level: 'high', control: { type: 'effort', value: 'high' }, source: 'overridden' }],
      },
    ])
  })

  test('rejects out-of-range priority and negative failure controls', () => {
    const targets = createRouteTargetForms(undefined, 'provider-a', 'model-a')
    targets[0].priority = 100_001
    expect(buildRouteTargets(targets).error).toBe('invalid-priority')
    targets[0].priority = 0
    targets[0].targetRetryBudget = -1
    expect(buildRouteTargets(targets).error).toBe('invalid-retry-budget')
  })
})

import { describe, expect, test } from 'bun:test'

import { buildRouteTargets, createRouteTarget, createRouteTargetForms } from '../src/lib/components/route-targets-form'
import type { Route } from '../src/lib/types'

function routeWithTargets(): Route {
  return {
    id: 'route-id',
    model_id: 'route',
    display_name: null,
    balance: 'priority',
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
        weight: 20,
        priority: 2,
        created_at: '',
        thinking_level_map: [],
      },
      {
        id: 'target-a',
        model_id: 'route-id',
        provider_id: 'provider-a',
        model: 'model-a',
        weight: 80,
        priority: 1,
        created_at: '',
        thinking_level_map: [],
      },
    ],
  }
}

describe('route targets form', () => {
  test('restores targets in priority order with stable unique keys', () => {
    const targets = createRouteTargetForms(routeWithTargets(), '', '')
    const added = createRouteTarget(targets)

    expect(targets.map((target) => target.id)).toEqual(['target-a', 'target-b'])
    expect(targets.map((target) => target.key)).toEqual(['target-1', 'target-2'])
    expect(added.key).toBe('target-3')
  })

  test('normalizes non-weighted targets and only submits explicit new thinking overrides', () => {
    const targets = createRouteTargetForms(undefined, 'provider-a', ' model-a ')
    targets[0].thinkingLevelMap = [
      { level: 'low', control: { type: 'effort', value: 'low' }, source: 'generated' },
      { level: 'high', control: { type: 'effort', value: 'high' }, source: 'overridden' },
    ]

    const result = buildRouteTargets('priority', targets)

    expect(result.error).toBeUndefined()
    expect(result.targets).toEqual([
      {
        id: undefined,
        provider_id: 'provider-a',
        model: 'model-a',
        weight: 100,
        priority: 1,
        thinking_level_map: [{ level: 'high', control: { type: 'effort', value: 'high' }, source: 'overridden' }],
      },
    ])
  })

  test('rejects non-positive weighted targets', () => {
    const targets = createRouteTargetForms(undefined, 'provider-a', 'model-a')
    targets[0].weight = 0

    expect(buildRouteTargets('weighted', targets).error).toBe('invalid-weight')
  })
})

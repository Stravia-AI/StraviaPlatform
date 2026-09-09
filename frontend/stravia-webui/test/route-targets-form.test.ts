import { describe, expect, test } from 'bun:test'

import {
  buildRouteTargets,
  createRouteTarget,
  createRouteTargetForms,
  millisecondsToSeconds,
  moveRouteTargetToDock,
  moveRouteTargetToInsertion,
  moveRouteTargetToLane,
  planRouteTargetInsertion,
  priorityLanes,
  reorderRouteTargetBefore,
  secondsToMilliseconds,
} from '../src/lib/components/route-targets-form'
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
        enabled: true,
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
        enabled: false,
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
  test('restores enabled state and displays failure durations in seconds', () => {
    const targets = createRouteTargetForms(routeWithTargets(), '', '')
    const added = createRouteTarget(targets)

    expect(targets.map((target) => target.id)).toEqual(['target-b', 'target-a'])
    expect(targets[0]).toMatchObject({
      priority: 100_000,
      enabled: true,
      firstTokenTimeoutSeconds: 90,
      targetRetryBudget: 2,
      targetCooldownSeconds: 180,
    })
    expect(targets.map((target) => target.key)).toEqual(['target-1', 'target-2'])
    expect(added.key).toBe('target-3')
    expect(added).toMatchObject({
      enabled: false,
      priority: 0,
      firstTokenTimeoutSeconds: 60,
      targetRetryBudget: 5,
      targetCooldownSeconds: 120,
    })
  })

  test('submits enabled state and converts seconds to integer milliseconds', () => {
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
        enabled: true,
        priority: 100_000,
        first_token_timeout_ms: 60_000,
        target_retry_budget: 5,
        target_cooldown_ms: 120_000,
        thinking_level_map: [{ level: 'high', control: { type: 'effort', value: 'high' }, source: 'overridden' }],
      },
    ])
  })

  test('groups enabled targets by descending signed priority', () => {
    const targets = createRouteTargetForms(routeWithTargets(), '', '')
    const peer = createRouteTarget(targets, {
      providerId: 'provider-c',
      model: 'model-c',
      enabled: true,
      priority: 100_000,
    })
    targets.push(peer)

    expect(
      priorityLanes(targets).map((lane) => ({
        priority: lane.priority,
        keys: lane.targets.map((target) => target.key),
      })),
    ).toEqual([{ priority: 100_000, keys: ['target-1', 'target-3'] }])
  })

  test('keeps same-lane layout order out of the write payload', () => {
    const targets = createRouteTargetForms(routeWithTargets(), '', '')
    targets[1].enabled = true
    targets[1].priority = targets[0].priority

    expect(reorderRouteTargetBefore(targets, 'target-2', 'target-1')).toBe(true)
    expect(priorityLanes(targets)[0].targets.map((target) => target.key)).toEqual(['target-2', 'target-1'])
    expect(buildRouteTargets(targets).targets.map((target) => target.id)).toEqual(['target-b', 'target-a'])
  })

  test('moves targets into lanes and creates top and bottom priorities', () => {
    const targets = createRouteTargetForms(routeWithTargets(), '', '')
    expect(moveRouteTargetToLane(targets, 'target-2', 100_000)).toBe(true)
    expect(targets[1]).toMatchObject({ enabled: true, priority: 100_000 })

    targets[1].enabled = false
    expect(planRouteTargetInsertion(targets, 'target-2', { position: 'top' })?.priority).toBe(100_001)
    expect(moveRouteTargetToInsertion(targets, 'target-2', { position: 'top' })).toBe(true)
    expect(targets[1]).toMatchObject({ enabled: true, priority: 100_001 })
    expect(targets[0].priority).toBe(100_000)

    expect(moveRouteTargetToInsertion(targets, 'target-2', { position: 'bottom' })).toBe(true)
    expect(targets[1].priority).toBe(99_999)
    expect(targets[0].priority).toBe(100_000)
  })

  test('inserts a target between lanes using an available priority', () => {
    const targets = [createRouteTarget([], { providerId: 'provider-a', model: 'model-a', enabled: true, priority: 10 })]
    targets.push(createRouteTarget(targets, { providerId: 'provider-b', model: 'model-b', enabled: true, priority: 0 }))
    targets.push(
      createRouteTarget(targets, { providerId: 'provider-c', model: 'model-c', enabled: false, priority: -20 }),
    )
    const insertion = { position: 'between' as const, upperPriority: 10, lowerPriority: 0 }

    expect(planRouteTargetInsertion(targets, 'target-3', insertion)?.priority).toBe(5)
    expect(moveRouteTargetToInsertion(targets, 'target-3', insertion)).toBe(true)
    expect(targets.map((target) => target.priority)).toEqual([10, 0, 5])
  })

  test('opens space between adjacent lanes without changing their order', () => {
    const targets = [createRouteTarget([], { providerId: 'provider-a', model: 'model-a', enabled: true, priority: 1 })]
    targets.push(createRouteTarget(targets, { providerId: 'provider-b', model: 'model-b', enabled: true, priority: 0 }))
    targets.push(
      createRouteTarget(targets, { providerId: 'provider-c', model: 'model-c', enabled: true, priority: -4 }),
    )
    targets.push(
      createRouteTarget(targets, { providerId: 'provider-d', model: 'model-d', enabled: false, priority: 12 }),
    )
    const insertion = { position: 'between' as const, upperPriority: 1, lowerPriority: 0 }

    expect(planRouteTargetInsertion(targets, 'target-4', insertion)?.priority).toBe(0)
    expect(moveRouteTargetToInsertion(targets, 'target-4', insertion)).toBe(true)
    expect(targets.map((target) => target.priority)).toEqual([1, -1, -5, 0])
    expect(priorityLanes(targets).map((lane) => lane.priority)).toEqual([1, 0, -1, -5])
  })

  test('uses saved priority for the first lane and protects the last enabled target', () => {
    const targets = createRouteTargetForms(routeWithTargets(), '', '')
    targets[0].enabled = false
    targets[1].priority = -7

    expect(moveRouteTargetToInsertion(targets, 'target-2', { position: 'top' })).toBe(true)
    expect(targets[1]).toMatchObject({ enabled: true, priority: -7 })
    expect(moveRouteTargetToDock(targets, 'target-2')).toBe(false)
    expect(targets[1].enabled).toBe(true)
  })

  test('rejects incomplete activation and i32 edge overflow', () => {
    const targets = [
      createRouteTarget([], { providerId: 'provider-a', model: 'model-a', enabled: true, priority: i32Max }),
    ]
    targets.push(createRouteTarget(targets, { enabled: false }))
    expect(moveRouteTargetToLane(targets, 'target-2', 0)).toBe(false)
    targets[1].providerId = 'provider-b'
    targets[1].model = 'model-b'
    expect(moveRouteTargetToInsertion(targets, 'target-2', { position: 'top' })).toBe(false)
  })

  test('converts seconds and milliseconds with millisecond precision', () => {
    expect(millisecondsToSeconds(60_000)).toBe(60)
    expect(secondsToMilliseconds(0.5)).toBe(500)
    expect(secondsToMilliseconds(0)).toBe(0)
    expect(secondsToMilliseconds(1.234)).toBe(1234)
    expect(secondsToMilliseconds(1.2344)).toBeUndefined()
  })

  test('accepts signed i32 priorities and rejects invalid controls', () => {
    const targets = createRouteTargetForms(undefined, 'provider-a', 'model-a')
    targets[0].priority = -1
    expect(buildRouteTargets(targets).error).toBeUndefined()
    targets[0].priority = i32Min
    expect(buildRouteTargets(targets).error).toBeUndefined()
    targets[0].priority = i32Max
    expect(buildRouteTargets(targets).error).toBeUndefined()
    targets[0].priority = i32Max + 1
    expect(buildRouteTargets(targets).error).toBe('invalid-priority')
    targets[0].priority = 0
    targets[0].targetRetryBudget = -1
    expect(buildRouteTargets(targets).error).toBe('invalid-retry-budget')
  })

  test('requires at least one enabled complete target', () => {
    const targets = createRouteTargetForms(undefined, 'provider-a', 'model-a')
    targets[0].enabled = false
    expect(buildRouteTargets(targets).error).toBe('no-enabled-target')
  })
})

const i32Min = -2_147_483_648
const i32Max = 2_147_483_647

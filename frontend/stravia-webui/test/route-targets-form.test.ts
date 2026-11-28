import { describe, expect, test } from 'bun:test'

import {
  buildRouteTargets,
  checkGroupThinkingCompatibility,
  commonGroupEffortSupport,
  createRouteTarget,
  createRouteTargetForms,
  groupLaneTargetsByModel,
  inheritGroupConfigForNewTarget,
  mergeLaneIntoUpper,
  millisecondsToSeconds,
  moveModelGroupToDock,
  moveModelGroupToInsertion,
  moveModelGroupToLane,
  moveRouteTargetToDock,
  moveRouteTargetToInsertion,
  moveRouteTargetToLane,
  moveTargetToLaneWithInheritance,
  planRouteTargetInsertion,
  priorityLanes,
  reorderRouteTargetBefore,
  routeSupportedThinkingLevels,
  secondsToMilliseconds,
  setTargetConfigMode,
  stepModelGroup,
  stepTargetSplitMerge,
  summarizeTargetGroupConfig,
  swapAdjacentLanes,
  THINKING_LEVELS,
  updateModelGroupConfig,
  type RouteTargetForm,
} from '../src/lib/components/route-targets-form'
import type { Route, ThinkingLevel, ThinkingLevelMapping } from '../src/lib/types'

function routeWithTargets(): Route {
  return {
    id: 'route-id',
    purpose: 'inference',
    image_generation_bound: false,
    web_search_bound: false,
    model_id: 'route',
    display_name: null,
    balance: 'traffic_equalization',
    target_provider: 'provider-a',
    target_model: 'model-a',
    is_enabled: true,
    created_at: '',
    supported_thinking_levels: [],
    supports_remote_compaction: false,
    targets: [
      {
        id: 'target-b',
        purpose_mismatch: false,
        route_purposes: ['inference'],
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
        purpose_mismatch: false,
        route_purposes: ['inference'],
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
      firstTokenTimeoutSeconds: 120,
      targetRetryBudget: 5,
      targetCooldownSeconds: 120,
    })
  })

  test('creating a target does not freeze generated thinking defaults', () => {
    const target = createRouteTarget([], {
      providerId: 'provider-a',
      model: 'model-a',
      enabled: true,
      thinkingLevelMap: [
        { level: 'medium', control: { type: 'effort', value: 'medium' }, source: 'generated' },
        { level: 'high', control: { type: 'effort', value: 'high' }, source: 'overridden' },
      ],
    })
    expect(buildRouteTargets([target]).targets[0].thinking_level_map?.map((row) => row.level)).toEqual(['high'])
    target.persisted = true
    expect(buildRouteTargets([target]).targets[0].thinking_level_map?.map((row) => row.level)).toEqual([
      'medium',
      'high',
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
    const payload = buildRouteTargets(targets).targets
    expect(payload.map((target) => target.provider_id)).toEqual(['provider-b', 'provider-a'])
    expect(payload.every((target) => !('id' in target))).toBe(true)
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

  test('supported thinking levels are the union across enabled targets in canonical order', () => {
    expect(routeSupportedThinkingLevels([])).toEqual([])

    const map = (hidden: ThinkingLevel[]): ThinkingLevelMapping[] =>
      THINKING_LEVELS.map((level) => ({
        level,
        control: hidden.includes(level) ? { type: 'hidden' } : { type: 'effort', value: level },
        source: 'generated',
      }))
    const targets = [
      createRouteTarget([], {
        providerId: 'provider-a',
        model: 'model-a',
        enabled: true,
        thinkingLevelMap: map(['max']),
      }),
    ]
    targets.push(
      createRouteTarget(targets, {
        providerId: 'provider-b',
        model: 'model-b',
        enabled: true,
        thinkingLevelMap: map(['xhigh', 'max']),
      }),
    )

    // 'xhigh' is offered by provider-a alone; 'max' by neither.
    expect(routeSupportedThinkingLevels(targets)).toEqual(['off', 'minimal', 'low', 'medium', 'high', 'xhigh'])

    targets[0].enabled = false
    expect(routeSupportedThinkingLevels(targets)).toEqual(['off', 'minimal', 'low', 'medium', 'high'])

    targets[1].enabled = false
    expect(routeSupportedThinkingLevels(targets)).toEqual([])
  })

  test('groups targets by model, updates shared members while protecting custom, and supports overwrite', () => {
    const map = (value: string): ThinkingLevelMapping[] =>
      THINKING_LEVELS.map((level) => ({
        level,
        control: level === 'medium' ? { type: 'effort', value } : { type: 'hidden' },
        source: 'overridden',
      }))

    const targets: RouteTargetForm[] = []
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p1',
        model: 'gemini-4-neon',
        enabled: true,
        priority: 10,
        firstTokenTimeoutSeconds: 300,
        targetRetryBudget: 5,
        targetCooldownSeconds: 120,
        configMode: 'shared',
        thinkingLevelMap: map('medium'),
      }),
    )
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p2',
        model: 'gemini-4-neon',
        enabled: true,
        priority: 10,
        firstTokenTimeoutSeconds: 300,
        targetRetryBudget: 5,
        targetCooldownSeconds: 120,
        configMode: 'shared',
        thinkingLevelMap: map('medium'),
      }),
    )
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p3',
        model: 'gemini-4-neon',
        enabled: true,
        priority: 10,
        firstTokenTimeoutSeconds: 60,
        targetRetryBudget: 3,
        targetCooldownSeconds: 90,
        configMode: 'custom',
        thinkingLevelMap: map('low'),
      }),
    )
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p4',
        model: 'claude-4-6-opus',
        enabled: true,
        priority: 10,
        configMode: 'shared',
      }),
    )

    const groups = groupLaneTargetsByModel(targets, { enabled: true, priority: 10 })
    expect(groups.map((group) => [group.model, group.sharedTargets.length, group.customTargets.length])).toEqual([
      ['gemini-4-neon', 2, 1],
      ['claude-4-6-opus', 1, 0],
    ])
    expect(summarizeTargetGroupConfig(groups[0].sharedConfig)).toEqual({
      timeoutSeconds: 300,
      retryBudget: 5,
      cooldownSeconds: 120,
      enabledThinkingLevels: ['medium'],
    })

    const nextConfig = {
      firstTokenTimeoutSeconds: 180,
      targetRetryBudget: 4,
      targetCooldownSeconds: 60,
      thinkingLevelMap: map('high'),
    }
    const partial = updateModelGroupConfig(targets, { enabled: true, priority: 10 }, 'gemini-4-neon', nextConfig)
    expect(partial).toEqual({ updatedCount: 2, overwrittenCustomCount: 0 })
    expect(targets[0].firstTokenTimeoutSeconds).toBe(180)
    expect(targets[1].firstTokenTimeoutSeconds).toBe(180)
    expect(targets[2].firstTokenTimeoutSeconds).toBe(60)
    expect(targets[2].configMode).toBe('custom')

    const syncedBack = setTargetConfigMode(targets, targets[2].key, 'shared')
    expect(syncedBack).toEqual({ ok: true, syncedFromGroup: true })
    expect(targets[2].firstTokenTimeoutSeconds).toBe(180)
    expect(targets[2].targetRetryBudget).toBe(4)

    targets[2].configMode = 'custom'
    targets[2].firstTokenTimeoutSeconds = 45
    const overwrite = updateModelGroupConfig(
      targets,
      { enabled: true, priority: 10 },
      'gemini-4-neon',
      { ...nextConfig, firstTokenTimeoutSeconds: 240 },
      { overwriteCustom: true },
    )
    expect(overwrite).toEqual({ updatedCount: 3, overwrittenCustomCount: 1 })
    expect(targets.map((target) => target.configMode)).toEqual(['shared', 'shared', 'shared', 'shared'])
    expect(targets[2].firstTokenTimeoutSeconds).toBe(240)

    const added = createRouteTarget(targets, {
      providerId: 'p5',
      model: 'gemini-4-neon',
      enabled: true,
      priority: 10,
    })
    expect(inheritGroupConfigForNewTarget(targets, added, { enabled: true, priority: 10 })).toBe(true)
    expect(added.firstTokenTimeoutSeconds).toBe(240)
    expect(added.configMode).toBe('shared')

    const payload = buildRouteTargets(targets)
    expect(payload.error).toBeUndefined()
    expect(payload.targets.map((target) => target.config_mode)).toEqual(['shared', 'shared', 'shared', 'shared'])
  })

  test('layer swap, merge into upper, split-merge step, and whole-group moves preserve invariants', () => {
    const targets: RouteTargetForm[] = []
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p1',
        model: 'model-a',
        enabled: true,
        priority: 2,
        firstTokenTimeoutSeconds: 300,
        configMode: 'shared',
      }),
    )
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p2',
        model: 'model-b',
        enabled: true,
        priority: 2,
        firstTokenTimeoutSeconds: 200,
        configMode: 'shared',
      }),
    )
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p3',
        model: 'model-a',
        enabled: true,
        priority: 1,
        firstTokenTimeoutSeconds: 90,
        configMode: 'shared',
      }),
    )
    targets.push(
      createRouteTarget(targets, {
        providerId: 'p4',
        model: 'model-a',
        enabled: true,
        priority: 1,
        firstTokenTimeoutSeconds: 45,
        configMode: 'custom',
      }),
    )

    // Split p2 down out of lane 2, then merge it back up: strictly reversible.
    const split = stepTargetSplitMerge(targets, targets[1].key, 'down')
    expect(split.ok).toBe(true)
    expect(split.action).toBe('split-down')
    expect(priorityLanes(targets).length).toBe(3)

    const mergedBack = stepTargetSplitMerge(targets, targets[1].key, 'up')
    expect(mergedBack.ok).toBe(true)
    expect(mergedBack.action).toBe('merge-up')
    expect(priorityLanes(targets).length).toBe(2)

    // Swap top & bottom lanes.
    const lanesBefore = priorityLanes(targets)
    expect(swapAdjacentLanes(targets, lanesBefore[0].priority, lanesBefore[1].priority)).toBe(true)
    expect(priorityLanes(targets)[0].targets.map((t) => t.providerId)).toEqual(['p3', 'p4'])

    // Swap back, then merge lower lane into upper: shared p3 inherits upper model-a's 300s, custom p4 keeps 45s!
    const lanesSwapped = priorityLanes(targets)
    expect(swapAdjacentLanes(targets, lanesSwapped[0].priority, lanesSwapped[1].priority)).toBe(true)
    const lanesRestored = priorityLanes(targets)
    const merged = mergeLaneIntoUpper(targets, lanesRestored[1].priority, lanesRestored[0].priority)
    expect(merged).toEqual({ moved: 2, syncedShared: 1 })
    expect(targets[2].firstTokenTimeoutSeconds).toBe(300)
    expect(targets[3].firstTokenTimeoutSeconds).toBe(45)
    expect(targets[3].configMode).toBe('custom')

    // Whole model-a group steps down into its own new lane, keeping custom p4 intact.
    const groupKeys = [targets[0].key, targets[2].key, targets[3].key]
    const groupStep = stepModelGroup(targets, groupKeys, 'down')
    expect(groupStep.ok).toBe(true)
    expect(groupStep.action).toBe('split-down')
    expect(priorityLanes(targets).length).toBe(2)
    expect(targets[3].firstTokenTimeoutSeconds).toBe(45)

    // Whole group move into top lane & insertion & dock protection.
    const topPriority = priorityLanes(targets)[0].priority
    expect(moveModelGroupToLane(targets, groupKeys, topPriority).ok).toBe(true)
    expect(moveModelGroupToInsertion(targets, groupKeys, { position: 'bottom' })).toBe(true)
    expect(moveModelGroupToDock(targets, groupKeys)).toBe(true)
    // Cannot dock the last remaining enabled target p2.
    expect(moveModelGroupToDock(targets, [targets[1].key])).toBe(false)

    // Single target lane move inherits destination shared peer config.
    const singleMove = moveTargetToLaneWithInheritance(targets, targets[2].key, topPriority)
    expect(singleMove).toEqual({ ok: true, inheritedFromGroup: false })
  })

  test('checks cross-member thinking effort intersection and compatibility warnings', () => {
    const members = [
      {
        label: 'Account A',
        context: { protocol: 'anthropic-messages', reasoningEfforts: ['low', 'medium', 'high', 'max'] },
      },
      {
        label: 'Account B',
        context: { protocol: 'anthropic-messages', reasoningEfforts: ['low', 'medium', 'high'] },
      },
    ]
    const support = commonGroupEffortSupport(members)
    expect(support.common).toEqual(['low', 'medium', 'high'])
    expect(support.differences).toEqual([{ label: 'Account B', missing: ['max'] }])

    const issues = checkGroupThinkingCompatibility(members, [
      { level: 'high', control: { type: 'effort', value: 'high' }, source: 'generated' },
      { level: 'max', control: { type: 'effort', value: 'max' }, source: 'overridden' },
    ])
    expect(issues).toEqual([{ level: 'max', effort: 'max', incompatibleMembers: ['Account B'] }])
  })
})

const i32Min = -2_147_483_648
const i32Max = 2_147_483_647

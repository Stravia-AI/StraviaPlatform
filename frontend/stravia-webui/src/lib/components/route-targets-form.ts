import type {
  CreateTarget,
  ModelCapabilities,
  ProviderModelSummary,
  Route,
  ThinkingLevelMapping,
  UpsertTarget,
} from '$lib/types'

export interface RouteTargetForm {
  key: string
  id?: string
  providerId: string
  model: string
  enabled: boolean
  priority: number
  firstTokenTimeoutSeconds: number
  targetRetryBudget: number
  targetCooldownSeconds: number
  inventory: ProviderModelSummary[]
  capabilities?: ModelCapabilities
  custom: boolean
  persisted: boolean
  loading: boolean
  validationError: string
  thinkingLevelMap: ThinkingLevelMapping[]
}

export type RouteTargetsValidationError =
  | 'invalid-priority'
  | 'invalid-first-token-timeout'
  | 'invalid-retry-budget'
  | 'invalid-cooldown'
  | 'incomplete-target'
  | 'no-enabled-target'

export interface RouteTargetLane {
  priority: number
  targets: RouteTargetForm[]
}

export type RouteTargetEdge = 'top' | 'bottom'

const i32Minimum = -2_147_483_648
const i32Maximum = 2_147_483_647

function nextTargetKey(targets: RouteTargetForm[]): string {
  const next =
    targets.reduce((maximum, target) => {
      const match = /^target-(\d+)$/.exec(target.key)
      return Math.max(maximum, match ? Number(match[1]) : 0)
    }, 0) + 1
  return `target-${next}`
}

function targetKeyOrder(target: RouteTargetForm): number {
  return Number(/^target-(\d+)$/.exec(target.key)?.[1] ?? Number.MAX_SAFE_INTEGER)
}

export function createRouteTarget(targets: RouteTargetForm[], values: Partial<RouteTargetForm> = {}): RouteTargetForm {
  return {
    key: nextTargetKey(targets),
    providerId: '',
    model: '',
    enabled: false,
    priority: 0,
    firstTokenTimeoutSeconds: 60,
    targetRetryBudget: 5,
    targetCooldownSeconds: 120,
    inventory: [],
    custom: false,
    persisted: false,
    loading: false,
    validationError: '',
    thinkingLevelMap: [],
    ...values,
  }
}

export function createRouteTargetForms(
  route: Route | undefined,
  initialProviderId: string,
  initialModelId: string,
): RouteTargetForm[] {
  const targets: RouteTargetForm[] = []
  if (route?.targets.length) {
    for (const target of route.targets) {
      targets.push(
        createRouteTarget(targets, {
          id: target.id,
          providerId: target.provider_id,
          model: target.model,
          enabled: target.enabled ?? true,
          priority: target.priority,
          firstTokenTimeoutSeconds: millisecondsToSeconds(target.first_token_timeout_ms),
          targetRetryBudget: target.target_retry_budget,
          targetCooldownSeconds: millisecondsToSeconds(target.target_cooldown_ms),
          persisted: true,
          thinkingLevelMap: target.thinking_level_map?.map((row) => ({ ...row, control: { ...row.control } })) ?? [],
        }),
      )
    }
    return targets
  }
  if (!initialProviderId || !initialModelId.trim()) return targets
  return [createRouteTarget(targets, { providerId: initialProviderId, model: initialModelId, enabled: true })]
}

export function addRouteTarget(targets: RouteTargetForm[]): RouteTargetForm {
  const target = createRouteTarget(targets)
  targets.push(target)
  return target
}

export function removeRouteTarget(targets: RouteTargetForm[], index: number): boolean {
  const target = targets[index]
  if (!target) return false
  if (target.enabled && targets.filter((candidate) => candidate.enabled).length === 1) return false
  targets.splice(index, 1)
  return true
}

function validInteger(value: number, minimum = 0, maximum?: number): boolean {
  const number = Number(value)
  return Number.isInteger(number) && number >= minimum && (maximum === undefined || number <= maximum)
}

function validSeconds(value: number): boolean {
  const number = Number(value)
  const milliseconds = Math.round(number * 1000)
  return (
    Number.isFinite(number) &&
    number >= 0 &&
    Number.isSafeInteger(milliseconds) &&
    Math.abs(number * 1000 - milliseconds) < 1e-6
  )
}

function completeTarget(target: RouteTargetForm): boolean {
  return Boolean(target.providerId && target.model.trim())
}

export function millisecondsToSeconds(milliseconds: number): number {
  return milliseconds / 1000
}

export function secondsToMilliseconds(seconds: number): number | undefined {
  if (!validSeconds(seconds)) return undefined
  return Math.round(Number(seconds) * 1000)
}

export function priorityLanes(targets: RouteTargetForm[]): RouteTargetLane[] {
  const lanes = new Map<number, RouteTargetForm[]>()
  for (const target of targets) {
    if (!target.enabled) continue
    const lane = lanes.get(target.priority)
    if (lane) lane.push(target)
    else lanes.set(target.priority, [target])
  }
  return [...lanes.entries()]
    .sort(([left], [right]) => right - left)
    .map(([priority, laneTargets]) => ({ priority, targets: laneTargets }))
}

export function moveRouteTargetToLane(targets: RouteTargetForm[], key: string, priority: number): boolean {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target || !completeTarget(target) || !validInteger(priority, i32Minimum, i32Maximum)) return false
  target.enabled = true
  target.priority = priority
  return true
}

export function moveRouteTargetToEdge(targets: RouteTargetForm[], key: string, edge: RouteTargetEdge): boolean {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target || !completeTarget(target)) return false
  const enabledPriorities = targets.filter((candidate) => candidate.enabled).map((candidate) => candidate.priority)
  if (enabledPriorities.length === 0) {
    target.enabled = true
    return true
  }
  const edgePriority = edge === 'top' ? Math.max(...enabledPriorities) : Math.min(...enabledPriorities)
  if ((edge === 'top' && edgePriority === i32Maximum) || (edge === 'bottom' && edgePriority === i32Minimum))
    return false
  target.enabled = true
  target.priority = edge === 'top' ? edgePriority + 1 : edgePriority - 1
  return true
}

export function moveRouteTargetToDock(targets: RouteTargetForm[], key: string): boolean {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return false
  if (!target.enabled) return true
  if (targets.filter((candidate) => candidate.enabled).length === 1) return false
  target.enabled = false
  return true
}

export function reorderRouteTargetBefore(targets: RouteTargetForm[], key: string, beforeKey: string): boolean {
  if (key === beforeKey) return false
  const from = targets.findIndex((target) => target.key === key)
  const before = targets.findIndex((target) => target.key === beforeKey)
  if (from < 0 || before < 0) return false
  const [target] = targets.splice(from, 1)
  const destination = targets.findIndex((candidate) => candidate.key === beforeKey)
  targets.splice(destination, 0, target)
  return true
}

export function buildRouteTargets(targets: RouteTargetForm[]): {
  targets: Array<CreateTarget & UpsertTarget>
  error?: RouteTargetsValidationError
} {
  if (targets.some((target) => !validInteger(target.priority, i32Minimum, i32Maximum)))
    return { targets: [], error: 'invalid-priority' }
  if (targets.some((target) => !validSeconds(target.firstTokenTimeoutSeconds)))
    return { targets: [], error: 'invalid-first-token-timeout' }
  if (targets.some((target) => !validInteger(target.targetRetryBudget)))
    return { targets: [], error: 'invalid-retry-budget' }
  if (targets.some((target) => !validSeconds(target.targetCooldownSeconds)))
    return { targets: [], error: 'invalid-cooldown' }
  if (!targets.some((target) => target.enabled)) return { targets: [], error: 'no-enabled-target' }

  const cleanTargets = [...targets]
    .sort((left, right) => targetKeyOrder(left) - targetKeyOrder(right))
    .filter((target) => target.providerId && target.model.trim())
    .map((target): CreateTarget & UpsertTarget => ({
      id: target.id,
      provider_id: target.providerId,
      model: target.model.trim(),
      enabled: target.enabled,
      priority: Number(target.priority),
      first_token_timeout_ms: secondsToMilliseconds(target.firstTokenTimeoutSeconds),
      target_retry_budget: Number(target.targetRetryBudget),
      target_cooldown_ms: secondsToMilliseconds(target.targetCooldownSeconds),
      thinking_level_map: target.persisted
        ? target.thinkingLevelMap
        : target.thinkingLevelMap.filter((row) => row.source === 'overridden'),
    }))

  return cleanTargets.length === targets.length && cleanTargets.length > 0
    ? { targets: cleanTargets }
    : { targets: cleanTargets, error: 'incomplete-target' }
}

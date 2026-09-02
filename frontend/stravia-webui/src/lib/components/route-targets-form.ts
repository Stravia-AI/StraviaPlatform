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
  priority: number
  firstTokenTimeoutMs: number
  targetRetryBudget: number
  targetCooldownMs: number
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

function nextTargetKey(targets: RouteTargetForm[]): string {
  const next = targets.reduce((maximum, target) => {
    const match = /^target-(\d+)$/.exec(target.key)
    return Math.max(maximum, match ? Number(match[1]) : 0)
  }, 0) + 1
  return `target-${next}`
}

export function createRouteTarget(
  targets: RouteTargetForm[],
  values: Partial<RouteTargetForm> = {},
): RouteTargetForm {
  return {
    key: nextTargetKey(targets),
    providerId: '',
    model: '',
    priority: 0,
    firstTokenTimeoutMs: 60_000,
    targetRetryBudget: 5,
    targetCooldownMs: 120_000,
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
      targets.push(createRouteTarget(targets, {
        id: target.id,
        providerId: target.provider_id,
        model: target.model,
        priority: target.priority,
        firstTokenTimeoutMs: target.first_token_timeout_ms,
        targetRetryBudget: target.target_retry_budget,
        targetCooldownMs: target.target_cooldown_ms,
        persisted: true,
        thinkingLevelMap: target.thinking_level_map?.map((row) => ({
          ...row,
          control: { ...row.control },
        })) ?? [],
      }))
    }
    return targets
  }
  return [createRouteTarget(targets, { providerId: initialProviderId, model: initialModelId })]
}

export function addRouteTarget(targets: RouteTargetForm[]): void {
  targets.push(createRouteTarget(targets))
}

export function removeRouteTarget(targets: RouteTargetForm[], index: number): void {
  if (targets.length === 1) return
  targets.splice(index, 1)
}

function validInteger(value: number, maximum?: number): boolean {
  const number = Number(value)
  return Number.isInteger(number) && number >= 0 && (maximum === undefined || number <= maximum)
}

export function buildRouteTargets(
  targets: RouteTargetForm[],
): { targets: Array<CreateTarget & UpsertTarget>; error?: RouteTargetsValidationError } {
  if (targets.some((target) => !validInteger(target.priority, 100_000)))
    return { targets: [], error: 'invalid-priority' }
  if (targets.some((target) => !validInteger(target.firstTokenTimeoutMs)))
    return { targets: [], error: 'invalid-first-token-timeout' }
  if (targets.some((target) => !validInteger(target.targetRetryBudget)))
    return { targets: [], error: 'invalid-retry-budget' }
  if (targets.some((target) => !validInteger(target.targetCooldownMs)))
    return { targets: [], error: 'invalid-cooldown' }

  const cleanTargets = targets
    .filter((target) => target.providerId && target.model.trim())
    .map((target): CreateTarget & UpsertTarget => ({
      id: target.id,
      provider_id: target.providerId,
      model: target.model.trim(),
      priority: Number(target.priority),
      first_token_timeout_ms: Number(target.firstTokenTimeoutMs),
      target_retry_budget: Number(target.targetRetryBudget),
      target_cooldown_ms: Number(target.targetCooldownMs),
      thinking_level_map: target.persisted
        ? target.thinkingLevelMap
        : target.thinkingLevelMap.filter((row) => row.source === 'overridden'),
    }))

  return cleanTargets.length === targets.length && cleanTargets.length > 0
    ? { targets: cleanTargets }
    : { targets: cleanTargets, error: 'incomplete-target' }
}

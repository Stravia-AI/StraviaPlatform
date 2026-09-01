import type {
  CreateTarget,
  ModelCapabilities,
  ProviderModelSummary,
  Route,
  RouteSelectionStrategy,
  ThinkingLevelMapping,
  UpsertTarget,
} from '$lib/types'

export interface RouteTargetForm {
  key: string
  id?: string
  providerId: string
  model: string
  weight: number
  inventory: ProviderModelSummary[]
  capabilities?: ModelCapabilities
  custom: boolean
  persisted: boolean
  loading: boolean
  validationError: string
  thinkingLevelMap: ThinkingLevelMapping[]
}

export type RouteTargetsValidationError = 'invalid-weight' | 'incomplete-target'

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
    weight: 100,
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
    for (const target of route.targets.slice().sort((left, right) => left.priority - right.priority)) {
      targets.push(createRouteTarget(targets, {
        id: target.id,
        providerId: target.provider_id,
        model: target.model,
        weight: target.weight,
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

export function moveRouteTarget(targets: RouteTargetForm[], index: number, offset: -1 | 1): void {
  const destination = index + offset
  if (destination < 0 || destination >= targets.length) return
  const [target] = targets.splice(index, 1)
  targets.splice(destination, 0, target)
}

export function validRouteTargetWeight(value: number): boolean {
  const number = Number(value)
  return Number.isInteger(number) && number > 0
}

export function buildRouteTargets(
  strategy: RouteSelectionStrategy,
  targets: RouteTargetForm[],
): { targets: Array<CreateTarget & UpsertTarget>; error?: RouteTargetsValidationError } {
  if (strategy === 'weighted' && targets.some((target) => !validRouteTargetWeight(target.weight))) {
    return { targets: [], error: 'invalid-weight' }
  }

  const cleanTargets = targets
    .filter((target) => target.providerId && target.model.trim())
    .map((target, index): CreateTarget & UpsertTarget => ({
      id: target.id,
      provider_id: target.providerId,
      model: target.model.trim(),
      weight: strategy === 'weighted' ? Number(target.weight) : 100,
      priority: index + 1,
      thinking_level_map: target.persisted
        ? target.thinkingLevelMap
        : target.thinkingLevelMap.filter((row) => row.source === 'overridden'),
    }))

  return cleanTargets.length === targets.length && cleanTargets.length > 0
    ? { targets: cleanTargets }
    : { targets: cleanTargets, error: 'incomplete-target' }
}

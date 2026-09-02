import type { Route } from '$lib/types'

export type LogicalModelIdentity = Pick<Route, 'model_id' | 'display_name'>

export function effectiveModelDisplayName(model: LogicalModelIdentity): string {
  return model.display_name?.trim() || model.model_id
}

export function logicalModelSecondaryId(model: LogicalModelIdentity): string | undefined {
  const displayName = effectiveModelDisplayName(model)
  return displayName === model.model_id ? undefined : model.model_id
}

function compareText(left: string, right: string): number {
  return left === right ? 0 : left < right ? -1 : 1
}

export function compareLogicalModels(left: LogicalModelIdentity, right: LogicalModelIdentity): number {
  return (
    compareText(effectiveModelDisplayName(left), effectiveModelDisplayName(right)) ||
    compareText(left.model_id, right.model_id)
  )
}

export function sortLogicalModels<T extends LogicalModelIdentity>(models: readonly T[]): T[] {
  return [...models].sort(compareLogicalModels)
}

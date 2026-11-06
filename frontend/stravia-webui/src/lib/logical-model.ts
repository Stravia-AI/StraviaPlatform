import type { SearchComboboxOption } from '#lib/search-combobox-option.js'
import type { Route } from '#lib/types.js'

export type LogicalModelIdentity = Pick<Route, 'model_id' | 'display_name'>

export function effectiveModelDisplayName(model: LogicalModelIdentity): string {
  return model.display_name?.trim() || model.model_id
}

export function logicalModelSecondaryId(model: LogicalModelIdentity): string | undefined {
  const displayName = effectiveModelDisplayName(model)
  return displayName === model.model_id ? undefined : model.model_id
}

/** 选择框候选：展示名在上，与之不同的模型 ID 在下；未命名时展示名就是 ID，按技术值显示。 */
export function logicalModelOption(model: LogicalModelIdentity & { id: string }): SearchComboboxOption {
  const detail = logicalModelSecondaryId(model)
  return {
    value: model.id,
    label: effectiveModelDisplayName(model),
    detail,
    keywords: [model.model_id],
    technical: !detail,
  }
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

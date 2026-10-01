import type { ModelSpecification } from '$lib/types'

export interface SpecificationFilter {
  context?: number
  inputModalities: string[]
  outputModalities: string[]
  reasoningEfforts: string[]
}

export const emptySpecificationFilter: SpecificationFilter = {
  inputModalities: [],
  outputModalities: [],
  reasoningEfforts: [],
}

export function specificationFilterCount(filter: SpecificationFilter): number {
  return (
    Number(filter.context != null) +
    Number(filter.inputModalities.length > 0) +
    Number(filter.outputModalities.length > 0) +
    Number(filter.reasoningEfforts.length > 0)
  )
}

export function matchesSpecification(specification: ModelSpecification, filter: SpecificationFilter): boolean {
  return (
    (filter.context == null ||
      (specification.limit?.context != null && specification.limit.context >= filter.context)) &&
    filter.inputModalities.every((modality) => specification.modalities?.input.includes(modality)) &&
    filter.outputModalities.every((modality) => specification.modalities?.output.includes(modality)) &&
    filter.reasoningEfforts.every((effort) => specification.reasoning_efforts?.includes(effort))
  )
}

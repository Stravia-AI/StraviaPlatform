import type { ModelSpecification } from '$lib/types'
import type { specificationFeatures } from '$lib/model-specification'

export type SpecificationFeature = (typeof specificationFeatures)[number]['key']

export interface SpecificationFilter {
  context?: number
  output?: number
  inputModalities: string[]
  outputModalities: string[]
  features: SpecificationFeature[]
}

export const emptySpecificationFilter: SpecificationFilter = { inputModalities: [], outputModalities: [], features: [] }

export function specificationFilterCount(filter: SpecificationFilter): number {
  return (
    Number(filter.context != null) +
    Number(filter.output != null) +
    Number(filter.inputModalities.length > 0) +
    Number(filter.outputModalities.length > 0) +
    Number(filter.features.length > 0)
  )
}

export function matchesSpecification(specification: ModelSpecification, filter: SpecificationFilter): boolean {
  return (
    (filter.context == null ||
      (specification.limit?.context != null && specification.limit.context >= filter.context)) &&
    (filter.output == null || (specification.limit?.output != null && specification.limit.output >= filter.output)) &&
    filter.inputModalities.every((modality) => specification.modalities?.input.includes(modality)) &&
    filter.outputModalities.every((modality) => specification.modalities?.output.includes(modality)) &&
    filter.features.every((feature) => specification[feature] === true)
  )
}

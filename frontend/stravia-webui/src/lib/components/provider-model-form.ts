import * as m from '#lib/paraglide/messages.js'
import type { ProviderModelMetadata } from '#lib/types.js'

import { buildModelCostJson, normalizeProviderModelEfforts, type PriceForm } from './price-form.js'

export { normalizeProviderModelEfforts }
export type { PriceForm, ProviderModelSubmission } from './price-form.js'
export { buildProviderModelChanges } from './price-form.js'

export function providerModelFormFingerprint(metadata: ProviderModelMetadata, cost: PriceForm): string {
  return JSON.stringify({ metadata, cost })
}

/** Full metadata payload for manual model creation (the create endpoint is unchanged). */
export function buildProviderModelMetadataJson(
  detailId: string,
  metadata: ProviderModelMetadata,
  cost: PriceForm,
): { json: string | null; errors: string[] } {
  const errors: string[] = []
  const value = structuredClone(metadata) as Record<string, unknown>
  value.id = detailId
  for (const key of [
    'attachment',
    'reasoning',
    'tool_call',
    'structured_output',
    'temperature',
    'interleaved',
    'reasoning_options',
  ]) {
    delete value[key]
  }
  if (metadata.limit) value.limit = { context: metadata.limit.context }
  if (metadata.reasoning_efforts) value.reasoning_efforts = normalizeProviderModelEfforts(metadata.reasoning_efforts)

  if (Object.prototype.hasOwnProperty.call(metadata, 'limit') && metadata.limit) {
    for (const [key, number] of Object.entries({ context: metadata.limit.context })) {
      if (number != null && (!Number.isSafeInteger(number) || number < 0)) {
        errors.push(m.provider_model_editor_invalid_limit({ key }))
      }
    }
  }

  if (Object.prototype.hasOwnProperty.call(metadata, 'cost') && metadata.cost !== null) {
    const costResult = buildModelCostJson(cost)
    errors.push(...costResult.errors)
    if (costResult.value) value.cost = costResult.value
  }

  return { json: errors.length === 0 ? encodeJson(value) : null, errors }
}

function encodeJson(value: unknown): string {
  if (value === null) return 'null'
  if (typeof value === 'string' || typeof value === 'boolean') return JSON.stringify(value)
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw new Error(m.provider_model_editor_error_non_finite_number())
    return JSON.stringify(value)
  }
  if (Array.isArray(value)) return `[${value.map(encodeJson).join(',')}]`
  if (typeof value === 'object') {
    return `{${Object.entries(value)
      .filter(([, item]) => item !== undefined)
      .map(([key, item]) => `${JSON.stringify(key)}:${encodeJson(item)}`)
      .join(',')}}`
  }
  throw new Error(m.provider_model_editor_error_unsupported_value())
}

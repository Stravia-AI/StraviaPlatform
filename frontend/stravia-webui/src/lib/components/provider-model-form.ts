import * as m from '$lib/paraglide/messages.js'
import type { ProviderModelMetadata, ProviderModelPrices } from '$lib/types'

export interface ProviderModelPriceForm {
  input: string
  output: string
  reasoning: string
  cache_read: string
  cache_write: string
  input_audio: string
  output_audio: string
}

export interface ProviderModelCostTierForm extends ProviderModelPriceForm {
  threshold: string
}

export interface ProviderModelCostForm {
  base: ProviderModelPriceForm
  tiers: ProviderModelCostTierForm[]
}

interface RawDecimal {
  readonly rawDecimal: string
}

const priceFields: Array<{ key: keyof ProviderModelPriceForm; label: () => string }> = [
  { key: 'input', label: m.provider_model_field_input },
  { key: 'output', label: m.provider_model_field_output },
  { key: 'reasoning', label: m.provider_model_field_reasoning },
  { key: 'cache_read', label: m.provider_model_field_cache_read },
  { key: 'cache_write', label: m.provider_model_field_cache_write },
  { key: 'input_audio', label: m.provider_model_field_audio_input },
  { key: 'output_audio', label: m.provider_model_field_audio_output },
]
const decimalPattern = /^(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/

export function emptyProviderModelPrices(): ProviderModelPriceForm {
  return { input: '', output: '', reasoning: '', cache_read: '', cache_write: '', input_audio: '', output_audio: '' }
}

function pricesFromMetadata(prices: ProviderModelPrices | null | undefined): ProviderModelPriceForm {
  const result = emptyProviderModelPrices()
  for (const { key } of priceFields) {
    const value = prices?.[key]
    result[key] = value == null ? '' : String(value)
  }
  return result
}

export function emptyProviderModelCost(): ProviderModelCostForm {
  return { base: emptyProviderModelPrices(), tiers: [] }
}

export function providerModelCostFromMetadata(value: ProviderModelMetadata): ProviderModelCostForm {
  if (!value.cost) return emptyProviderModelCost()
  return {
    base: pricesFromMetadata(value.cost),
    tiers: (value.cost.tiers ?? []).map((tier) => ({ ...pricesFromMetadata(tier), threshold: String(tier.tier.size) })),
  }
}

export function providerModelFormFingerprint(
  metadata: ProviderModelMetadata,
  cost: ProviderModelCostForm,
): string {
  return JSON.stringify({ metadata, cost })
}

function rawDecimal(value: string, field: string, errors: string[]): RawDecimal | undefined {
  const trimmed = value.trim()
  if (!trimmed) return undefined
  if (!decimalPattern.test(trimmed)) {
    errors.push(m.provider_model_editor_value_must_non_negative_decimal({ field }))
    return undefined
  }
  return { rawDecimal: trimmed }
}

function buildPrices(form: ProviderModelPriceForm, label: string, errors: string[]): Record<string, unknown> {
  const result: Record<string, unknown> = {}
  for (const field of priceFields) {
    const value = rawDecimal(form[field.key], `${label} ${field.label()}`, errors)
    if (value) result[field.key] = value
  }
  return result
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
    if ('rawDecimal' in value) return (value as RawDecimal).rawDecimal
    return `{${Object.entries(value)
      .filter(([, item]) => item !== undefined)
      .map(([key, item]) => `${JSON.stringify(key)}:${encodeJson(item)}`)
      .join(',')}}`
  }
  throw new Error(m.provider_model_editor_error_unsupported_value())
}

export function buildProviderModelMetadataJson(
  detailId: string,
  metadata: ProviderModelMetadata,
  cost: ProviderModelCostForm,
): { json: string | null; errors: string[] } {
  const errors: string[] = []
  const value = structuredClone(metadata) as Record<string, unknown>
  value.id = detailId

  if (Object.prototype.hasOwnProperty.call(metadata, 'limit') && metadata.limit) {
    for (const [key, number] of Object.entries(metadata.limit)) {
      if (number != null && (!Number.isSafeInteger(number) || number < 0)) {
        errors.push(m.provider_model_editor_invalid_limit({ key }))
      }
    }
  }

  if (Object.prototype.hasOwnProperty.call(metadata, 'cost') && metadata.cost !== null) {
    const costValue: Record<string, unknown> = buildPrices(cost.base, m.provider_model_editor_base_cost(), errors)
    costValue.tiers = cost.tiers.map((tier, index) => {
      const threshold = Number(tier.threshold)
      if (!Number.isSafeInteger(threshold) || threshold < 0) {
        errors.push(m.provider_model_editor_invalid_tier_threshold({ index: index + 1 }))
      }
      return {
        tier: { type: 'context', size: threshold },
        ...buildPrices(tier, m.provider_model_editor_tier_value({ index: index + 1 }), errors),
      }
    })
    value.cost = costValue
  }

  return { json: errors.length === 0 ? encodeJson(value) : null, errors }
}

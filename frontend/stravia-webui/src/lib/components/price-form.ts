import * as m from '#lib/paraglide/messages.js'
import type {
  PeakPricing,
  PriceFieldName,
  ProviderModelCost,
  ProviderModelCostTier,
  ProviderModelMetadata,
  ProviderModelPrices,
} from '#lib/types.js'

export type BasePriceFieldName = Exclude<PriceFieldName, 'tiers'>
export type PriceCurrency = 'usd' | 'custom'
export type PriceTouchField = BasePriceFieldName | 'tiers' | 'peak'

export interface PriceFieldValue {
  amount: string
  currency: PriceCurrency
}

export type PriceFieldSet = Record<BasePriceFieldName, PriceFieldValue>
export type PriceAmountSet = Record<BasePriceFieldName, string>

export interface PriceTierForm {
  threshold: string
  prices: PriceAmountSet
}

export interface PeakRangeForm {
  start: string
  end: string
}

export interface PeakForm {
  timezone: string
  weekdays: number[]
  ranges: PeakRangeForm[]
  multiplier: string
}

/**
 * Editable mirror of `ModelCost`. `contextOver200k` is retained verbatim because
 * the shared editor does not expose the legacy field, but the tiers block still
 * owns it when it is written back.
 */
export interface PriceForm {
  base: PriceFieldSet
  tiers: PriceTierForm[]
  tiersCurrency: PriceCurrency
  contextOver200k: PriceAmountSet | null
  peak: PeakForm | null
}

export interface PriceFormResult {
  value: ProviderModelCost | null
  errors: string[]
}

export type { PriceFieldName }

export type ProviderModelSubmission =
  { kind: 'create'; metadataJson: string } | { kind: 'update'; changes: Record<string, unknown> }

export const basePriceFields: readonly BasePriceFieldName[] = ['input', 'output', 'cache_read', 'cache_write']
export const isoWeekdays: readonly number[] = [1, 2, 3, 4, 5, 6, 7]

const decimalPattern = /^(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/
const integerPattern = /^\d+$/
const clockSyntaxPattern = /^\d{2}:\d{2}$/
const decimalSyntaxPattern = /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/

/** Display symbol for a ModelCost field; `tiers` covers context_over_200k and every tier. */
export function priceFieldCurrencySymbol(
  field: PriceFieldName,
  customFields: ReadonlySet<PriceFieldName>,
  customSymbol: string | null | undefined,
): string {
  return customFields.has(field) && customSymbol ? customSymbol : '$'
}

export function priceFieldLabel(field: BasePriceFieldName): string {
  switch (field) {
    case 'input':
      return m.provider_model_field_input()
    case 'output':
      return m.provider_model_field_output()
    case 'cache_read':
      return m.provider_model_field_cache_read()
    case 'cache_write':
      return m.provider_model_field_cache_write()
  }
}

function decimalText(value: number | string | null | undefined): string {
  return value == null ? '' : String(value)
}

function emptyPriceFieldSet(): PriceFieldSet {
  return {
    input: { amount: '', currency: 'usd' },
    output: { amount: '', currency: 'usd' },
    cache_read: { amount: '', currency: 'usd' },
    cache_write: { amount: '', currency: 'usd' },
  }
}

function emptyPriceAmounts(): PriceAmountSet {
  return { input: '', output: '', cache_read: '', cache_write: '' }
}

function priceFieldSetAmounts(fields: PriceFieldSet): PriceAmountSet {
  return {
    input: fields.input.amount,
    output: fields.output.amount,
    cache_read: fields.cache_read.amount,
    cache_write: fields.cache_write.amount,
  }
}

export function emptyPriceForm(): PriceForm {
  return { base: emptyPriceFieldSet(), tiers: [], tiersCurrency: 'usd', contextOver200k: null, peak: null }
}

function amountsFromPrices(prices: ProviderModelPrices | null | undefined): PriceAmountSet {
  const result = emptyPriceAmounts()
  for (const field of basePriceFields) result[field] = decimalText(prices?.[field])
  return result
}

function peakFromCost(peak: PeakPricing): PeakForm {
  return {
    timezone: peak.timezone,
    weekdays: [...new Set(peak.weekdays)].filter((day) => Number.isInteger(day) && day >= 1 && day <= 7).sort(),
    ranges: peak.ranges.map((range) => ({ start: range.start, end: range.end })),
    multiplier: decimalText(peak.multiplier),
  }
}

export function priceFormFromCost(cost: ProviderModelCost | null | undefined): PriceForm {
  if (!cost) return emptyPriceForm()
  const customFields = new Set(cost.custom_currency_fields ?? [])
  const base = emptyPriceFieldSet()
  for (const field of basePriceFields) {
    base[field] = { amount: decimalText(cost[field]), currency: customFields.has(field) ? 'custom' : 'usd' }
  }
  return {
    base,
    tiers: (cost.tiers ?? []).map((tier) => ({
      threshold: decimalText(tier.tier.size),
      prices: amountsFromPrices(tier),
    })),
    tiersCurrency: customFields.has('tiers') ? 'custom' : 'usd',
    contextOver200k: cost.context_over_200k ? amountsFromPrices(cost.context_over_200k) : null,
    peak: cost.peak ? peakFromCost(cost.peak) : null,
  }
}

export function priceFormFingerprint(form: PriceForm): string {
  return JSON.stringify(form)
}

/** DeepSeek-style weekday peak defaults, matching the Core editor contract. */
export function peakFormPrefill(): PeakForm {
  return {
    timezone: 'Asia/Shanghai',
    weekdays: [1, 2, 3, 4, 5],
    ranges: [
      { start: '09:00', end: '12:00' },
      { start: '14:00', end: '18:00' },
    ],
    multiplier: '2',
  }
}

function amountsRecord(prices: PriceAmountSet): Record<BasePriceFieldName, string | null> {
  const record: Record<BasePriceFieldName, string | null> = {
    input: null,
    output: null,
    cache_read: null,
    cache_write: null,
  }
  for (const field of basePriceFields) record[field] = prices[field].trim() || null
  return record
}

function amountsObject(prices: PriceAmountSet): Partial<Record<BasePriceFieldName, string>> {
  const result: Partial<Record<BasePriceFieldName, string>> = {}
  for (const field of basePriceFields) {
    const amount = prices[field].trim()
    if (amount !== '') result[field] = amount
  }
  return result
}

export function peakFormJson(peak: PeakForm): PeakPricing {
  return {
    timezone: peak.timezone.trim(),
    weekdays: [...new Set(peak.weekdays)].sort((left, right) => left - right),
    ranges: peak.ranges.map((range) => ({ start: range.start.trim(), end: range.end.trim() })),
    multiplier: peak.multiplier.trim(),
  }
}

/** Composite tiers change shared with the Provider Model write contract. */
export function tiersChangeValue(form: PriceForm): Record<string, unknown> | null {
  if (form.tiers.length === 0 && form.contextOver200k == null) return null
  return {
    currency: form.tiersCurrency,
    context_over_200k: form.contextOver200k ? amountsRecord(form.contextOver200k) : null,
    tiers: form.tiers.map((tier) => ({ threshold: Number(tier.threshold.trim()), prices: amountsRecord(tier.prices) })),
  }
}

function priceChangeValue(value: PriceFieldValue): Record<string, unknown> | null {
  const amount = value.amount.trim()
  return amount === '' ? null : { amount, currency: value.currency }
}

function validatePeak(peak: PeakForm, errors: string[]): void {
  // UI-local checks only: required input and lexical syntax. IANA timezone,
  // weekday uniqueness, clock bounds, overlap, midnight crossing and multiplier
  // positivity are Core domain rules and are reported by the save response.
  if (peak.timezone.trim() === '') errors.push(m.pricing_peak_timezone_required())
  if (peak.weekdays.length === 0) errors.push(m.pricing_peak_weekdays_required())
  if (peak.ranges.length === 0) errors.push(m.pricing_peak_ranges_required())
  for (const [index, range] of peak.ranges.entries()) {
    if (!clockSyntaxPattern.test(range.start.trim()) || !clockSyntaxPattern.test(range.end.trim())) {
      errors.push(m.pricing_peak_range_time_invalid({ range: index + 1 }))
    }
  }
  if (!decimalSyntaxPattern.test(peak.multiplier.trim())) {
    errors.push(m.pricing_invalid_amount({ field: m.pricing_peak_multiplier() }))
  }
}

export function validatePriceForm(form: PriceForm): string[] {
  const errors: string[] = []
  for (const field of basePriceFields) {
    const amount = form.base[field].amount.trim()
    if (amount !== '' && !decimalPattern.test(amount)) {
      errors.push(m.pricing_invalid_amount({ field: priceFieldLabel(field) }))
    }
  }
  const thresholds = new Set<number>()
  for (const [index, tier] of form.tiers.entries()) {
    const threshold = tier.threshold.trim()
    if (!integerPattern.test(threshold) || !Number.isSafeInteger(Number(threshold))) {
      errors.push(m.pricing_invalid_tier_threshold({ tier: index + 1 }))
    } else if (thresholds.has(Number(threshold))) {
      errors.push(m.pricing_duplicate_tier_threshold({ tier: index + 1 }))
    } else {
      thresholds.add(Number(threshold))
    }
    for (const field of basePriceFields) {
      const amount = tier.prices[field].trim()
      if (amount !== '' && !decimalPattern.test(amount)) {
        errors.push(m.pricing_invalid_tier_amount({ tier: index + 1, field: priceFieldLabel(field) }))
      }
    }
  }
  if (form.peak) validatePeak(form.peak, errors)
  return errors
}

/**
 * Builds the `ModelCost` JSON used by manual creation and Route per-token billing.
 * Amounts are decimal strings; Core deserializes them without floating-point loss.
 */
export function buildModelCostJson(form: PriceForm): PriceFormResult {
  const errors = validatePriceForm(form)
  if (errors.length > 0) return { value: null, errors }

  const value: ProviderModelCost = {
    ...amountsObject(priceFieldSetAmounts(form.base)),
    tiers: form.tiers.map((tier): ProviderModelCostTier => ({
      tier: { type: 'context', size: Number(tier.threshold.trim()) },
      ...amountsObject(tier.prices),
    })),
  }
  const contextAmounts = form.contextOver200k ? amountsObject(form.contextOver200k) : {}
  if (Object.keys(contextAmounts).length > 0) {
    value.context_over_200k = amountsRecord(form.contextOver200k!)
  }

  const customFields: PriceFieldName[] = basePriceFields.filter(
    (field) => form.base[field].currency === 'custom' && form.base[field].amount.trim() !== '',
  )
  if (form.tiersCurrency === 'custom' && (form.tiers.length > 0 || Object.keys(contextAmounts).length > 0)) {
    customFields.push('tiers')
  }
  if (customFields.length > 0) value.custom_currency_fields = customFields
  if (form.peak) value.peak = peakFormJson(form.peak)
  return { value, errors: [] }
}

function normalizedEfforts(metadata: ProviderModelMetadata): string[] | null {
  if (metadata.reasoning_efforts == null) return null
  return normalizeProviderModelEfforts(metadata.reasoning_efforts)
}

export function normalizeProviderModelEfforts(values: readonly string[]): string[] {
  return [
    ...new Set(
      values
        .filter((value) => typeof value === 'string')
        .map((value) => value.trim())
        .filter((value) => value !== '' && value !== 'default' && value !== 'null'),
    ),
  ]
}

/**
 * Only touched or actually changed fields are returned. A touched field is sent
 * even when its value equals the confirmed value, because presence pins it.
 */
export function buildProviderModelChanges(
  confirmed: ProviderModelMetadata,
  metadata: ProviderModelMetadata,
  confirmedCost: PriceForm,
  cost: PriceForm,
  touched: ReadonlySet<string>,
): { changes: Record<string, unknown>; errors: string[] } {
  const errors = validatePriceForm(cost)
  if (errors.length > 0) return { changes: {}, errors }

  const changes: Record<string, unknown> = {}
  const include = (key: string, current: unknown, initial: unknown): void => {
    if (touched.has(key) || JSON.stringify(current) !== JSON.stringify(initial)) changes[key] = current
  }
  const modalities = (value: ProviderModelMetadata, target: 'input' | 'output'): string[] | null =>
    value.modalities == null ? null : [...value.modalities[target]]

  include('name', metadata.name ?? null, confirmed.name ?? null)
  include('description', metadata.description ?? null, confirmed.description ?? null)
  include('limit.context', metadata.limit?.context ?? null, confirmed.limit?.context ?? null)
  include('modalities.input', modalities(metadata, 'input'), modalities(confirmed, 'input'))
  include('modalities.output', modalities(metadata, 'output'), modalities(confirmed, 'output'))
  include('reasoning_efforts', normalizedEfforts(metadata), normalizedEfforts(confirmed))

  for (const field of basePriceFields) {
    include(`cost.${field}`, priceChangeValue(cost.base[field]), priceChangeValue(confirmedCost.base[field]))
  }
  include('cost.tiers', tiersChangeValue(cost), tiersChangeValue(confirmedCost))
  include(
    'cost.peak',
    cost.peak ? peakFormJson(cost.peak) : null,
    confirmedCost.peak ? peakFormJson(confirmedCost.peak) : null,
  )
  return { changes, errors: [] }
}

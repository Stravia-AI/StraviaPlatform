import { describe, expect, test } from 'bun:test'

import {
  buildProviderModelMetadataJson,
  emptyProviderModelCost,
  providerModelCostFromMetadata,
} from '../src/lib/components/provider-model-form'

describe('provider model form', () => {
  test('preserves decimal price text without floating-point conversion', () => {
    const metadata = { cost: { input: 0, tiers: [] } }
    const cost = providerModelCostFromMetadata(metadata)
    cost.base.input = '0.000000000000000000123'

    const result = buildProviderModelMetadataJson('model-id', metadata, cost)

    expect(result.errors).toEqual([])
    expect(result.json).toContain('"input":0.000000000000000000123')
    expect(JSON.parse(result.json!)).toMatchObject({ id: 'model-id' })
  })

  test('rejects negative tier thresholds', () => {
    const metadata = { cost: { tiers: [] } }
    const cost = emptyProviderModelCost()
    cost.tiers.push({ ...cost.base, threshold: '-1' })

    const result = buildProviderModelMetadataJson('model-id', metadata, cost)

    expect(result.json).toBeNull()
    expect(result.errors).toHaveLength(1)
  })

  test('preserves an explicit null cost', () => {
    const metadata = { cost: null }

    const result = buildProviderModelMetadataJson('model-id', metadata, providerModelCostFromMetadata(metadata))

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toMatchObject({ id: 'model-id', cost: null })
  })
})

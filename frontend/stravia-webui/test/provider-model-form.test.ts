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

  test('removes retired specification fields while preserving explicit supported efforts', () => {
    const metadata = {
      name: 'Model',
      attachment: true,
      reasoning: true,
      tool_call: true,
      structured_output: true,
      temperature: true,
      interleaved: { field: 'reasoning_content' },
      reasoning_options: [{ type: 'toggle' }],
      limit: { context: 128000, input: 100000, output: 32000 },
      reasoning_efforts: [
        'none',
        'minimal',
        'low',
        'medium',
        'high',
        'xhigh',
        'max',
        ' custom ',
        '',
        'default',
        'null',
        'high',
      ],
    }
    const result = buildProviderModelMetadataJson('model-id', metadata, emptyProviderModelCost())

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toEqual({
      id: 'model-id',
      name: 'Model',
      limit: { context: 128000 },
      reasoning_efforts: ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'custom'],
    })
  })

  test('serializes only supported prices across base, extended context, and tiers', () => {
    const metadata = {
      cost: {
        input: 2,
        reasoning: 1,
        input_audio: 2,
        output_audio: 3,
        context_over_200k: {
          input: 4,
          output: 8,
          cache_read: 0,
          cache_write: 2,
          reasoning: 4,
          input_audio: 5,
          output_audio: 6,
        },
        tiers: [{ tier: { type: 'context', size: 200000 }, input: 4, reasoning: 7, input_audio: 8, output_audio: 9 }],
      },
    }
    const cost = providerModelCostFromMetadata(metadata)
    cost.base.input = '3'
    cost.tiers[0].output = '9'
    const result = buildProviderModelMetadataJson('model-id', metadata, cost)

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toEqual({
      id: 'model-id',
      cost: {
        input: 3,
        context_over_200k: { input: 4, output: 8, cache_read: 0, cache_write: 2 },
        tiers: [{ tier: { type: 'context', size: 200000 }, input: 4, output: 9 }],
      },
    })
  })

  test('preserves an explicit null cost', () => {
    const metadata = { cost: null }

    const result = buildProviderModelMetadataJson('model-id', metadata, providerModelCostFromMetadata(metadata))

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toMatchObject({ id: 'model-id', cost: null })
  })
})

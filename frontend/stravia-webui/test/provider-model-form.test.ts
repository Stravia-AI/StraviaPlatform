import { describe, expect, test } from 'bun:test'

import { buildProviderModelMetadataJson } from '../src/lib/components/provider-model-form'
import { emptyPriceForm, priceFormFromCost } from '../src/lib/components/price-form'

describe('provider model form', () => {
  test('preserves decimal price text without floating-point conversion', () => {
    const metadata = { cost: { input: 0, tiers: [] } }
    const cost = priceFormFromCost(metadata.cost)
    cost.base.input.amount = '0.000000000000000000123'

    const result = buildProviderModelMetadataJson('model-id', metadata, cost)

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toMatchObject({ id: 'model-id', cost: { input: '0.000000000000000000123' } })
  })

  test('rejects invalid tier thresholds', () => {
    const metadata = { cost: { tiers: [] } }
    const cost = emptyPriceForm()
    cost.tiers.push({ threshold: '-1', prices: { input: '', output: '', cache_read: '', cache_write: '' } })

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
    const result = buildProviderModelMetadataJson('model-id', metadata, emptyPriceForm())

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toEqual({
      id: 'model-id',
      name: 'Model',
      limit: { context: 128000 },
      reasoning_efforts: ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'custom'],
    })
  })

  test('serializes prices, tier block, per-field currency markers, and peak pricing', () => {
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
    const cost = priceFormFromCost(metadata.cost)
    cost.base.input.amount = '3'
    cost.base.output.amount = '2'
    cost.base.output.currency = 'custom'
    cost.tiers[0].prices.output = '9'
    cost.tiersCurrency = 'custom'
    cost.peak = {
      timezone: 'Asia/Shanghai',
      weekdays: [1, 2, 3, 4, 5],
      ranges: [
        { start: '09:00', end: '12:00' },
        { start: '14:00', end: '18:00' },
      ],
      multiplier: '2',
    }
    const result = buildProviderModelMetadataJson('model-id', metadata, cost)

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toEqual({
      id: 'model-id',
      cost: {
        input: '3',
        output: '2',
        context_over_200k: { input: '4', output: '8', cache_read: '0', cache_write: '2' },
        tiers: [{ tier: { type: 'context', size: 200000 }, input: '4', output: '9' }],
        custom_currency_fields: ['output', 'tiers'],
        peak: {
          timezone: 'Asia/Shanghai',
          weekdays: [1, 2, 3, 4, 5],
          ranges: [
            { start: '09:00', end: '12:00' },
            { start: '14:00', end: '18:00' },
          ],
          multiplier: '2',
        },
      },
    })
  })

  test('preserves an explicit null cost', () => {
    const metadata = { cost: null }

    const result = buildProviderModelMetadataJson('model-id', metadata, priceFormFromCost(metadata.cost))

    expect(result.errors).toEqual([])
    expect(JSON.parse(result.json!)).toMatchObject({ id: 'model-id', cost: null })
  })
})

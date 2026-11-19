import { describe, expect, test } from 'bun:test'

import { formatAllowanceAmount, formatAllowancePercent } from '../src/lib/provider-allowance-format'

const EN = 'en-US' as const
const ZH = 'zh-CN' as const

describe('provider allowance values', () => {
  test('keeps the upstream unit and currency explicit', () => {
    expect(formatAllowanceAmount({ value: 1_250, unit: 'tokens' }, EN)).toBe('1,250 tokens')
    expect(formatAllowanceAmount({ value: 12, unit: 'requests' }, ZH)).toBe('12 次请求')
    expect(formatAllowanceAmount({ value: 12.5, unit: 'currency', currency: 'USD' }, ZH)).toBe('12.50 USD')
    expect(formatAllowanceAmount(undefined, EN)).toBe('–')
  })

  test('rounds credits and currency to two localized decimals only', () => {
    expect(formatAllowanceAmount({ value: 34_247.465142, unit: 'credits' }, EN)).toBe('34,247.47 credits')
    expect(formatAllowanceAmount({ value: 34_247.465142, unit: 'credits' }, ZH)).toBe('34,247.47 点')
    expect(formatAllowanceAmount({ value: 8.125, unit: 'credits' }, EN, { value: 20, unit: 'credits' })).toBe(
      '8.13 / 20.00 credits',
    )
    expect(formatAllowanceAmount({ value: 34_247.125, unit: 'currency', currency: 'CNY' }, EN)).toBe('34,247.13 CNY')
    expect(formatAllowanceAmount({ value: 34_247, unit: 'currency', currency: 'CNY' }, EN)).toBe('34,247.00 CNY')
    expect(formatAllowanceAmount({ value: 1.25, unit: 'units' }, EN)).toBe('1.25 units')
  })

  test('formats the normalized percentage without inventing a quota', () => {
    expect(formatAllowancePercent(76, EN)).toBe('76%')
    expect(formatAllowancePercent(111.25, ZH)).toBe('111.25%')
    expect(formatAllowancePercent(undefined, EN)).toBe('–')
  })
})

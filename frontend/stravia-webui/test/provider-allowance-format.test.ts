import { describe, expect, test } from 'bun:test'

import { formatAllowanceAmount, formatAllowancePercent } from '../src/lib/provider-allowance-format'

const EN = 'en-US' as const
const ZH = 'zh-CN' as const

describe('provider allowance values', () => {
  test('keeps the upstream unit and currency explicit', () => {
    expect(formatAllowanceAmount({ value: 1_250, unit: 'tokens' }, EN)).toBe('1,250 tokens')
    expect(formatAllowanceAmount({ value: 12, unit: 'requests' }, ZH)).toBe('12 次请求')
    expect(formatAllowanceAmount({ value: 12.5, unit: 'currency', currency: 'USD' }, ZH)).toBe('12.5 USD')
    expect(formatAllowanceAmount(undefined, EN)).toBe('–')
  })

  test('formats the normalized percentage without inventing a quota', () => {
    expect(formatAllowancePercent(76, EN)).toBe('76%')
    expect(formatAllowancePercent(111.25, ZH)).toBe('111.25%')
    expect(formatAllowancePercent(undefined, EN)).toBe('–')
  })
})

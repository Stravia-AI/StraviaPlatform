import { describe, expect, test } from 'bun:test'
import {
  formatCostAmount,
  formatCostNumber,
  formatMoney,
  formatValueMultiple,
  hasMoneyRecords,
  roundToCents,
} from '../src/lib/money'

const EN = 'en-US' as const
const USD = { kind: 'usd', symbol: '$' } as const
const amount = (value: string, known = 0, unknown = 0) => ({
  amount: value,
  known_count: known,
  unknown_count: unknown,
})
const subscription = (multiple: string | null, unknownUsage = 0) => ({
  cost: amount('10', 0, 0),
  usage_equivalent_cost: amount('25', 3, unknownUsage),
  multiple,
})

describe('cost rounding', () => {
  test.each([
    ['1.005', 101n],
    ['1.004', 100n],
    ['0.999', 100n],
    ['-1.005', -101n],
    ['0.0049', 0n],
    ['12', 1200n],
    ['0.5', 50n],
    ['1234567890123456789.995', 123456789012345679000n],
  ])('%s rounds half away from zero to %p cents', (input, cents) => {
    expect(roundToCents(input)).toBe(cents)
  })

  test('rejects text that is not a plain decimal', () => {
    expect(roundToCents('1e-7')).toBeNull()
    expect(roundToCents('abc')).toBeNull()
  })

  test('always shows two decimals and groups thousands', () => {
    expect(formatCostNumber('1.005', EN)).toBe('1.01')
    expect(formatCostNumber('3', EN)).toBe('3.00')
    expect(formatCostNumber('0.00000001', EN)).toBe('0.00')
    expect(formatCostNumber('12345.6789', EN)).toBe('12,345.68')
    expect(formatCostAmount('23.5', '¥', EN)).toBe('¥23.50')
  })

  test('unparseable amounts are shown as received instead of invented numbers', () => {
    expect(formatCostNumber('n/a', EN)).toBe('n/a')
  })
})

describe('cost presence', () => {
  test('an empty aggregate is not a recorded zero', () => {
    expect(hasMoneyRecords(amount('0'))).toBe(false)
    expect(hasMoneyRecords(null)).toBe(false)
    expect(hasMoneyRecords(undefined)).toBe(false)
  })

  test.each([
    ['known attempts', amount('0', 2, 0)],
    ['unknown attempts', amount('0', 0, 1)],
    ['subscription amortization without attempts', amount('12.5')],
  ])('%s count as recorded cost', (_name, money) => {
    expect(hasMoneyRecords(money)).toBe(true)
  })

  test('all-unknown usage reads Unknown, not $0.00', () => {
    expect(formatMoney(amount('0', 0, 3), USD)).not.toContain('$')
    expect(formatMoney(amount('0.004', 1, 0), USD)).toBe('$0.00')
  })
})

describe('subscription value multiple', () => {
  test('rounds to two decimals with a multiplication sign', () => {
    expect(formatValueMultiple(subscription('2.5'))).toBe('2.50×')
    expect(formatValueMultiple(subscription('12.3456'))).toBe('12.35×')
  })

  test('marks a lower bound when some usage has no price', () => {
    expect(formatValueMultiple(subscription('2.5', 2))).toBe('≥2.50×')
  })

  test('shows a dash when the subscription cost for the window is zero', () => {
    expect(formatValueMultiple(subscription(null))).toBe('—')
    expect(formatValueMultiple(null)).toBe('—')
  })
})

import { describe, expect, test } from 'bun:test'

import { STATS_TOP_N, sortStatsRows, visibleStatsRows } from '../src/lib/stats-table'

interface Row {
  name: string
  count: number
  speed: number | null
}

const values = { name: (row: Row) => row.name, count: (row: Row) => row.count, speed: (row: Row) => row.speed }
const rows: Row[] = [
  { name: 'beta', count: 20, speed: null },
  { name: 'alpha', count: 5, speed: 12 },
  { name: 'gamma', count: 20, speed: 3 },
  { name: 'delta', count: 1, speed: 40 },
]

describe('stats table ordering', () => {
  test('without a sort the backend order is kept and the input is not mutated', () => {
    const sorted = sortStatsRows(rows, [], values)
    expect(sorted.map((row) => row.name)).toEqual(['beta', 'alpha', 'gamma', 'delta'])
    expect(sorted).not.toBe(rows)
    expect(rows.map((row) => row.name)).toEqual(['beta', 'alpha', 'gamma', 'delta'])
  })

  test('numeric columns sort numerically in both directions and ties keep the backend order', () => {
    expect(sortStatsRows(rows, [{ id: 'count', desc: true }], values).map((row) => row.name)).toEqual([
      'beta',
      'gamma',
      'alpha',
      'delta',
    ])
    expect(sortStatsRows(rows, [{ id: 'count', desc: false }], values).map((row) => row.name)).toEqual([
      'delta',
      'alpha',
      'beta',
      'gamma',
    ])
  })

  test('unreported values sort after every reported value whichever the direction', () => {
    expect(sortStatsRows(rows, [{ id: 'speed', desc: false }], values).map((row) => row.name)).toEqual([
      'gamma',
      'alpha',
      'delta',
      'beta',
    ])
    expect(sortStatsRows(rows, [{ id: 'speed', desc: true }], values).map((row) => row.name)).toEqual([
      'delta',
      'alpha',
      'gamma',
      'beta',
    ])
  })

  test('text sorts naturally and ignores case', () => {
    const named = [{ name: 'key 10' }, { name: 'Key 2' }, { name: 'key 1' }]
    expect(
      sortStatsRows(named, [{ id: 'name', desc: false }], { name: (row) => row.name }).map((row) => row.name),
    ).toEqual(['key 1', 'Key 2', 'key 10'])
  })

  test('an unknown column id leaves the order unchanged', () => {
    expect(sortStatsRows(rows, [{ id: 'missing', desc: true }], values).map((row) => row.name)).toEqual([
      'beta',
      'alpha',
      'gamma',
      'delta',
    ])
  })
})

describe('stats table range', () => {
  const many = Array.from({ length: 11 }, (_, index) => index)

  test('shows the first eight after ordering unless all rows are requested', () => {
    expect(STATS_TOP_N).toBe(8)
    expect(visibleStatsRows(many, false)).toEqual(many.slice(0, 8))
    expect(visibleStatsRows(many, true)).toEqual(many)
  })

  test('short collections are shown whole', () => {
    expect(visibleStatsRows([1, 2, 3], false)).toEqual([1, 2, 3])
  })
})

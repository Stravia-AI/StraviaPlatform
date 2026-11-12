import { describe, expect, test } from 'bun:test'
import { virtualSegments } from '../src/lib/virtual-list'

const items = [
  { id: 'a1', group: 'a' },
  { id: 'a2', group: 'a' },
  { id: 'b1', group: 'b' },
  { id: 'solo' },
  { id: 'a3', group: 'a' },
]
const keyOf = (item: (typeof items)[number]) => item.id
const groupOf = (item: (typeof items)[number]) => item.group
const entries = (indexes: number[]) => indexes.map((index) => ({ index, start: index * 10, key: items[index].id }))

describe('virtualSegments', () => {
  test('puts every row in its own segment without a group', () => {
    const segments = virtualSegments(entries([0, 1, 2]), items, keyOf)
    expect(segments.map((segment) => [segment.key, segment.start, segment.rows.map((row) => row.key)])).toEqual([
      ['row:a1', 0, ['a1']],
      ['row:a2', 10, ['a2']],
      ['row:b1', 20, ['b1']],
    ])
  })

  test('merges adjacent rows of one group at the first row offset', () => {
    const segments = virtualSegments(entries([1, 2, 3]), items, keyOf, groupOf)
    expect(segments.map((segment) => [segment.key, segment.group, segment.start, segment.rows.length])).toEqual([
      ['group:a', 'a', 10, 1],
      ['group:b', 'b', 20, 1],
      ['row:solo', undefined, 30, 1],
    ])
    expect(virtualSegments(entries([0, 1]), items, keyOf, groupOf)[0].rows.map((row) => row.index)).toEqual([0, 1])
  })

  test('splits a group around a retained row that is not adjacent', () => {
    const segments = virtualSegments(entries([0, 1, 4]), items, keyOf, groupOf)
    expect(segments.map((segment) => [segment.key, segment.rows.map((row) => row.index)])).toEqual([
      ['group:a', [0, 1]],
      ['group:a#1', [4]],
    ])
  })

  test('drops entries whose row is gone or was replaced', () => {
    const stale = [
      { index: 0, start: 0, key: 'a1' },
      { index: 1, start: 10, key: 'moved' },
      { index: 9, start: 90, key: 'a1' },
    ]
    expect(virtualSegments(stale, items, keyOf, groupOf).map((segment) => segment.rows.map((row) => row.key))).toEqual([
      ['a1'],
    ])
  })
})

import { describe, expect, test } from 'bun:test'

import { layoutForest, type CachedLayout, type LayoutRequest } from '../src/lib/interaction-layout'

function request(roots: LayoutRequest['roots'], edges: LayoutRequest['edges'] = []): LayoutRequest {
  return { requestId: 1, roots, edges }
}

function xOf(positions: { id: string; x: number }[], id: string): number {
  const position = positions.find((item) => item.id === id)
  if (!position) throw new Error(`missing ${id}`)
  return position.x
}

describe('interaction layout columns', () => {
  test('reclaims a narrowed group width before positioning the next root', () => {
    const roots: LayoutRequest['roots'] = [
      { id: 'tree', startedAt: 200, interactions: Array.from({ length: 8 }, (_, index) => ({ id: `turn-${index}` })) },
      { id: 'next', startedAt: 100, interactions: [{ id: 'next' }] },
    ]
    const cache = new Map<string, CachedLayout>()
    layoutForest(request(roots), cache)
    const connected = request(
      roots,
      Array.from({ length: 7 }, (_, index) => ({ source: `turn-${index}`, target: `turn-${index + 1}` })),
    )
    const retained = layoutForest(connected, cache)
    const fresh = layoutForest(connected, new Map())
    expect(xOf(retained, 'next')).toBe(xOf(fresh, 'next'))
  })

  test('places the newest root request in the leftmost column', () => {
    const positions = layoutForest(
      request([
        { id: 'old', startedAt: 100, interactions: [{ id: 'old' }] },
        { id: 'new', startedAt: 300, interactions: [{ id: 'new' }] },
        { id: 'mid', startedAt: 200, interactions: [{ id: 'mid' }] },
      ]),
      new Map(),
    )
    expect(xOf(positions, 'new')).toBeLessThan(xOf(positions, 'mid'))
    expect(xOf(positions, 'mid')).toBeLessThan(xOf(positions, 'old'))
  })

  test('keeps a newer isolated root left of an older linked group', () => {
    const positions = layoutForest(
      request(
        [
          { id: 'old-a', startedAt: 100, interactions: [{ id: 'old-a' }] },
          { id: 'old-b', startedAt: 150, interactions: [{ id: 'old-b' }] },
          { id: 'new', startedAt: 300, interactions: [{ id: 'new' }] },
        ],
        [{ source: 'old-a', target: 'old-b' }],
      ),
      new Map(),
    )
    expect(xOf(positions, 'new')).toBeLessThan(xOf(positions, 'old-a'))
    expect(xOf(positions, 'new')).toBeLessThan(xOf(positions, 'old-b'))
  })

  test('places a linked group by its newest root request', () => {
    const positions = layoutForest(
      request(
        [
          { id: 'new', startedAt: 300, interactions: [{ id: 'new' }] },
          { id: 'old', startedAt: 100, interactions: [{ id: 'old' }] },
          { id: 'mid', startedAt: 200, interactions: [{ id: 'mid' }] },
        ],
        [{ source: 'new', target: 'old' }],
      ),
      new Map(),
    )
    expect(xOf(positions, 'new')).toBeLessThan(xOf(positions, 'mid'))
    expect(xOf(positions, 'old')).toBeLessThan(xOf(positions, 'mid'))
  })
})

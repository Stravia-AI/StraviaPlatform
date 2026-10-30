import { describe, expect, test } from 'bun:test'

import { AUTOSCROLL_EDGE_PX, AUTOSCROLL_MAX_SPEED, autoscrollSpeed } from '../src/lib/drag-autoscroll'

describe('autoscrollSpeed', () => {
  const top = 100
  const bottom = 900

  test('does not scroll while the pointer is away from both edges', () => {
    expect(autoscrollSpeed(500, top, bottom)).toBe(0)
    expect(autoscrollSpeed(top + AUTOSCROLL_EDGE_PX, top, bottom)).toBe(0)
    expect(autoscrollSpeed(bottom - AUTOSCROLL_EDGE_PX, top, bottom)).toBe(0)
  })

  test('scrolls toward the nearer edge and speeds up as the pointer approaches it', () => {
    const shallow = autoscrollSpeed(bottom - AUTOSCROLL_EDGE_PX / 2, top, bottom)
    const deep = autoscrollSpeed(bottom - 4, top, bottom)
    expect(shallow).toBeGreaterThan(0)
    expect(deep).toBeGreaterThan(shallow)
    expect(autoscrollSpeed(top + AUTOSCROLL_EDGE_PX / 2, top, bottom)).toBe(-shallow)
  })

  test('caps the speed once the pointer reaches or leaves the container', () => {
    expect(autoscrollSpeed(bottom, top, bottom)).toBe(AUTOSCROLL_MAX_SPEED)
    expect(autoscrollSpeed(bottom + 500, top, bottom)).toBe(AUTOSCROLL_MAX_SPEED)
    expect(autoscrollSpeed(top - 500, top, bottom)).toBe(-AUTOSCROLL_MAX_SPEED)
  })

  test('keeps a dead zone in containers shorter than two edge zones', () => {
    const shortTop = 0
    const shortBottom = 80
    expect(autoscrollSpeed(40, shortTop, shortBottom)).toBe(0)
    expect(autoscrollSpeed(shortBottom - 5, shortTop, shortBottom)).toBeGreaterThan(0)
    expect(autoscrollSpeed(10, shortTop, 0)).toBe(0)
  })
})

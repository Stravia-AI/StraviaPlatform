import { describe, expect, test } from 'bun:test'
import {
  customHitWindow,
  hitWindowBounds,
  hitWindowQuery,
  isValidHitRange,
  shiftHitWindow,
  type HitWindow,
} from '../src/lib/credential-hit-window'

const HOUR = 3_600_000
const NOW = 1_800_000_000_000
const live = (hours: number, custom = false): HitWindow => ({ live: true, custom, spanMs: hours * HOUR })

describe('hitWindowQuery', () => {
  test('sends only a rolling lower bound for a live window so new hits are never cut off', () => {
    expect(hitWindowQuery(live(24), NOW)).toEqual({ start_at: NOW - 24 * HOUR })
  })

  test('sends both bounds of a fixed window exactly as chosen', () => {
    expect(hitWindowQuery(customHitWindow(NOW - 3 * HOUR, NOW - HOUR), NOW)).toEqual({
      start_at: NOW - 3 * HOUR,
      end_at: NOW - HOUR,
    })
  })
})

describe('hitWindowBounds', () => {
  test('ends a live window at the current time and keeps a fixed window as chosen', () => {
    expect(hitWindowBounds(live(6), NOW)).toEqual({ startAt: NOW - 6 * HOUR, endAt: NOW })
    expect(hitWindowBounds(customHitWindow(NOW - 3 * HOUR, NOW - HOUR), NOW)).toMatchObject({
      startAt: NOW - 3 * HOUR,
      endAt: NOW - HOUR,
    })
  })
})

describe('shiftHitWindow', () => {
  test('moves from the live window to the span right before it', () => {
    expect(shiftHitWindow(live(6), -1, NOW)).toMatchObject({
      live: false,
      custom: false,
      startAt: NOW - 12 * HOUR,
      endAt: NOW - 6 * HOUR,
    })
  })

  test('keeps stepping back by whole spans and returns to the live window once it reaches now', () => {
    const twice = shiftHitWindow(shiftHitWindow(live(6), -1, NOW), -1, NOW)
    expect(twice).toMatchObject({ startAt: NOW - 18 * HOUR, endAt: NOW - 12 * HOUR })
    const newer = shiftHitWindow(twice, 1, NOW)
    expect(newer).toMatchObject({ live: false, startAt: NOW - 12 * HOUR, endAt: NOW - 6 * HOUR })
    expect(shiftHitWindow(newer, 1, NOW)).toEqual(live(6))
  })

  test('returns to the live window after Older then Newer even when time passed in between', () => {
    const older = shiftHitWindow(live(6), -1, NOW)
    expect(shiftHitWindow(older, 1, NOW + 90_000)).toEqual(live(6))
  })

  test('keeps a custom range custom when it is shifted or returns to live', () => {
    const custom = customHitWindow(NOW - 3 * HOUR, NOW - 2 * HOUR)
    expect(shiftHitWindow(custom, -1, NOW)).toMatchObject({ custom: true, startAt: NOW - 4 * HOUR, endAt: NOW - 3 * HOUR })
    expect(shiftHitWindow(custom, 1, NOW)).toMatchObject({ live: false, startAt: NOW - 2 * HOUR, endAt: NOW - HOUR })
    expect(shiftHitWindow(customHitWindow(NOW - 2 * HOUR, NOW - HOUR), 1, NOW)).toEqual(live(1, true))
  })
})

describe('isValidHitRange', () => {
  test('requires finite times with the end after the start and has no maximum span', () => {
    expect(isValidHitRange(NOW - 400 * 24 * HOUR, NOW)).toBe(true)
    expect(isValidHitRange(NOW, NOW)).toBe(false)
    expect(isValidHitRange(NOW, NOW - 1)).toBe(false)
    expect(isValidHitRange(Number.NaN, NOW)).toBe(false)
  })
})

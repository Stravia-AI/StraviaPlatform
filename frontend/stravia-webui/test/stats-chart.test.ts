import { describe, expect, test } from 'bun:test'

import { buildActivityGrid, buildLatencyChart } from '../src/lib/stats-chart'

const HOUR_MS = 3_600_000
const DAY_MS = 86_400_000

describe('latency chart', () => {
  test('breaks both series across buckets without requests', () => {
    const start = Date.UTC(2026, 8, 2, 18)
    const points = buildLatencyChart(
      [
        { bucket_start: start, avg_first_token_ms: 1_000, avg_duration_ms: 5_000 },
        { bucket_start: start + 2 * HOUR_MS, avg_first_token_ms: 2_000, avg_duration_ms: 6_000 },
        { bucket_start: start + 3 * HOUR_MS, avg_first_token_ms: null, avg_duration_ms: 7_000 },
      ],
      (ms) => new Date(ms).toISOString(),
      HOUR_MS,
    )

    expect(points).toEqual([
      { bucket: '2026-09-02T18:00:00.000Z', firstToken: 1, duration: 5 },
      { bucket: '2026-09-02T19:00:00.000Z', firstToken: null, duration: null },
      { bucket: '2026-09-02T20:00:00.000Z', firstToken: 2, duration: 6 },
      { bucket: '2026-09-02T21:00:00.000Z', firstToken: null, duration: 7 },
    ])
  })
})

describe('activity grid', () => {
  test.each([900_000, HOUR_MS, 6 * HOUR_MS, DAY_MS])(
    'omits future buckets while retaining current and past zero usage (%i ms)',
    (bucketMs) => {
      const end = new Date(2026, 8, 17, 8, 7).getTime()
      const tzOffsetMs = -new Date(end).getTimezoneOffset() * 60_000
      const current = Math.floor((end + tzOffsetMs) / bucketMs) * bucketMs - tzOffsetMs
      const grid = buildActivityGrid([], { endMs: end, spanMs: 7 * DAY_MS, bucketMs, tzOffsetMs })
      const starts = new Set(grid.cells.map((cell) => cell.start))

      expect(grid.cells.every((cell) => cell.start <= current)).toBe(true)
      expect(starts.has(current)).toBe(true)
      expect(starts.has(current - bucketMs)).toBe(true)
      expect(grid.cells.find((cell) => cell.start === current)?.tokens).toBe(0)
    },
  )

  test('fills every bucket in the window and distinguishes zero and unknown usage', () => {
    const end = Date.UTC(2026, 8, 17)
    const grid = buildActivityGrid(
      [
        {
          bucket_start: end - DAY_MS,
          total_input_tokens: 10,
          total_output_tokens: null,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
        {
          bucket_start: end - 2 * DAY_MS,
          total_input_tokens: null,
          total_output_tokens: null,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
      ],
      { endMs: end, spanMs: 3 * DAY_MS, bucketMs: DAY_MS, tzOffsetMs: 0 },
    )

    // 窗口 [end-3d, end] 覆盖 4 个日格；仅过去的窗口边缘位置补 0 值方格。
    expect(grid.cells.every((cell) => cell.start <= end)).toBe(true)
    const byStart = new Map(grid.cells.map((cell) => [cell.start, cell]))
    expect(byStart.get(end - 3 * DAY_MS)?.tokens).toBe(0)
    expect(byStart.get(end - 2 * DAY_MS)?.tokens).toBeNull()
    expect(byStart.get(end - DAY_MS)?.tokens).toBe(10)
    expect(byStart.get(end - DAY_MS)?.level).toBe(4)
    expect(byStart.get(end)?.tokens).toBe(0)
    expect(byStart.get(end)?.level).toBe(0)

    const positions = new Set(grid.cells.map((cell) => `${cell.col}:${cell.row}`))
    expect(positions.size).toBe(grid.cells.length)
    expect(grid.rowCount).toBeLessThanOrEqual(7)
    expect(grid.colCount).toBe(grid.colStarts.length)
  })

  test('minCols extends the window left by whole parent columns, newest stays rightmost', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    const history = Date.UTC(2026, 7, 25)
    const grid = buildActivityGrid(
      [
        {
          bucket_start: history,
          total_input_tokens: 5,
          total_output_tokens: null,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
      ],
      { endMs: end, spanMs: DAY_MS, bucketMs: DAY_MS, tzOffsetMs: 0, minCols: 4 },
    )

    // spanMs 只有 1 天，minCols=4 把窗口向前延伸为 4 个周列；
    // 延伸范围内的真实数据被取回，最新列保持在右端且不补未来时段。
    expect(grid.colCount).toBe(4)
    expect(grid.cells.every((cell) => cell.start <= end)).toBe(true)
    const byStart = new Map(grid.cells.map((cell) => [cell.start, cell]))
    expect(byStart.get(history)?.tokens).toBe(5)
    expect(byStart.get(history)?.col).toBe(0)
    expect(byStart.get(Date.UTC(2026, 8, 17))?.col).toBe(3)
  })

  test('sub-day buckets group into parent-period columns', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    const grid = buildActivityGrid([], { endMs: end, spanMs: 6 * HOUR_MS, bucketMs: 900_000, tzOffsetMs: 0 })
    expect(grid.cells.every((cell) => cell.start <= end)).toBe(true)
    expect(grid.rowCount).toBe(4)
    expect(grid.colCount).toBeGreaterThanOrEqual(6)
    expect(grid.colCount).toBeLessThanOrEqual(7)
    const positions = new Set(grid.cells.map((cell) => `${cell.col}:${cell.row}`))
    expect(positions.size).toBe(grid.cells.length)
  })
})

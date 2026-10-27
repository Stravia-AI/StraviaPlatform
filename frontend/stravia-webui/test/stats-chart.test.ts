import { describe, expect, test } from 'bun:test'

import {
  buildActivityGrid,
  buildAvailabilityChart,
  buildAvailabilitySummary,
  buildLatencyChart,
  planActivityGrid,
} from '../src/lib/stats-chart'

const HOUR_MS = 3_600_000
const DAY_MS = 86_400_000
const QUARTER_MS = 900_000
const MAX_BUCKETS = 512

describe('latency chart', () => {
  test('missing latency points follow the requested coarse grain, not the base quarter hour', () => {
    const start = Date.UTC(2026, 8, 2, 18, 15)
    const points = buildLatencyChart(
      [
        { bucket_start: start, avg_first_token_ms: 0, avg_output_tps: null },
        { bucket_start: start + 8_100_000, avg_first_token_ms: null, avg_output_tps: 12 },
      ],
      2_700_000,
    )
    expect(points).toEqual([
      { bucket: new Date(start), firstToken: 0, outputTps: null },
      { bucket: new Date(start + 2_700_000), firstToken: null, outputTps: null },
      { bucket: new Date(start + 5_400_000), firstToken: null, outputTps: null },
      { bucket: new Date(start + 8_100_000), firstToken: null, outputTps: 12 },
    ])
  })
  test('breaks both series across buckets without requests', () => {
    const start = Date.UTC(2026, 8, 2, 18)
    const points = buildLatencyChart(
      [
        { bucket_start: start, avg_first_token_ms: 1_000, avg_output_tps: 50 },
        { bucket_start: start + 2 * HOUR_MS, avg_first_token_ms: 2_000, avg_output_tps: null },
        { bucket_start: start + 3 * HOUR_MS, avg_first_token_ms: null, avg_output_tps: 0 },
      ],
      HOUR_MS,
    )

    expect(points).toEqual([
      { bucket: new Date(start), firstToken: 1, outputTps: 50 },
      { bucket: new Date(start + HOUR_MS), firstToken: null, outputTps: null },
      { bucket: new Date(start + 2 * HOUR_MS), firstToken: 2, outputTps: null },
      { bucket: new Date(start + 3 * HOUR_MS), firstToken: null, outputTps: 0 },
    ])
  })

  test('keeps unique time coordinates when first and last buckets share a localized label', () => {
    // 跨日 24h 窗口返回 25 个小时桶，首尾同为 19:00；显示标签重名，
    // 但 x 坐标必须保持真实时刻，否则会折回左端画出斜线。
    const start = Date.UTC(2026, 8, 2, 19)
    const points = buildLatencyChart(
      Array.from({ length: 25 }, (_, i) => ({
        bucket_start: start + i * HOUR_MS,
        avg_first_token_ms: 1_000,
        avg_output_tps: 20,
      })),
      HOUR_MS,
    )

    expect(points).toHaveLength(25)
    expect(points[0].bucket.toISOString().slice(11, 16)).toBe('19:00')
    expect(points[24].bucket.toISOString().slice(11, 16)).toBe('19:00')
    const times = points.map((point) => point.bucket.getTime())
    expect(times).toEqual([...times].sort((a, b) => a - b))
    expect(new Set(times).size).toBe(times.length)
    expect(points.every((point) => point.bucket instanceof Date)).toBe(true)
  })
})

describe('availability chart', () => {
  const row = (
    bucket_start: number,
    terminal_request_count: number,
    failed_request_count: number,
    upstream_request_count: number,
    upstream_error_count: number,
  ) => ({ bucket_start, terminal_request_count, failed_request_count, upstream_request_count, upstream_error_count })

  test('missing buckets stay null so both lines break instead of claiming 100%', () => {
    const start = Date.UTC(2026, 8, 2, 18, 15)
    const points = buildAvailabilityChart([row(start, 4, 1, 8, 2), row(start + 8_100_000, 4, 0, 4, 4)], 2_700_000)

    expect(points).toEqual([
      {
        bucket: new Date(start),
        terminalRequests: 4,
        failedRequests: 1,
        upstreamRequests: 8,
        upstreamErrors: 2,
        client: 0.75,
        upstream: 0.75,
      },
      {
        bucket: new Date(start + 2_700_000),
        terminalRequests: 0,
        failedRequests: 0,
        upstreamRequests: 0,
        upstreamErrors: 0,
        client: null,
        upstream: null,
      },
      {
        bucket: new Date(start + 5_400_000),
        terminalRequests: 0,
        failedRequests: 0,
        upstreamRequests: 0,
        upstreamErrors: 0,
        client: null,
        upstream: null,
      },
      {
        bucket: new Date(start + 8_100_000),
        terminalRequests: 4,
        failedRequests: 0,
        upstreamRequests: 4,
        upstreamErrors: 4,
        client: 1,
        upstream: 0,
      },
    ])
  })

  test('a zero denominator is unknown while a zero numerator is a real 100%', () => {
    const start = Date.UTC(2026, 8, 2, 18)
    const points = buildAvailabilityChart(
      [row(start, 0, 0, 3, 0), row(start + HOUR_MS, 5, 0, 0, 0), row(start + 2 * HOUR_MS, 5, 5, 3, 3)],
      HOUR_MS,
    )

    expect(points.map((point) => [point.client, point.upstream])).toEqual([
      [null, 1],
      [1, null],
      [0, 0],
    ])
  })

  test('the window aggregate divides overview totals instead of averaging buckets', () => {
    const start = Date.UTC(2026, 8, 2, 18)
    const points = buildAvailabilityChart([row(start, 1, 0, 1, 0), row(start + HOUR_MS, 9, 1, 9, 0)], HOUR_MS)

    expect(points.map((point) => point.client)).toEqual([1, 1 - 1 / 9])
    // 聚合只读概览的整数总量：1 − 1/10，而不是桶均值；请求总数取概览而不是折线求和。
    expect(
      buildAvailabilitySummary({
        terminal_request_count: 10,
        failed_request_count: 1,
        upstream_request_count: 10,
        upstream_error_count: 0,
      }),
    ).toEqual({ client: 0.9, upstream: 1 })
  })

  test('an empty window has no points and neutral overview aggregates', () => {
    expect(buildAvailabilityChart([], HOUR_MS)).toEqual([])
    expect(
      buildAvailabilitySummary({
        terminal_request_count: 0,
        failed_request_count: 0,
        upstream_request_count: 0,
        upstream_error_count: 0,
      }),
    ).toEqual({ client: null, upstream: null })
    expect(
      buildAvailabilitySummary({
        terminal_request_count: 0,
        failed_request_count: 0,
        upstream_request_count: 4,
        upstream_error_count: 1,
      }),
    ).toEqual({ client: null, upstream: 0.75 })
  })
})

describe('activity grid', () => {
  test('absolute windows exclude both outside edges without extra cells', () => {
    const start = Date.UTC(2026, 8, 17, 8, 15)
    const grid = buildActivityGrid(
      [
        {
          bucket_start: start - 900_000,
          total_input_tokens: 500,
          total_output_tokens: 0,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
        {
          bucket_start: start,
          total_input_tokens: 10,
          total_output_tokens: 2,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
        {
          bucket_start: start + 900_000,
          total_input_tokens: null,
          total_output_tokens: null,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
        {
          bucket_start: start + 2_700_000,
          total_input_tokens: 500,
          total_output_tokens: 0,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
      ],
      { startMs: start, endMs: start + 2_700_000, bucketMs: 900_000 },
    )

    expect(grid.cells.map(({ start: time, tokens, level }) => [time, tokens, level])).toEqual([
      [start, 12, 4],
      [start + 900_000, null, 0],
      [start + 1_800_000, 0, 0],
    ])
    expect(grid.colStarts).toEqual([start])
    expect(grid.colCount).toBe(1)
    expect(grid.rowCount).toBe(3)
  })
  test.each([2_700_000, 18_900_000, 172_800_000, 777_600_000])(
    'absolute windows preserve all 512 arbitrary-grain buckets without position collisions (%i ms)',
    (bucketMs) => {
      const start = Date.UTC(2026, 8, 17, 8, 15)
      const last = start + 511 * bucketMs
      const grid = buildActivityGrid(
        [
          {
            bucket_start: last,
            total_input_tokens: 7,
            total_output_tokens: 5,
            total_cache_read_tokens: null,
            total_cache_write_tokens: null,
          },
        ],
        { startMs: start, endMs: start + 512 * bucketMs, bucketMs },
      )
      expect(grid.cells).toHaveLength(512)
      expect(grid.cells[0].start).toBe(start)
      expect(grid.cells[511].start).toBe(last)
      expect(grid.cells[511].tokens).toBe(12)
      expect(grid.cells.reduce((sum, cell) => sum + (cell.tokens ?? 0), 0)).toBe(12)
      expect(new Set(grid.cells.map((cell) => `${cell.col}:${cell.row}`)).size).toBe(512)
      expect(grid.colCount * grid.rowCount).toBeGreaterThanOrEqual(512)
    },
  )

  test('the last coarse bucket can cover a partial period without extending the selected interval', () => {
    const start = Date.UTC(2026, 8, 17, 8, 15)
    const grid = buildActivityGrid([], { startMs: start, endMs: start + HOUR_MS, bucketMs: 2_700_000 })
    expect(grid.cells.map((cell) => cell.start)).toEqual([start, start + 2_700_000])
  })

  test('repeated DST local labels keep separate activity cells and usage', () => {
    const start = Date.UTC(2026, 10, 1, 5)
    const repeated = start + HOUR_MS
    const label = new Intl.DateTimeFormat('en-US', {
      timeZone: 'America/New_York',
      hour: '2-digit',
      minute: '2-digit',
      hourCycle: 'h23',
    })
    expect(label.format(start)).toBe('01:00')
    expect(label.format(repeated)).toBe('01:00')
    const grid = buildActivityGrid(
      [
        {
          bucket_start: start,
          total_input_tokens: 3,
          total_output_tokens: 0,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
        {
          bucket_start: repeated,
          total_input_tokens: 7,
          total_output_tokens: 0,
          total_cache_read_tokens: null,
          total_cache_write_tokens: null,
        },
      ],
      { startMs: start, endMs: start + 2 * HOUR_MS, bucketMs: 900_000 },
    )
    expect(grid.cells).toHaveLength(8)
    expect(grid.cells.filter((cell) => cell.tokens !== 0).map((cell) => [cell.start, cell.tokens])).toEqual([
      [start, 3],
      [repeated, 7],
    ])
    expect(new Set(grid.cells.map((cell) => `${cell.col}:${cell.row}`)).size).toBe(8)
    expect(grid.colStarts).toEqual([start, repeated])
  })

  test.each([
    [100, 20, null, null, 120],
    [100, 20, 80, 10, 120],
    [100, null, null, null, 100],
    [null, 20, 80, 10, 20],
    [null, null, 80, 10, null],
    [null, null, null, null, null],
    [0, 0, 80, 10, 0],
  ])(
    'counts reported input/output without adding cache subsets (%s/%s/%s/%s)',
    (input, output, read, write, expected) => {
      const end = Date.UTC(2026, 8, 17)
      const grid = buildActivityGrid(
        [
          {
            bucket_start: end,
            total_input_tokens: input,
            total_output_tokens: output,
            total_cache_read_tokens: read,
            total_cache_write_tokens: write,
          },
        ],
        { startMs: end, endMs: end + DAY_MS, bucketMs: DAY_MS },
      )
      expect(grid.cells.find((cell) => cell.start === end)?.tokens).toBe(expected)
    },
  )

  test.each([900_000, HOUR_MS, 6 * HOUR_MS, DAY_MS])(
    'includes current bucket when the selected end is its exclusive boundary (%i ms)',
    (bucketMs) => {
      const current = Date.UTC(2026, 8, 17)
      const grid = buildActivityGrid([], { startMs: current - bucketMs, endMs: current + bucketMs, bucketMs })
      expect(grid.cells.map((cell) => [cell.start, cell.tokens])).toEqual([
        [current - bucketMs, 0],
        [current, 0],
      ])
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
      { startMs: end - 3 * DAY_MS, endMs: end, bucketMs: DAY_MS },
    )

    expect(grid.cells.map((cell) => cell.start)).toEqual([end - 3 * DAY_MS, end - 2 * DAY_MS, end - DAY_MS])
    const byStart = new Map(grid.cells.map((cell) => [cell.start, cell]))
    expect(byStart.get(end - 3 * DAY_MS)?.tokens).toBe(0)
    expect(byStart.get(end - 2 * DAY_MS)?.tokens).toBeNull()
    expect(byStart.get(end - DAY_MS)?.tokens).toBe(10)
    expect(byStart.get(end - DAY_MS)?.level).toBe(4)
    expect(byStart.has(end)).toBe(false)

    const positions = new Set(grid.cells.map((cell) => `${cell.col}:${cell.row}`))
    expect(positions.size).toBe(grid.cells.length)
    expect(grid.rowCount).toBeLessThanOrEqual(7)
    expect(grid.colCount).toBe(grid.colStarts.length)
  })

  test('sub-day buckets group into parent-period columns', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    const grid = buildActivityGrid([], { startMs: end - 6 * HOUR_MS, endMs: end, bucketMs: 900_000 })
    expect(grid.cells).toHaveLength(24)
    expect(grid.cells[0].start).toBe(end - 6 * HOUR_MS)
    expect(grid.cells[23].start).toBe(end - 900_000)
    expect(grid.rowCount).toBe(4)
    expect(grid.colCount).toBe(6)
    const positions = new Set(grid.cells.map((cell) => `${cell.col}:${cell.row}`))
    expect(positions.size).toBe(grid.cells.length)
  })
})

/** 把 plan 的输出直接喂进 buildActivityGrid，返回图模型。 */
function planAndBuild(opts: { startMs: number; endMs: number; bucketMs: number; widthPx: number; heightPx: number }) {
  const plan = planActivityGrid(opts)
  return { plan, model: buildActivityGrid([], plan) }
}

describe('planActivityGrid', () => {
  const VIEWPORT = { widthPx: 640, heightPx: 224 }

  test('keeps the requested grain and extends whole columns to fill the viewport', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    const { plan, model } = planAndBuild({ startMs: end - 6 * HOUR_MS, endMs: end, bucketMs: QUARTER_MS, ...VIEWPORT })
    expect(plan.bucketMs).toBe(QUARTER_MS)
    expect(plan.endMs).toBe(end)
    // start = end - colCount*rowCount*bucket，且早于所选起点（向左补满）。
    expect(plan.startMs).toBe(end - plan.colCount * plan.rowCount * plan.bucketMs)
    expect(plan.startMs).toBeLessThan(end - 6 * HOUR_MS)
    expect(plan.colCount * plan.rowCount).toBeLessThanOrEqual(MAX_BUCKETS)
    // 完整覆盖所选范围：最后一项恰好是所选 endMs 前的最后一个桶。
    expect(model.cells.at(-1)!.start + plan.bucketMs).toBe(end)
    expect(model.cells[0].start).toBe(plan.startMs)
    expect(model.bucketMs).toBe(plan.bucketMs)
  })

  test('a resize to the same width reproduces the same plan and range', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    const first = planActivityGrid({
      startMs: end - 24 * HOUR_MS,
      endMs: end,
      bucketMs: HOUR_MS,
      widthPx: 700,
      heightPx: 224,
    })
    const second = planActivityGrid({
      startMs: end - 24 * HOUR_MS,
      endMs: end,
      bucketMs: HOUR_MS,
      widthPx: 700,
      heightPx: 225,
    })
    expect(second.bucketMs).toBe(first.bucketMs)
    expect(second.startMs).toBe(first.startMs)
    expect(second.endMs).toBe(first.endMs)
    expect(second.rowCount).toBe(first.rowCount)
  })

  test('coarsens by quarter-hour multiples instead of truncating a too-wide selection', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    // 很窄的可视宽度把 15m 粒度逼到必须合桶。
    const { plan, model } = planAndBuild({
      startMs: end - 24 * HOUR_MS,
      endMs: end,
      bucketMs: QUARTER_MS,
      widthPx: 90,
      heightPx: 224,
    })
    expect(plan.bucketMs % QUARTER_MS).toBe(0)
    expect(plan.bucketMs).toBeGreaterThan(QUARTER_MS)
    // 合并后单图仍不超过 512 桶。
    expect(plan.colCount * plan.rowCount).toBeLessThanOrEqual(MAX_BUCKETS)
    // 所选范围完整保留，不截断、不右移：最后一个桶紧贴 endMs。
    expect(model.cells.at(-1)!.start + plan.bucketMs).toBe(end)
    expect(model.cells[0].start).toBeLessThanOrEqual(end - 24 * HOUR_MS)
    // 每格与列号在网格内唯一，无重叠。
    const positions = new Set(model.cells.map((cell) => `${cell.col}:${cell.row}`))
    expect(positions.size).toBe(model.cells.length)
    model.cells.forEach((cell) => {
      expect(cell.col).toBeGreaterThanOrEqual(0)
      expect(cell.col).toBeLessThan(plan.colCount)
      expect(cell.row).toBeGreaterThanOrEqual(0)
      expect(cell.row).toBeLessThan(plan.rowCount)
    })
  })

  test('coarse grains keep cells, labels and legend within the compact chart height', () => {
    const end = Date.UTC(2026, 8, 17)
    for (const bucketMs of [QUARTER_MS, HOUR_MS, 6 * HOUR_MS, DAY_MS, 7 * DAY_MS]) {
      const plan = planActivityGrid({ startMs: end - 7 * DAY_MS, endMs: end, bucketMs, widthPx: 640, heightPx: 160 })
      expect(plan.bucketMs % QUARTER_MS).toBe(0)
      expect(plan.cellPx).toBeGreaterThanOrEqual(10)
      // clientHeight 包含 py-2 的 16px；列标签与图例另预留 38px。
      const gridHeight = plan.rowCount * plan.cellPx + (plan.rowCount - 1) * 4
      expect(gridHeight + 16 + 38).toBeLessThanOrEqual(160)
    }
  })

  test('colCount * cellPx fits inside the measured width', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    for (const widthPx of [120, 320, 480, 640, 960, 1280]) {
      const plan = planActivityGrid({
        startMs: end - 24 * HOUR_MS,
        endMs: end,
        bucketMs: HOUR_MS,
        widthPx,
        heightPx: 224,
      })
      expect(plan.colCount * plan.cellPx + (plan.colCount - 1) * 4).toBeLessThanOrEqual(widthPx)
      expect(plan.colCount).toBeGreaterThanOrEqual(1)
    }
  })

  test('stays queryable when history reaches back near the epoch', () => {
    // 自定义范围落在 1970 附近时，向左补历史不得越过有效的 15m 正边界。
    const end = 3 * QUARTER_MS
    const plan = planActivityGrid({
      startMs: QUARTER_MS,
      endMs: end,
      bucketMs: QUARTER_MS,
      widthPx: 640,
      heightPx: 224,
    })
    expect(plan.endMs).toBe(end)
    expect(plan.startMs).toBeGreaterThan(0)
    expect(plan.startMs % QUARTER_MS).toBe(0)
    expect(plan.startMs).toBe(QUARTER_MS)
    expect(plan.bucketMs).toBe(QUARTER_MS)
    const model = buildActivityGrid([], plan)
    expect(model.cells.at(-1)!.start + plan.bucketMs).toBe(end)
    expect(model.cells[0].start).toBe(plan.startMs)
  })

  test('exposes plan fields the caller can pass straight to buildActivityGrid', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    const plan = planActivityGrid({
      startMs: end - 3 * DAY_MS,
      endMs: end,
      bucketMs: 6 * HOUR_MS,
      widthPx: 640,
      heightPx: 224,
    })
    const model = buildActivityGrid([], plan)
    expect(model.colCount).toBe(plan.colCount)
    expect(model.rowCount).toBe(plan.rowCount)
    expect(model.bucketMs).toBe(plan.bucketMs)
    expect(model.cells.length).toBeLessThanOrEqual(MAX_BUCKETS)
  })

  test('wider viewports query more genuine history without changing the fitting grain', () => {
    const endMs = Date.UTC(2026, 8, 17, 12)
    const selected = { startMs: endMs - 24 * HOUR_MS, endMs, bucketMs: HOUR_MS, heightPx: 224 }
    const narrow = planActivityGrid({ ...selected, widthPx: 320 })
    const wide = planActivityGrid({ ...selected, widthPx: 640 })
    expect(narrow.bucketMs).toBe(HOUR_MS)
    expect(wide.bucketMs).toBe(HOUR_MS)
    expect(wide.startMs).toBeLessThan(narrow.startMs)
    expect(narrow.startMs).toBeLessThanOrEqual(selected.startMs)
    expect(wide.colCount).toBeGreaterThan(narrow.colCount)
    expect(planActivityGrid({ ...selected, widthPx: 320 })).toEqual(narrow)
  })

  test('reduces full columns at the 512-bucket cap instead of overflowing', () => {
    const endMs = Date.UTC(2026, 8, 17, 12)
    const plan = planActivityGrid({
      startMs: endMs - 24 * HOUR_MS,
      endMs,
      bucketMs: HOUR_MS,
      widthPx: 20_000,
      heightPx: 224,
    })
    expect(plan.bucketMs).toBe(HOUR_MS)
    expect(plan.colCount).toBe(85)
    expect(plan.rowCount).toBe(6)
    expect(plan.colCount * plan.rowCount).toBe(510)
    expect(buildActivityGrid([], plan).cells).toHaveLength(510)
    expect(plan.colCount * plan.cellPx + (plan.colCount - 1) * 4).toBeLessThanOrEqual(20_000)
  })

  test('covers non-divisible selected spans and allows a partial epoch-edge bucket', () => {
    const plan = planActivityGrid({
      startMs: QUARTER_MS,
      endMs: 6 * QUARTER_MS,
      bucketMs: 2 * QUARTER_MS,
      widthPx: 640,
      heightPx: 224,
    })
    expect(plan.startMs).toBe(QUARTER_MS)
    expect(plan.endMs).toBe(6 * QUARTER_MS)
    expect(plan.bucketMs).toBe(2 * QUARTER_MS)
    const model = buildActivityGrid([], plan)
    expect(model.cells.map((cell) => cell.start)).toEqual([QUARTER_MS, 3 * QUARTER_MS, 5 * QUARTER_MS])
    expect(model.cells.at(-1)!.start).toBeLessThan(plan.endMs)
    expect(model.cells.at(-1)!.start + plan.bucketMs).toBeGreaterThan(plan.endMs)
    expect(model.colCount).toBeLessThanOrEqual(plan.colCount)
    expect(model.cells.length).toBeLessThanOrEqual(MAX_BUCKETS)
  })

  test('zero viewport computes a bounded measurement-stage plan', () => {
    const end = Date.UTC(2026, 8, 17, 12)
    const plan = planActivityGrid({
      startMs: end - 6 * HOUR_MS,
      endMs: end,
      bucketMs: QUARTER_MS,
      widthPx: 0,
      heightPx: 0,
    })
    expect(plan.colCount).toBeGreaterThanOrEqual(1)
    expect(plan.endMs).toBe(end)
    expect(plan.cellPx).toBeGreaterThan(0)
    expect(plan.startMs).toBeGreaterThan(0)
  })
})

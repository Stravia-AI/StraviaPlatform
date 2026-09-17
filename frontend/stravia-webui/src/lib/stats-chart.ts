import type { StatsSeries } from '$lib/types'

type LatencyStats = Pick<StatsSeries, 'bucket_start' | 'avg_first_token_ms' | 'avg_duration_ms'>
type TokenStats = Pick<
  StatsSeries,
  'bucket_start' | 'total_input_tokens' | 'total_output_tokens' | 'total_cache_read_tokens' | 'total_cache_write_tokens'
>

const HOUR_MS = 3_600_000
const DAY_MS = 86_400_000

/** 后端按「本地墙面时间」对齐 bucket；此偏移与其使用的 tz_offset 参数一致。 */
export function localTzOffsetMs(): number {
  return -new Date().getTimezoneOffset() * 60 * 1000
}

export interface LatencyChartPoint {
  bucket: string
  firstToken: number | null
  duration: number | null
}

export function buildLatencyChart(
  rows: readonly LatencyStats[],
  formatBucket: (bucketStart: number) => string,
  bucketMs: number,
): LatencyChartPoint[] {
  const points: LatencyChartPoint[] = []
  let previousStart: number | undefined

  for (const row of rows) {
    const start = row.bucket_start
    if (previousStart != null && Number.isFinite(start)) {
      for (let missing = previousStart + bucketMs; missing < start; missing += bucketMs) {
        points.push({ bucket: formatBucket(missing), firstToken: null, duration: null })
      }
    }
    points.push({
      bucket: formatBucket(start),
      firstToken: row.avg_first_token_ms == null ? null : row.avg_first_token_ms / 1000,
      duration: row.avg_duration_ms == null ? null : row.avg_duration_ms / 1000,
    })
    previousStart = Number.isFinite(start) ? start : undefined
  }

  return points
}

export interface ActivityCell {
  /** Bucket 起点时刻（本地边界对齐后的真实 epoch ms）。 */
  start: number
  /** null 表示该时段有请求但 Token 用量未知；无请求为 0。 */
  tokens: number | null
  level: 0 | 1 | 2 | 3 | 4
  col: number
  row: number
}

export interface ActivityGridModel {
  cells: ActivityCell[]
  /** 每列父周期的起点时刻，用于列标签。 */
  colStarts: number[]
  colCount: number
  rowCount: number
  bucketMs: number
}

/** 与后端一致的本地对齐 bucket 起点。 */
function alignBucketStart(timeMs: number, bucketMs: number, tzOffsetMs: number): number {
  return Math.floor((timeMs + tzOffsetMs) / bucketMs) * bucketMs - tzOffsetMs
}

/**
 * 方格归属：列 = 父周期，行 = 父周期内的子序号。
 * 日格列=本地周（周一起）；6h 格列=本地日；小时格列=本地 6h 段；亚小时格列=本地小时。
 */
function columnOf(start: number, bucketMs: number): { key: number; row: number } {
  const d = new Date(start)
  const dayStart = () => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()
  if (bucketMs >= DAY_MS) {
    const row = (d.getDay() + 6) % 7
    return { key: new Date(d.getFullYear(), d.getMonth(), d.getDate() - row).getTime(), row }
  }
  if (bucketMs >= 6 * HOUR_MS) {
    return { key: dayStart(), row: Math.floor(d.getHours() / 6) }
  }
  if (bucketMs >= HOUR_MS) {
    const row = d.getHours() % 6
    return { key: new Date(d.getFullYear(), d.getMonth(), d.getDate(), d.getHours() - row).getTime(), row }
  }
  const perHour = Math.max(1, Math.round(HOUR_MS / bucketMs))
  const row = Math.min(Math.floor(d.getMinutes() / (bucketMs / 60_000)), perHour - 1)
  return { key: new Date(d.getFullYear(), d.getMonth(), d.getDate(), d.getHours()).getTime(), row }
}

function tokenTotal(row: TokenStats): number | null {
  const parts = [
    row.total_input_tokens,
    row.total_output_tokens,
    row.total_cache_read_tokens,
    row.total_cache_write_tokens,
  ]
  if (parts.every((value) => value == null)) return null
  return parts.reduce<number>((sum, value) => sum + (value ?? 0), 0)
}

/**
 * 把序列行展开为覆盖整个窗口的方格矩阵：无数据的 bucket 也占位，
 * 行 = 父周期内子序号、列 = 父周期，时间自上而下再向右流动。
 */
export function buildActivityGrid(
  rows: readonly TokenStats[],
  opts: { endMs: number; spanMs: number; bucketMs: number; tzOffsetMs: number },
): ActivityGridModel {
  const { endMs, spanMs, bucketMs, tzOffsetMs } = opts
  const totals = new Map<number, number | null>()
  for (const row of rows) totals.set(row.bucket_start, tokenTotal(row))

  const colIndex = new Map<number, number>()
  const colStarts: number[] = []
  const rawCells: Omit<ActivityCell, 'level'>[] = []
  let rowCount = 0
  let max = 0
  for (
    let start = alignBucketStart(endMs - spanMs, bucketMs, tzOffsetMs);
    start <= alignBucketStart(endMs, bucketMs, tzOffsetMs);
    start += bucketMs
  ) {
    const { key, row } = columnOf(start, bucketMs)
    let col = colIndex.get(key)
    if (col == null) {
      col = colStarts.length
      colIndex.set(key, col)
      colStarts.push(key)
    }
    rowCount = Math.max(rowCount, row + 1)
    const tokens = totals.has(start) ? totals.get(start)! : 0
    if (tokens != null) max = Math.max(max, tokens)
    rawCells.push({ start, tokens, col, row })
  }

  const cells = rawCells.map((cell): ActivityCell => ({
    ...cell,
    level:
      cell.tokens == null || cell.tokens === 0 || max === 0
        ? 0
        : (Math.min(4, Math.ceil((cell.tokens / max) * 4)) as 1 | 2 | 3 | 4),
  }))
  return { cells, colStarts, colCount: colStarts.length, rowCount, bucketMs }
}

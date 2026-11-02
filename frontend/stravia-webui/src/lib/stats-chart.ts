import type { StatsOverview, StatsSeries } from '#lib/types.js'

type LatencyStats = Pick<StatsSeries, 'bucket_start' | 'avg_first_token_ms' | 'avg_output_tps'>
type TokenStats = Pick<
  StatsSeries,
  'bucket_start' | 'total_input_tokens' | 'total_output_tokens' | 'total_cache_read_tokens' | 'total_cache_write_tokens'
>

const HOUR_MS = 3_600_000
const DAY_MS = 86_400_000

export interface LatencyChartPoint {
  /** Bucket 起点的真实时刻；显示文案由轴刻度与 tooltip 按需本地化格式化。 */
  bucket: Date
  firstToken: number | null
  outputTps: number | null
}

export function buildLatencyChart(rows: readonly LatencyStats[], bucketMs: number): LatencyChartPoint[] {
  const points: LatencyChartPoint[] = []
  let previousStart: number | undefined

  for (const row of rows) {
    const start = row.bucket_start
    if (previousStart != null && Number.isFinite(start)) {
      for (let missing = previousStart + bucketMs; missing < start; missing += bucketMs) {
        points.push({ bucket: new Date(missing), firstToken: null, outputTps: null })
      }
    }
    points.push({
      bucket: new Date(start),
      firstToken: row.avg_first_token_ms == null ? null : row.avg_first_token_ms / 1000,
      outputTps: row.avg_output_tps,
    })
    previousStart = Number.isFinite(start) ? start : undefined
  }

  return points
}

type AvailabilityStats = Pick<
  StatsSeries,
  'bucket_start' | 'terminal_request_count' | 'failed_request_count' | 'upstream_request_count' | 'upstream_error_count'
>

interface AvailabilityCounts {
  /** 已完成或最终失败、非取消、且不是错误的请求。 */
  terminalRequests: number
  /** 已终结请求中最终失败的请求。 */
  failedRequests: number
  /** 已得到上游判定结果的 Target 尝试。 */
  upstreamRequests: number
  /** 上游来源的终态失败尝试。 */
  upstreamErrors: number
}

export interface AvailabilityChartPoint extends AvailabilityCounts {
  /** Bucket 起点的真实时刻；显示文案由轴刻度与 tooltip 按需本地化格式化。 */
  bucket: Date
  /** 0–1 的客户端可用率；分母为零表示未知，保持 null 让折线断开。 */
  client: number | null
  /** 0–1 的上游可用率；分母为零表示未知，保持 null 让折线断开。 */
  upstream: number | null
}

export interface AvailabilitySummary {
  /** 0–1 的窗口客户端可用率；分母为零表示未知。 */
  client: number | null
  /** 0–1 的窗口上游可用率；分母为零表示未知。 */
  upstream: number | null
}

type AvailabilityTotals = Pick<
  StatsOverview,
  'terminal_request_count' | 'failed_request_count' | 'upstream_request_count' | 'upstream_error_count'
>

/** 可用率 = 1 − 错误 ÷ 分母。零分母表示没有可判定的流量，不能补成 100%。 */
function availabilityRatio(errors: number, total: number): number | null {
  return total > 0 ? 1 - errors / total : null
}

/**
 * 窗口聚合取概览的总量相除；概览与折线同一窗口，但聚合不经过桶边界。
 * 任一总量为 0 时返回 null，由界面显示为中性的“—”。
 */
export function buildAvailabilitySummary(totals: AvailabilityTotals): AvailabilitySummary {
  return {
    client: availabilityRatio(totals.failed_request_count, totals.terminal_request_count),
    upstream: availabilityRatio(totals.upstream_error_count, totals.upstream_request_count),
  }
}

/** 缺桶点：两个分母都为零，两条线都在此断开。 */
const ZERO_AVAILABILITY: AvailabilityCounts = {
  terminalRequests: 0,
  failedRequests: 0,
  upstreamRequests: 0,
  upstreamErrors: 0,
}

/** 从 series 构造客户端与上游两条可用率折线；缺桶补 null（断线），不插值。 */
export function buildAvailabilityChart(rows: readonly AvailabilityStats[], bucketMs: number): AvailabilityChartPoint[] {
  const points: AvailabilityChartPoint[] = []
  let previousStart: number | undefined

  for (const row of rows) {
    const start = row.bucket_start
    if (previousStart != null && Number.isFinite(start)) {
      for (let missing = previousStart + bucketMs; missing < start; missing += bucketMs) {
        points.push({ bucket: new Date(missing), ...ZERO_AVAILABILITY, client: null, upstream: null })
      }
    }
    const counts: AvailabilityCounts = {
      terminalRequests: row.terminal_request_count,
      failedRequests: row.failed_request_count,
      upstreamRequests: row.upstream_request_count,
      upstreamErrors: row.upstream_error_count,
    }
    points.push({
      bucket: new Date(start),
      ...counts,
      client: availabilityRatio(counts.failedRequests, counts.terminalRequests),
      upstream: availabilityRatio(counts.upstreamErrors, counts.upstreamRequests),
    })
    previousStart = Number.isFinite(start) ? start : undefined
  }

  return points
}

export interface ActivityCell {
  /** Bucket 起点的真实 epoch ms；不按本地墙面时间重新对齐。 */
  start: number
  /** null 表示该时段有请求但 Token 用量未知；无请求为 0。 */
  tokens: number | null
  level: 0 | 1 | 2 | 3 | 4
  col: number
  row: number
}

export interface ActivityGridModel {
  cells: ActivityCell[]
  /** 每列首个 bucket 的真实时刻，用于本地化列标签。 */
  colStarts: number[]
  colCount: number
  rowCount: number
  bucketMs: number
}

/** 每列的目标跨度；列只包含完整粒度的 bucket，不以本地日历定义身份。 */
function activityColumnMs(bucketMs: number): number {
  if (bucketMs >= DAY_MS) return 7 * DAY_MS
  if (bucketMs >= 6 * HOUR_MS) return DAY_MS
  if (bucketMs >= HOUR_MS) return 6 * HOUR_MS
  return HOUR_MS
}

/** 后端 stats series 的 UTC 基准桶；start_at 必须为正且落在基准边界上。 */
const STATS_BASE_BUCKET_MS = 900_000
/** 单图桶上限：与后端 series 的 512 呈现桶上限一致。 */
const MAX_ACTIVITY_BUCKETS = 512
/** gap-1：方格之间的间距。 */
const ACTIVITY_GAP_PX = 4
/** 容器内边距（py-2=16）与列标签、图例（≈38）的预留像素，cellPx 不得侵占。 */
const ACTIVITY_VERTICAL_OVERHEAD_PX = 54
/** 方格可读的最小与最大边长。 */
const MIN_CELL_PX = 10
const MAX_CELL_PX = 24
/** 最早可查询的 15m 对齐边界；向左补历史时 clamp 到此，避免越界或错位起点。 */
const EARLIEST_ALIGNED_START_MS = STATS_BASE_BUCKET_MS

export interface ActivityPlan {
  /** 查询起点（含）；已向左扩展成整列，且落在有效 15m 边界上。 */
  startMs: number
  /** 查询终点（不含）；严格等于所选窗口终点。 */
  endMs: number
  /** 单图统一粒度；通常保持入参粒度，所选范围放不下时按 15min 整数倍合并。 */
  bucketMs: number
  /** 方格边长；同图一致。 */
  cellPx: number
  /** 可视宽度可容纳的列数（受 512 桶上限约束）。 */
  colCount: number
  /** 满列行数，与当前粒度一致。 */
  rowCount: number
}

/**
 * 计算 Token 活动图的真实查询范围与渲染尺寸；纯函数，不发请求。
 *
 * 结束严格等于所选 endMs；粒度通常保持入参值，只有所选范围在可视宽度内放不下时
 * 才按 15min 整数倍逐步合桶（含 512 桶上限），保证完整覆盖所选范围而不截断。
 * 起点由结束点向左扩展整列以填满可视宽度，必要时 clamp 到最早有效的 15m 边界。
 * 返回值可直接用于 stats series 查询与 buildActivityGrid。
 */
export function planActivityGrid(opts: {
  startMs: number
  endMs: number
  bucketMs: number
  widthPx: number
  heightPx: number
}): ActivityPlan {
  const endMs = opts.endMs
  const spanMs = endMs - opts.startMs
  const inputBucketMs = Number.isFinite(opts.bucketMs) && opts.bucketMs > 0 ? opts.bucketMs : STATS_BASE_BUCKET_MS
  let bucketMs = Math.ceil(inputBucketMs / STATS_BASE_BUCKET_MS) * STATS_BASE_BUCKET_MS

  // 正跨度有限；粒度每次翻倍，最终一个桶即可覆盖，循环必然终止。
  for (;;) {
    const rowCount = Math.max(1, Math.ceil(activityColumnMs(bucketMs) / bucketMs))
    // 高度预算：扣除容器内边距、列标签与图例预留，再减去行间间距。
    const usableHeight = opts.heightPx - ACTIVITY_VERTICAL_OVERHEAD_PX - (rowCount - 1) * ACTIVITY_GAP_PX
    const cellPx =
      opts.heightPx > 0 && usableHeight > 0
        ? Math.max(MIN_CELL_PX, Math.min(MAX_CELL_PX, Math.floor(usableHeight / rowCount)))
        : MIN_CELL_PX
    // 可视宽度可容纳的整列数：colCount * cellPx + (colCount - 1) * gap <= widthPx。
    let colCount =
      opts.widthPx > 0 ? Math.max(1, Math.floor((opts.widthPx + ACTIVITY_GAP_PX) / (cellPx + ACTIVITY_GAP_PX))) : 1
    // 512 桶上限装不满整列时缩减列数，避免横向溢出与越界查询。
    if (colCount * rowCount > MAX_ACTIVITY_BUCKETS) {
      colCount = Math.max(1, Math.floor(MAX_ACTIVITY_BUCKETS / rowCount))
    }
    const neededBuckets = Math.ceil(spanMs / bucketMs)
    if (neededBuckets <= colCount * rowCount || bucketMs >= spanMs) {
      const startMs = Math.max(EARLIEST_ALIGNED_START_MS, endMs - colCount * rowCount * bucketMs)
      return { startMs, endMs, bucketMs, cellPx, colCount, rowCount }
    }
    // 合并相邻 bucket；翻倍保持 15min 倍数且保证收敛（spanMs/bucketMs 递减至 1）。
    bucketMs *= 2
  }
}

function tokenTotal(row: TokenStats): number | null {
  // 管理输入已扣除缓存读取；缓存分项不再加回输入与输出的合计。
  if (row.total_input_tokens == null && row.total_output_tokens == null) return null
  return (row.total_input_tokens ?? 0) + (row.total_output_tokens ?? 0)
}

/**
 * 展开绝对窗口 [startMs, endMs)：无数据的 bucket 占位，但不补窗口之外的边缘格。
 * 时间从选定起点自上而下再向右流动；本地标签重复不会合并 bucket。
 * 调用方选择有效 bucketMs，使整个窗口不超过 512 个 bucket。
 */
export function buildActivityGrid(
  rows: readonly TokenStats[],
  opts: { startMs: number; endMs: number; bucketMs: number },
): ActivityGridModel {
  const { startMs, endMs, bucketMs } = opts
  const totals = new Map<number, number | null>()
  for (const row of rows) totals.set(row.bucket_start, tokenTotal(row))

  // Even grains that do not divide an hour/day/week use distinct integer positions.
  const rowsPerColumn = Math.max(1, Math.ceil(activityColumnMs(bucketMs) / bucketMs))
  const colStarts: number[] = []
  const rawCells: Omit<ActivityCell, 'level'>[] = []
  let rowCount = 0
  let max = 0
  for (let start = startMs, index = 0; start < endMs; start += bucketMs, index++) {
    const col = Math.floor(index / rowsPerColumn)
    const row = index % rowsPerColumn
    if (row === 0) colStarts.push(start)
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

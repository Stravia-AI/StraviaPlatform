import type { StatsHourly } from '$lib/types'

const HOUR_MS = 3_600_000

type LatencyStats = Pick<StatsHourly, 'hour' | 'avg_first_token_ms' | 'avg_duration_ms'>

export interface LatencyChartPoint {
  bucket: string
  firstToken: number | null
  duration: number | null
}

function parseUtcHour(value: string): number {
  return Date.parse(value.includes('T') ? value : `${value.replace(' ', 'T')}Z`)
}

function formatUtcHour(value: number): string {
  return new Date(value).toISOString().replace('T', ' ').slice(0, 19)
}

export function buildLatencyChart(
  rows: readonly LatencyStats[],
  formatBucket: (hour: string) => string,
): LatencyChartPoint[] {
  const points: LatencyChartPoint[] = []
  let previousHour: number | undefined

  for (const row of rows) {
    const hour = parseUtcHour(row.hour)
    if (previousHour != null && Number.isFinite(hour)) {
      for (let missingHour = previousHour + HOUR_MS; missingHour < hour; missingHour += HOUR_MS) {
        points.push({ bucket: formatBucket(formatUtcHour(missingHour)), firstToken: null, duration: null })
      }
    }
    points.push({
      bucket: formatBucket(row.hour),
      firstToken: row.avg_first_token_ms == null ? null : row.avg_first_token_ms / 1000,
      duration: row.avg_duration_ms / 1000,
    })
    previousHour = Number.isFinite(hour) ? hour : undefined
  }

  return points
}

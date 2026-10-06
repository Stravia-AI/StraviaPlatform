import { getLocale, type Locale } from '$lib/paraglide/runtime.js'

type DateInput = Date | number | string | null | undefined

const dateFormatters: Record<Locale, Intl.DateTimeFormat> = {
  'en-US': new Intl.DateTimeFormat('en-US', { year: 'numeric', month: 'numeric', day: 'numeric' }),
  'zh-CN': new Intl.DateTimeFormat('zh-CN', { year: 'numeric', month: 'numeric', day: 'numeric' }),
}

const timeFormatters: Record<Locale, Intl.DateTimeFormat> = {
  'en-US': new Intl.DateTimeFormat('en-US', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hourCycle: 'h23',
  }),
  'zh-CN': new Intl.DateTimeFormat('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hourCycle: 'h23',
  }),
}

const minuteTimeFormatters: Record<Locale, Intl.DateTimeFormat> = {
  'en-US': new Intl.DateTimeFormat('en-US', { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }),
  'zh-CN': new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }),
}

const monthDayFormatters: Record<Locale, Intl.DateTimeFormat> = {
  'en-US': new Intl.DateTimeFormat('en-US', { month: 'short', day: 'numeric' }),
  'zh-CN': new Intl.DateTimeFormat('zh-CN', { month: 'short', day: 'numeric' }),
}

const numberFormatters: Record<Locale, Intl.NumberFormat> = {
  'en-US': new Intl.NumberFormat('en-US', { maximumFractionDigits: 2 }),
  'zh-CN': new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 2 }),
}

const oneDecimalFormatters: Record<Locale, Intl.NumberFormat> = {
  'en-US': new Intl.NumberFormat('en-US', { maximumFractionDigits: 1 }),
  'zh-CN': new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 1 }),
}

const integerFormatters: Record<Locale, Intl.NumberFormat> = {
  'en-US': new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 }),
  'zh-CN': new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 0 }),
}

const percentFormatters: Record<Locale, Intl.NumberFormat> = {
  'en-US': new Intl.NumberFormat('en-US', { style: 'percent', maximumFractionDigits: 2 }),
  'zh-CN': new Intl.NumberFormat('zh-CN', { style: 'percent', maximumFractionDigits: 2 }),
}

function asDate(value: DateInput): Date | null {
  if (value == null) return null
  if (value instanceof Date) return Number.isNaN(value.getTime()) ? null : value
  const date =
    typeof value === 'number' ? new Date(value) : new Date(value.includes('T') ? value : `${value.replace(' ', 'T')}Z`)
  return Number.isNaN(date.getTime()) ? null : date
}

function formatDecimal(value: number, locale: Locale, maximumFractionDigits: 1 | 2): string {
  return (maximumFractionDigits === 1 ? oneDecimalFormatters : numberFormatters)[locale].format(value)
}

export function formatNumber(value: number | null | undefined, locale = getLocale()): string {
  if (value == null || !Number.isFinite(value)) return '–'
  return numberFormatters[locale].format(value)
}

export function formatPercent(value: number | null | undefined, locale = getLocale()): string {
  if (value == null || !Number.isFinite(value)) return '–'
  return percentFormatters[locale].format(value)
}

export function formatDate(value: DateInput, locale = getLocale()): string {
  const date = asDate(value)
  return date ? dateFormatters[locale].format(date) : '–'
}

export function formatTime(value: DateInput, locale = getLocale()): string {
  const date = asDate(value)
  return date ? timeFormatters[locale].format(date) : '–'
}

export function formatLogTime(value: DateInput, locale = getLocale()): string {
  const date = asDate(value)
  return date ? `${dateFormatters[locale].format(date)} ${timeFormatters[locale].format(date)}` : '–'
}

/** “9月12日” / “Sep 12” 式短日期，用于热力格标签与提示。 */
export function formatMonthDay(value: DateInput, locale = getLocale()): string {
  const date = asDate(value)
  return date ? monthDayFormatters[locale].format(date) : '–'
}

/** 不带秒的 HH:mm，用于亚日粒度的时间点。 */
export function formatMinuteTime(value: DateInput, locale = getLocale()): string {
  const date = asDate(value)
  return date ? minuteTimeFormatters[locale].format(date) : '–'
}

export function formatDuration(ms: number | null | undefined, locale = getLocale()): string {
  if (ms == null || !Number.isFinite(ms)) return '–'
  if (ms < 1000) return `${integerFormatters[locale].format(Math.round(ms))} ms`
  if (ms < 60_000) return `${formatDecimal(ms / 1000, locale, 2)} s`
  if (ms < 3_600_000) return `${formatDecimal(ms / 60_000, locale, 1)} m`
  return `${formatDecimal(ms / 3_600_000, locale, 1)} h`
}

export function formatDurationSeconds(ms: number | null | undefined, locale = getLocale()): string {
  if (ms == null || !Number.isFinite(ms)) return '–'
  return `${formatDecimal(ms / 1000, locale, 2)} s`
}

export function formatCompactCount(value: number | null | undefined, locale = getLocale()): string {
  if (value == null || !Number.isFinite(value)) return '–'
  const count = Math.max(0, Math.floor(value))
  if (count < 1_000) return integerFormatters[locale].format(count)
  if (count < 1_000_000) return `${formatDecimal(count / 1_000, locale, 1)}K`
  return `${formatDecimal(count / 1_000_000, locale, 2)}M`
}

export const formatTokenCount = formatCompactCount

export function formatTps(tps: number | null | undefined, locale = getLocale()): string {
  if (tps == null || !Number.isFinite(tps) || tps <= 0) return '–'
  const value = tps < 100 ? formatDecimal(tps, locale, 1) : integerFormatters[locale].format(Math.round(tps))
  return `${value} tok/s`
}

export function formatBytes(bytes: number | null | undefined, locale = getLocale()): string {
  if (bytes == null || !Number.isFinite(bytes)) return '–'
  if (Math.abs(bytes) < 1024) return `${integerFormatters[locale].format(Math.round(bytes))} B`
  if (Math.abs(bytes) < 1024 * 1024) return `${formatDecimal(bytes / 1024, locale, 1)} KiB`
  if (Math.abs(bytes) < 1024 * 1024 * 1024) return `${formatDecimal(bytes / (1024 * 1024), locale, 1)} MiB`
  return `${formatDecimal(bytes / (1024 * 1024 * 1024), locale, 1)} GiB`
}

export function formatPixels(value: number | null | undefined, locale = getLocale()): string {
  if (value == null || !Number.isFinite(value)) return '–'
  return `${integerFormatters[locale].format(Math.round(value))} px`
}

export function formatList(values: readonly string[], locale = getLocale()): string {
  return values.join(locale === 'zh-CN' ? '、' : ', ')
}

/** 计算客户端可见输出速率所需的最小字段集。 */
export interface TpsInput {
  output_tokens?: number | null
  is_stream?: boolean | null
  stream_chunks_count?: number | null
  latency_upstream_ms?: number | null
  latency_total_ms?: number | null
  stream_first_chunk_ms?: number | null
}

/**
 * 净生成耗时(ms):流式 = 上游耗时 − 首字节延迟;非流式 = 上游往返耗时;
 * 缺失时回退到端到端总耗时。无法确定时返回 null。
 */
export function generationMsOf(log: TpsInput | null | undefined): number | null {
  if (!log) return null
  const isStream = log.is_stream ?? (log.stream_chunks_count ?? 0) > 0
  const upstream = log.latency_upstream_ms ?? null
  const ttfb = log.stream_first_chunk_ms ?? null
  if (isStream && upstream != null && ttfb != null) {
    const gen = upstream - ttfb
    // 与 Provider 统计共用 50 ms 下限，不根据等待占比猜测上游是否增量生成。
    if (gen < 50) return upstream
    return gen
  }
  return upstream ?? log.latency_total_ms ?? null
}

/** 净生成速度(tok/s)；输出未知或净生成耗时无效时返回 null，已知零输出保留零。 */
export function computeTps(log: TpsInput | null | undefined): number | null {
  const gen = generationMsOf(log)
  const out = log?.output_tokens
  if (out != null && out >= 0 && gen != null && gen > 0) return out / (gen / 1000)
  return null
}

export function tryPrettyJson(raw: string | null | undefined): string {
  if (raw == null) return ''
  if (typeof raw !== 'string') {
    try {
      return JSON.stringify(raw, null, 2)
    } catch {
      return String(raw)
    }
  }
  const trimmed = raw.trim()
  if (!trimmed) return raw
  try {
    const parsed: unknown = JSON.parse(trimmed)
    return JSON.stringify(parsed, null, 2)
  } catch {
    return raw
  }
}

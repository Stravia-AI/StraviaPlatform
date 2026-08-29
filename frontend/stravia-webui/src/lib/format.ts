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
  if (value == null || !Number.isFinite(value)) return '0'
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
  return `${formatDecimal(bytes / (1024 * 1024), locale, 1)} MiB`
}

export function formatPixels(value: number | null | undefined, locale = getLocale()): string {
  if (value == null || !Number.isFinite(value)) return '–'
  return `${integerFormatters[locale].format(Math.round(value))} px`
}

export function formatList(values: readonly string[], locale = getLocale()): string {
  return values.join(locale === 'zh-CN' ? '、' : ', ')
}

/** 计算 TPS 所需的最小字段集(结构兼容 `RequestLog`)。 */
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
    // 净生成耗时必须真实反映增量解码阶段。当首字节延迟占上游耗时比例过高
    // (上游未真正增量流式,而是在服务端算完后一口气 flush),gen 会趋近于 0,
    // 导致 TPS 被放大成荒诞的数值。此时回退到上游往返耗时作为生成耗时。
    const TTFB_RATIO_THRESHOLD = 0.8
    const GEN_MIN_MS = 50
    const looksNonIncremental = gen <= 0 || ttfb / upstream >= TTFB_RATIO_THRESHOLD || gen < GEN_MIN_MS
    if (looksNonIncremental) return upstream > 0 ? upstream : null
    return gen
  }
  return upstream ?? log.latency_total_ms ?? null
}

/** 净生成速度(tok/s);output ≤ 0 或净生成耗时无效时返回 null。 */
export function computeTps(log: TpsInput | null | undefined): number | null {
  const gen = generationMsOf(log)
  const out = log?.output_tokens ?? 0
  if (out > 0 && gen && gen > 0) return out / (gen / 1000)
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
    const parsed = JSON.parse(trimmed)
    return JSON.stringify(parsed, null, 2)
  } catch {
    return raw
  }
}

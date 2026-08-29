import { describe, expect, test } from 'bun:test'
import * as m from '../src/lib/paraglide/messages.js'
import {
  formatBytes,
  formatCompactCount,
  formatDate,
  formatDuration,
  formatDurationSeconds,
  formatNumber,
  formatPercent,
  formatPixels,
  formatTime,
  formatTokenCount,
  formatTps,
} from '../src/lib/format'

const EN = 'en-US' as const
const ZH = 'zh-CN' as const

describe('locale-aware standalone values', () => {
  test('formats grouped numbers and percentages with an explicit locale', () => {
    expect(formatNumber(1_234_567.89, EN)).toBe('1,234,567.89')
    expect(formatNumber(1_234_567.89, ZH)).toBe('1,234,567.89')
    expect(formatPercent(0.125, EN)).toBe('12.5%')
    expect(formatPercent(0.125, ZH)).toBe('12.5%')
  })

  test('uses locale date order and a 24-hour local time', () => {
    const value = new Date(2026, 0, 2, 23, 4, 5)
    expect(formatDate(value, EN)).toBe('1/2/2026')
    expect(formatDate(value, ZH)).toBe('2026/1/2')
    expect(formatTime(value, EN)).toBe('23:04:05')
    expect(formatTime(value, ZH)).toBe('23:04:05')
  })
})

describe('stable engineering values', () => {
  test('keeps compact count thresholds and K/M suffixes stable', () => {
    expect(formatCompactCount(999, EN)).toBe('999')
    expect(formatCompactCount(1_000, EN)).toBe('1K')
    expect(formatCompactCount(1_250, ZH)).toBe('1.3K')
    expect(formatCompactCount(1_000_000, ZH)).toBe('1M')
    expect(formatTokenCount(1_250_000, EN)).toBe('1.25M')
  })

  test('formats durations, throughput, bytes, and pixels with standard units', () => {
    expect(formatDuration(999, EN)).toBe('999 ms')
    expect(formatDuration(1_500, ZH)).toBe('1.5 s')
    expect(formatDurationSeconds(436, ZH)).toBe('0.44 s')
    expect(formatDurationSeconds(13_030, EN)).toBe('13.03 s')
    expect(formatTps(42.25, EN)).toBe('42.3 tok/s')
    expect(formatBytes(1_536, ZH)).toBe('1.5 KiB')
    expect(formatBytes(1_572_864, EN)).toBe('1.5 MiB')
    expect(formatPixels(1024, ZH)).toBe('1,024 px')
  })

})

describe('localized count sentences', () => {
  test('uses English plurals and Chinese word order', () => {
    expect(String(m.provider_model_catalog_used_by_models({ count: 1 }, { locale: EN }))).toBe('Used by 1 model')
    expect(String(m.provider_model_catalog_used_by_models({ count: 2 }, { locale: EN }))).toBe('Used by 2 models')
    expect(String(m.provider_model_catalog_used_by_models({ count: 2 }, { locale: ZH }))).toBe('用于 2 个模型')
  })

  test('selects singular and plural nouns for catalog refresh summaries', () => {
    expect(
      String(m.provider_editor_catalog_refresh_summary({ provider_count: 1, model_count: 1 }, { locale: EN })),
    ).toBe('Service list updated: 1 service and 1 model.')
    expect(
      String(m.provider_editor_catalog_refresh_summary({ provider_count: 1, model_count: 2 }, { locale: EN })),
    ).toBe('Service list updated: 1 service and 2 models.')
    expect(
      String(m.provider_editor_catalog_refresh_summary({ provider_count: 2, model_count: 1 }, { locale: EN })),
    ).toBe('Service list updated: 2 services and 1 model.')
    expect(
      String(m.provider_editor_catalog_refresh_summary({ provider_count: 2, model_count: 2 }, { locale: ZH })),
    ).toBe('服务列表已更新：2 个服务、2 个模型。')
  })
})

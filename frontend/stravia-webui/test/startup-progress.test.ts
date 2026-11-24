import { describe, expect, test } from 'bun:test'
import * as m from '../src/lib/paraglide/messages.js'
import { getLocale, overwriteGetLocale, type Locale } from '../src/lib/paraglide/runtime.js'
import { startupPhaseLabel } from '../src/lib/startup-progress'

function withLocale<T>(locale: Locale, fn: () => T): T {
  const original = getLocale
  overwriteGetLocale(() => locale)
  try {
    return fn()
  } finally {
    overwriteGetLocale(original)
  }
}

describe('startupPhaseLabel', () => {
  test.each([
    ['en-US', 'Rebuilding history index'],
    ['zh-CN', '正在重建历史索引'],
  ] as const)('localizes history_index in %s without falling back to backend label', (locale, expected) => {
    const progress = { phase: 'history_index', label: 'fallback', completed: 0, total: 100 }
    const label = withLocale(locale, () => startupPhaseLabel(progress))
    expect(label).toBe(expected)
    expect(label).toBe(m.startup_phase_history_index({}, { locale }))
    expect(label).not.toBe(progress.label)
  })

  test('falls back to backend label for unknown phases', () => {
    const progress = { phase: 'unknown_future_phase', label: 'fallback', completed: 0, total: null }
    expect(startupPhaseLabel(progress)).toBe('fallback')
  })
})

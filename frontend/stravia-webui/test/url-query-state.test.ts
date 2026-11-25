import { describe, expect, it } from 'bun:test'
import { parseEnumParam, sameRelativeUrl, withQueryParams } from '#lib/url-query-state.js'

describe('url-query-state', () => {
  it('parses valid enum params and falls back on missing or invalid values', () => {
    const allowed = ['records', 'rules', 'test'] as const
    expect(parseEnumParam(new URLSearchParams('tab=rules'), 'tab', allowed, 'records')).toBe('rules')
    expect(parseEnumParam(new URLSearchParams('tab=unknown'), 'tab', allowed, 'records')).toBe('records')
    expect(parseEnumParam(new URLSearchParams(''), 'tab', allowed, 'records')).toBe('records')
  })

  it('sets non-default params, deletes defaults/empty values, and preserves unrelated params', () => {
    const base = new URL('http://localhost/logs?interaction=abc&tab=client-errors#top')

    const toUpstream = withQueryParams(base, { tab: 'upstream-errors' }, { tab: 'interactions' })
    expect(toUpstream.search).toBe('?interaction=abc&tab=upstream-errors')
    expect(toUpstream.hash).toBe('#top')
    expect(base.search).toBe('?interaction=abc&tab=client-errors')

    const toDefault = withQueryParams(toUpstream, { tab: 'interactions' }, { tab: 'interactions' })
    expect(toDefault.search).toBe('?interaction=abc')

    const cleared = withQueryParams(toDefault, { interaction: null })
    expect(cleared.search).toBe('')
  })

  it('detects equivalent relative URLs', () => {
    const a = new URL('http://localhost/connect?tab=code')
    const b = new URL('http://localhost/connect?tab=code')
    const c = new URL('http://localhost/connect?tab=mcp')
    expect(sameRelativeUrl(a, b)).toBe(true)
    expect(sameRelativeUrl(a, c)).toBe(false)
  })
})

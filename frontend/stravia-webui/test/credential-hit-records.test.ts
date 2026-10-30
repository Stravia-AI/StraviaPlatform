import { describe, expect, test } from 'bun:test'
import {
  HIT_RECORDS_REFRESH_MS,
  appendHitRecords,
  displayRuleSets,
  entryContext,
  entrySegments,
  hitQueryFailure,
  hitRecordsRefreshDelay,
  hitSourceKind,
  maskedEntryText,
  previewHits,
  refreshHitRecords,
  requestLogPath,
  uniqueHits,
  uniqueReuses,
} from '../src/lib/credential-hit-records'
import type { CredentialHitMatch, CredentialHitReuse, CredentialDiscoveryRecord } from '../src/lib/types'

function record(id: string, createdAt: number): CredentialDiscoveryRecord {
  return {
    id,
    created_at: createdAt,
    interaction_id: `interaction-${id}`,
    run_id: `run-${id}`,
    api_key_name: 'key',
    rule_sets: ['builtin'],
    rule_ids: ['github-pat'],
    hits: [{ rule_id: 'github-pat', source_type: 'user_message', latest_input: true }],
    omitted_hits: 0,
    linkable: true,
  }
}

const ids = (records: CredentialDiscoveryRecord[]) => records.map((item) => item.id)

const page = (items: CredentialDiscoveryRecord[], next: string | null) => ({ items, next_cursor: next })
const loadedRows = (items: CredentialDiscoveryRecord[], cursor: string | null = 'tail') => ({
  items,
  nextCursor: cursor,
})

function match(overrides: Partial<CredentialHitMatch> = {}): CredentialHitMatch {
  return {
    rule_id: 'github-pat',
    start: 0,
    end: 1,
    start_line: 1,
    start_column: 1,
    end_line: 1,
    end_column: 2,
    hit: false,
    ...overrides,
  }
}

describe('refreshHitRecords', () => {
  test('puts new mappings in front and keeps the loaded rows and cursor untouched', () => {
    const loaded = loadedRows([record('c', 300), record('b', 200), record('a', 100)])
    const merged = refreshHitRecords(loaded, page([record('e', 500), record('d', 400), record('c', 300)], 'head'))
    expect(ids(merged.items)).toEqual(['e', 'd', 'c', 'b', 'a'])
    expect(merged.nextCursor).toBe('tail')
  })

  test('does not duplicate a mapping that is already in the loaded rows', () => {
    const loaded = loadedRows([record('c', 300), record('b', 200), record('a', 100)])
    const merged = refreshHitRecords(loaded, page([record('a', 600), record('c', 300)], 'head'))
    expect(ids(merged.items)).toEqual(['a', 'c', 'b'])
    expect(merged.items.filter((item) => item.id === 'a')).toHaveLength(1)
  })

  test('starts over from the first page when a whole page of new mappings arrived between refreshes', () => {
    const loaded = loadedRows([record('c', 300), record('b', 200)], 'old-tail')
    const merged = refreshHitRecords(loaded, page([record('z', 900), record('y', 800)], 'new-tail'))
    expect(ids(merged.items)).toEqual(['z', 'y'])
    expect(merged.nextCursor).toBe('new-tail')
  })

  test('trusts the first page when it is the whole list, dropping rows that expired and the stale cursor', () => {
    const loaded = loadedRows([record('b', 200), record('a', 100)])
    const merged = refreshHitRecords(loaded, page([record('b', 200)], null))
    expect(ids(merged.items)).toEqual(['b'])
    expect(merged.nextCursor).toBeNull()
  })

  test('adopts the first page and its cursor when nothing was loaded yet', () => {
    const merged = refreshHitRecords(loadedRows([], null), page([record('a', 100)], 'next'))
    expect(ids(merged.items)).toEqual(['a'])
    expect(merged.nextCursor).toBe('next')
  })
})

describe('appendHitRecords', () => {
  test('skips rows that a refresh already moved to the head while the page was loading', () => {
    const loaded = [record('x', 900), record('c', 300)]
    expect(ids(appendHitRecords(loaded, [record('c', 300), record('b', 200)]))).toEqual(['x', 'c', 'b'])
  })
})

describe('hitRecordsRefreshDelay', () => {
  test('waits out the remainder of the refresh period', () => {
    expect(hitRecordsRefreshDelay(1_000, 1_000 + 10_000)).toBe(HIT_RECORDS_REFRESH_MS - 10_000)
  })

  test('refreshes immediately once the period has already elapsed', () => {
    expect(hitRecordsRefreshDelay(1_000, 1_000 + HIT_RECORDS_REFRESH_MS + 5)).toBe(0)
  })
})

describe('hitSourceKind', () => {
  test('separates replayed user messages from the latest input', () => {
    expect(hitSourceKind({ source_type: 'user_message', latest_input: false })).toBe('history_user_message')
    expect(hitSourceKind({ source_type: 'user_message', latest_input: true })).toBe('user_message')
  })

  test('keeps user messages from legacy events, which carry no locator, as plain user messages', () => {
    expect(hitSourceKind({ source_type: 'user_message' })).toBe('user_message')
  })

  test('leaves other sources unchanged', () => {
    expect(hitSourceKind({ source_type: 'tool_result', latest_input: false })).toBe('tool_result')
  })
})

describe('uniqueHits', () => {
  test('shows a rule once per displayed source even when legacy and new events disagree on the latest input flag', () => {
    const hits = [
      { rule_id: 'github-pat', source_type: 'user_message' },
      { rule_id: 'github-pat', source_type: 'user_message', latest_input: true },
      { rule_id: 'github-pat', source_type: 'user_message', latest_input: false },
      { rule_id: 'github-pat', source_type: 'tool_result' },
    ]
    expect(uniqueHits(hits)).toEqual([hits[0], hits[2], hits[3]])
  })
})

describe('previewHits', () => {
  test('collapses extra pairs and adds the locators omitted during capture', () => {
    const hits = [
      { rule_id: 'a', source_type: 'tool_result' },
      { rule_id: 'a', source_type: 'tool_result' },
      { rule_id: 'a', source_type: 'tool_arguments' },
      { rule_id: 'b', source_type: 'tool_result' },
      { rule_id: 'c', source_type: 'user_message', latest_input: true },
    ]
    const preview = previewHits(hits, 4)
    expect(preview.hits).toEqual([hits[0], hits[2], hits[3]])
    expect(preview.hidden).toBe(5)
  })

  test('shows everything and no counter when the record fits', () => {
    const preview = previewHits([{ rule_id: 'a', source_type: 'tool_result' }], 0)
    expect(preview).toEqual({ hits: [{ rule_id: 'a', source_type: 'tool_result' }], hidden: 0 })
  })
})

describe('displayRuleSets', () => {
  test('deduplicates and puts built-in before custom', () => {
    expect(displayRuleSets(['custom', 'builtin', 'custom'])).toEqual(['builtin', 'custom'])
    expect(displayRuleSets([])).toEqual([])
  })
})

describe('entryContext', () => {
  const text = Array.from({ length: 10 }, (_, index) => `line ${index + 1}`).join('\n')

  test('takes three lines before and after the match line', () => {
    const context = entryContext(text, 5)
    expect(context.lines.map((line) => line.number)).toEqual([2, 3, 4, 5, 6, 7, 8])
    expect(context.lines[3].text).toBe('line 5')
    expect(context.hasBefore).toBe(true)
    expect(context.hasAfter).toBe(true)
  })

  test('clamps at the start and the end of the entry', () => {
    expect(entryContext(text, 1)).toEqual({
      lines: [
        { number: 1, text: 'line 1' },
        { number: 2, text: 'line 2' },
        { number: 3, text: 'line 3' },
        { number: 4, text: 'line 4' },
      ],
      hasBefore: false,
      hasAfter: true,
    })
    const tail = entryContext(text, 10)
    expect(tail.lines.map((line) => line.number)).toEqual([7, 8, 9, 10])
    expect(tail.hasAfter).toBe(false)
  })

  test('strips CRLF carriage returns and keeps empty entries at the boundary', () => {
    expect(entryContext('a\r\nb\r\nc', 2).lines).toEqual([
      { number: 1, text: 'a' },
      { number: 2, text: 'b' },
      { number: 3, text: 'c' },
    ])
    expect(entryContext('', 1)).toEqual({ lines: [{ number: 1, text: '' }], hasBefore: false, hasAfter: false })
  })
})

describe('entrySegments', () => {
  test('splits plain text, other matches and hit matches by UTF-16 offsets', () => {
    const text = 'abSECRETcd'
    const matches = [match({ start: 2, end: 8, hit: true }), match({ start: 0, end: 2, hit: false, rule_id: 'other' })]
    const segments = entrySegments(text, matches)
    expect(segments).toEqual([
      { text: 'ab', kind: 'match', matchIndex: 1, revealed: false },
      { text: 'SECRET', kind: 'hit', matchIndex: 0, revealed: false },
      { text: 'cd', kind: 'plain', matchIndex: null, revealed: false },
    ])
  })

  test('marks a hit as revealed only when its index is in the revealed set', () => {
    const segments = entrySegments('SECRET', [match({ end: 6, hit: true })], new Set([0]))
    expect(segments).toEqual([{ text: 'SECRET', kind: 'hit', matchIndex: 0, revealed: true }])
  })

  test('clamps overlapping matches to the remaining range', () => {
    const segments = entrySegments('abcdefghij', [
      match({ start: 0, end: 6 }),
      match({ start: 4, end: 10, rule_id: 'second' }),
    ])
    expect(segments).toEqual([
      { text: 'abcdef', kind: 'match', matchIndex: 0, revealed: false },
      { text: 'ghij', kind: 'match', matchIndex: 1, revealed: false },
    ])
  })

  test('ignores empty, negative and out-of-range matches', () => {
    const text = 'abc'
    expect(entrySegments(text, [])).toEqual([{ text: 'abc', kind: 'plain', matchIndex: null, revealed: false }])
    expect(
      entrySegments(text, [
        match({ start: 1, end: 1 }),
        match({ start: -3, end: -1 }),
        match({ start: 5, end: 8 }),
        match({ start: 0, end: 2 }),
      ]),
    ).toEqual([
      { text: 'ab', kind: 'match', matchIndex: 3, revealed: false },
      { text: 'c', kind: 'plain', matchIndex: null, revealed: false },
    ])
  })

  test('returns nothing for an empty entry', () => {
    expect(entrySegments('', [match({ start: 0, end: 1 })])).toEqual([])
  })
})

describe('maskedEntryText', () => {
  test('masks hit fragments but keeps other matches and plain text', () => {
    const text = 'a SECRET b OTHER c'
    const matches = [
      match({ start: 2, end: 8, hit: true }),
      match({ start: 11, end: 16, hit: false, rule_id: 'other' }),
    ]
    expect(maskedEntryText(entrySegments(text, matches))).toBe('a •••••• b OTHER c')
  })

  test('shows the plaintext again once the hit is revealed', () => {
    const matches = [match({ start: 2, end: 8, hit: true })]
    expect(maskedEntryText(entrySegments('a SECRET b', matches, new Set([0])))).toBe('a SECRET b')
  })

  test('keeps the line numbering when a masked hit spans lines', () => {
    const text = 'x\nSEC\nRET\ny'
    const masked = maskedEntryText(entrySegments(text, [match({ start: 2, end: 9, hit: true })]))
    expect(masked).toBe('x\n••••••\n\ny')
    expect(masked.split('\n')).toHaveLength(text.split('\n').length)
  })
})

describe('uniqueReuses', () => {
  const reuse = (overrides: Partial<CredentialHitReuse> = {}): CredentialHitReuse => ({
    interaction_id: 'interaction-1',
    run_id: 'run-1',
    api_key_name: 'key',
    source_type: 'tool_arguments',
    tool_id: 'tool-1',
    latest_input: null,
    last_reused_at: 100,
    ...overrides,
  })

  test('keeps the latest occurrence of each location and sorts newest first', () => {
    const items = [
      reuse({ last_reused_at: 100 }),
      reuse({ last_reused_at: 300 }),
      reuse({ interaction_id: 'interaction-2', last_reused_at: 200 }),
      reuse({ source_type: 'user_message', tool_id: null, latest_input: true, last_reused_at: 250 }),
    ]
    expect(uniqueReuses(items).map((item) => [item.interaction_id, item.last_reused_at])).toEqual([
      ['interaction-1', 300],
      ['interaction-1', 250],
      ['interaction-2', 200],
    ])
  })

  test('treats different tools at one interaction as different locations', () => {
    const items = [reuse({ tool_id: 'tool-1' }), reuse({ tool_id: 'tool-2', last_reused_at: 200 })]
    expect(uniqueReuses(items)).toHaveLength(2)
  })
})

describe('requestLogPath', () => {
  test('links to the request observation and keeps the credential highlight entry', () => {
    expect(requestLogPath('interaction/a b')).toBe('/logs?interaction=interaction%2Fa%20b&highlight=credential-hits')
  })
})

describe('hitQueryFailure', () => {
  test('separates an expired record from a temporary failure', () => {
    expect(hitQueryFailure({ status: 404 })).toBe('gone')
    expect(hitQueryFailure(Object.assign(new Error('missing'), { status: 404 }))).toBe('gone')
    expect(hitQueryFailure({ status: 503 })).toBe('failed')
    expect(hitQueryFailure(new Error('network'))).toBe('failed')
    expect(hitQueryFailure(null)).toBe('failed')
  })
})

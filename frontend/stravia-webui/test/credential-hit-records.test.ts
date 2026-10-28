import { describe, expect, test } from 'bun:test'
import {
  HIT_RECORDS_REFRESH_MS,
  appendHitRecords,
  hitRecordsRefreshDelay,
  hitSourceKind,
  refreshHitRecords,
  uniqueHits,
} from '../src/lib/credential-hit-records'

import type { CredentialDiscoverySummary } from '../src/lib/types'

function record(id: string, discoveredAt: number): CredentialDiscoverySummary {
  return {
    interaction_id: id,
    api_key_name: 'key',
    discovered_at: discoveredAt,
    new_credential_count: 1,
    created: [{ rule_id: 'github-pat', source_type: 'user_message', latest_input: true }],
    reused: [],
    status: 'completed',
    observation_gap: false,
  }
}

const ids = (records: CredentialDiscoverySummary[]) => records.map((item) => item.interaction_id)

const page = (items: CredentialDiscoverySummary[], next: string | null) => ({ items, next_cursor: next })
const loadedRows = (items: CredentialDiscoverySummary[], cursor: string | null = 'tail') => ({
  items,
  nextCursor: cursor,
})

describe('refreshHitRecords', () => {
  test('puts newly hit interactions on top and keeps the rows below and the cursor untouched', () => {
    const loaded = loadedRows([record('c', 300), record('b', 200), record('a', 100)])
    const merged = refreshHitRecords(loaded, page([record('e', 500), record('d', 400), record('c', 300)], 'head'))
    expect(ids(merged.items)).toEqual(['e', 'd', 'c', 'b', 'a'])
    expect(merged.nextCursor).toBe('tail')
  })

  test('moves an interaction that received a newer hit instead of duplicating it', () => {
    const loaded = loadedRows([record('c', 300), record('b', 200), record('a', 100)])
    const merged = refreshHitRecords(loaded, page([record('a', 600), record('c', 300)], 'head'))
    expect(ids(merged.items)).toEqual(['a', 'c', 'b'])
    expect(merged.items[0].discovered_at).toBe(600)
  })

  test('orders equal timestamps by interaction id, newest id first', () => {
    const merged = refreshHitRecords(loadedRows([record('a', 100)]), page([record('b', 100), record('a', 100)], 'head'))
    expect(ids(merged.items)).toEqual(['b', 'a'])
  })

  test('starts over from the first page when a whole page of new hits arrived between refreshes', () => {
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
    expect(hitSourceKind({ rule_id: 'r', source_type: 'user_message', latest_input: false })).toBe(
      'history_user_message',
    )
    expect(hitSourceKind({ rule_id: 'r', source_type: 'user_message', latest_input: true })).toBe('user_message')
  })

  test('keeps user messages from legacy events, which carry no locator, as plain user messages', () => {
    expect(hitSourceKind({ rule_id: 'r', source_type: 'user_message' })).toBe('user_message')
  })

  test('leaves other sources unchanged', () => {
    expect(hitSourceKind({ rule_id: 'r', source_type: 'tool_result', latest_input: false })).toBe('tool_result')
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

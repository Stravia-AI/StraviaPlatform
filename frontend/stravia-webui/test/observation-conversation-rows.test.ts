import { describe, expect, test } from 'bun:test'
import type { ObservationChatMessage, ObservationConversationBlock } from '../src/lib/observation-conversation'
import {
  estimateConversationRow,
  observationConversationRows,
  type ConversationRow,
  type ConversationSectionCache,
} from '../src/lib/observation-conversation-rows'

function message(id: string, role: 'user' | 'assistant', text = '', at = 1): ObservationChatMessage {
  return { id, role, text, at, model: role === 'assistant' ? 'Model' : '', live: false, unsaved: false }
}
function text(id: string, source: string, live = false): ObservationConversationBlock {
  return { kind: 'text', id, text: source, live, at: 1 }
}
function rowsOf(
  groups: ObservationChatMessage[][],
  blocks: Record<string, ObservationConversationBlock[]> = {},
  cache: ConversationSectionCache = new Map(),
  liveActive = false,
): ConversationRow[] {
  return observationConversationRows(groups, new Map(Object.entries(blocks)), cache, liveActive)
}
// 超过 8192 字符的 token 才会让词法分段另起一段。
const large = (label: string) => `${label} ${'x'.repeat(9000)}`

describe('observation conversation rows', () => {
  test('splits one user input into recyclable sections inside a single bubble and closes every message with its time', () => {
    const rows = rowsOf([[message('u1', 'user', `${large('first')}\n\n${large('second')}\n\ntail`, 7)]])
    const sections = rows.filter((row) => row.kind === 'section')
    expect(sections.length).toBeGreaterThan(1)
    expect(sections.map((row) => row.bubble)).toEqual(
      sections.map((_, i) => (i === 0 ? 'first' : i === sections.length - 1 ? 'last' : 'middle')),
    )
    expect(sections.map((row) => row.leading)).toEqual(sections.map((_, i) => i === 0))
    expect(sections.every((row) => row.wide)).toBe(true)
    // 分段之间是连续文档，不留段间距；气泡最后一段之后才接时间行。
    expect(sections.slice(0, -1).every((row) => row.space === 'joined')).toBe(true)
    const meta = rows.at(-1)
    expect(meta).toMatchObject({ kind: 'meta', at: 7, space: 'end', group: 'u1' })
    expect(new Set(rows.map((row) => row.key)).size).toBe(rows.length)
  })

  test('keeps a short user input as one standalone bubble', () => {
    const [section, meta] = rowsOf([[message('u1', 'user', 'hello')]])
    expect(section).toMatchObject({ kind: 'section', bubble: 'only', wide: false, leading: true, allowImages: false })
    expect(meta.kind).toBe('meta')
  })

  test('separates assistant blocks with block spacing and carries the model name on every row', () => {
    const activity: ObservationConversationBlock = {
      kind: 'thinking',
      id: 'think',
      at: 1,
      text: 'considering',
      live: false,
    }
    const rows = rowsOf([[message('a1', 'assistant')]], { a1: [activity, text('t1', 'answer')] })
    expect(rows.map((row) => [row.kind, row.space])).toEqual([
      ['activity', 'block'],
      ['section', 'end'],
      ['meta', 'end'],
    ])
    expect(rows.every((row) => row.model === 'Model' && row.group === 'a1' && !row.user)).toBe(true)
    expect(rows[1]).toMatchObject({ allowImages: true, leading: false })
  })

  test('streams a receiving text block as one live row and only splits it once it is stored', () => {
    const blocks = { a1: [text('t1', 'streaming', true)] }
    expect(rowsOf([[message('a1', 'assistant')]], blocks, new Map(), true).map((row) => row.kind)).toEqual([
      'live',
      'meta',
    ])
    expect(rowsOf([[message('a1', 'assistant')]], blocks, new Map(), false).map((row) => row.kind)).toEqual([
      'section',
      'meta',
    ])
  })

  test('reuses sections of unchanged text and forgets blocks that left the conversation', () => {
    const cache: ConversationSectionCache = new Map()
    const group = [message('a1', 'assistant')]
    const first = rowsOf([group], { a1: [text('t1', 'stable'), text('t2', 'gone')] }, cache)
    const second = rowsOf([group], { a1: [text('t1', 'stable')] }, cache)
    const stable = (rows: ConversationRow[]) => rows.find((row) => row.kind === 'section' && row.key.includes('t1'))
    const before = stable(first)
    const after = stable(second)
    expect(before?.kind === 'section' && after?.kind === 'section' && before.section === after.section).toBe(true)
    expect([...cache.keys()]).toEqual(['text:a1:t1'])
    const changed = rowsOf([group], { a1: [text('t1', 'edited')] }, cache)
    const rewritten = stable(changed)
    expect(rewritten?.kind === 'section' && rewritten.section.source).toBe('edited')
  })

  test('estimates taller rows for longer text and never returns a collapsed row', () => {
    const short = rowsOf([[message('a1', 'assistant')]], { a1: [text('t1', 'one line')] })[0]
    const long = rowsOf([[message('a1', 'assistant')]], { a1: [text('t1', 'word '.repeat(400))] })[0]
    const empty = rowsOf([[message('a1', 'assistant')]], { a1: [text('t1', '')] })[0]
    expect(estimateConversationRow(long)).toBeGreaterThan(estimateConversationRow(short))
    expect(estimateConversationRow(empty)).toBeGreaterThan(0)
    for (const row of rowsOf([[message('a1', 'assistant')]], { a1: [text('t1', 'x')] }))
      expect(estimateConversationRow(row)).toBeGreaterThan(0)
  })
})

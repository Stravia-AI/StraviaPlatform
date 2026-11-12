import { deferredMarkdownBlocks, type DeferredMarkdownBlock } from './markdown'
import type { ObservationActivity } from './observation-activities'
import type { ObservationChatMessage, ObservationConversationBlock } from './observation-conversation'

/**
 * 行与下一行的间距，由行自己的内边距承担，虚拟化器才能量到整段版面：
 * `joined` 是同一块文档的后续分段，无间距；`block` 是下一块内容；`end` 是消息最后的内容行，其后接时间行。
 */
export type ConversationSpace = 'joined' | 'block' | 'end'

interface RowBase {
  key: string
  /** 同一条消息的行共用一个 `<article>`。 */
  group: string
  user: boolean
  /** 助手消息的模型名；用户消息为空。 */
  model: string
  /** 用户消息拆成多段时，气泡占满最大宽度，各段才不会因内容宽窄而参差。 */
  wide: boolean
  space: ConversationSpace
}

type TextBlock = Extract<ObservationConversationBlock, { kind: 'text' }>

export type ConversationRow = RowBase &
  (
    | {
        kind: 'section'
        messageId: string
        section: DeferredMarkdownBlock
        /** 同一块正文的后续分段，与上一段是连续文档。 */
        continued: boolean
        /** 这条消息的第一段；凭据命中标记挂在这里。 */
        leading: boolean
        /** 用户气泡内的位置，气泡的内边距由首尾行承担。 */
        bubble?: 'first' | 'middle' | 'last' | 'only'
        allowImages: boolean
      }
    | { kind: 'live'; block: TextBlock }
    | { kind: 'activity'; block: ObservationActivity }
    | { kind: 'meta'; at: number }
  )

interface CachedSections {
  text: string
  sections: DeferredMarkdownBlock[]
}

/** 同一段正文的词法分段只做一次：来源更新频繁，但多数块的文本不变。 */
export type ConversationSectionCache = Map<string, CachedSections>

function sectionsOf(cache: ConversationSectionCache, used: Set<string>, key: string, text: string) {
  used.add(key)
  const cached = cache.get(key)
  if (cached?.text === text) return cached.sections
  const sections = deferredMarkdownBlocks(text)
  cache.set(key, { text, sections })
  return sections
}

export function observationConversationRows(
  groups: readonly (readonly ObservationChatMessage[])[],
  responseBlocks: ReadonlyMap<string, readonly ObservationConversationBlock[]>,
  cache: ConversationSectionCache,
  /** 仍在接收的正文用单行承载逐字显示；落盘后才切成可回收的分段。 */
  liveActive: boolean,
): ConversationRow[] {
  const rows: ConversationRow[] = []
  const used = new Set<string>()
  for (const group of groups) {
    const id = group[0].id
    const user = group[0].role === 'user'
    const content: ConversationRow[] = []
    const base = {
      group: id,
      user,
      model: user ? '' : group[0].model,
      wide: false,
      space: 'block' as ConversationSpace,
    }
    for (const message of group) {
      if (user) {
        const sections = sectionsOf(cache, used, `user:${message.id}`, message.text)
        sections.forEach((section, index) => {
          content.push({
            ...base,
            key: `user:${message.id}:${section.id}`,
            kind: 'section',
            messageId: message.id,
            section,
            continued: index > 0,
            leading: index === 0,
            allowImages: false,
          })
        })
        continue
      }
      for (const block of responseBlocks.get(message.id) ?? []) {
        const key = `${message.id}:${block.id}`
        if (block.kind !== 'text') {
          content.push({ ...base, key: `activity:${key}`, kind: 'activity', block })
        } else if (block.live && liveActive) {
          content.push({ ...base, key: `text:${key}`, kind: 'live', block })
        } else {
          sectionsOf(cache, used, `text:${key}`, block.text).forEach((section, index) => {
            content.push({
              ...base,
              key: `text:${key}:${section.id}`,
              kind: 'section',
              messageId: message.id,
              section,
              continued: index > 0,
              leading: false,
              allowImages: true,
            })
          })
        }
      }
    }
    const wide = user && content.length > 1
    content.forEach((row, index) => {
      const next = content[index + 1]
      row.wide = wide
      row.space = !next ? 'end' : next.kind === 'section' && next.continued ? 'joined' : 'block'
      if (user && row.kind === 'section')
        row.bubble = content.length === 1 ? 'only' : index === 0 ? 'first' : !next ? 'last' : 'middle'
    })
    rows.push(...content)
    rows.push({ ...base, wide, key: `meta:${id}`, kind: 'meta', at: group[group.length - 1].at, space: 'end' })
  }
  for (const key of cache.keys()) if (!used.has(key)) cache.delete(key)
  return rows
}

// 0.8125rem 字号乘 1.58 行高。
const LINE_HEIGHT = 20.54
const ACTIVITY_HEIGHT = 32
const META_HEIGHT = 40
// 块间距 0.75rem 与气泡内边距 0.75rem。
const BLOCK_SPACE = 12
// 阅读宽度内大约的列数；估算只用来撑出初始滚动条，挂载后以实测行高为准。
const ESTIMATE_COLUMNS = 100

/** 按折行数估算正文高度；空行只是段间距，连续空行与结尾空行不再各算一行。 */
function estimateLines(source: string): number {
  let lines = 0
  let blank = false
  for (const line of source.split('\n')) {
    if (line.trim() === '') {
      blank = true
      continue
    }
    lines += Math.ceil(line.length / ESTIMATE_COLUMNS) + (blank && lines > 0 ? 0.5 : 0)
    blank = false
  }
  return Math.max(lines, 1)
}

export function estimateConversationRow(row: ConversationRow): number {
  const space = row.space === 'block' ? BLOCK_SPACE : 0
  switch (row.kind) {
    case 'section': {
      const bubble =
        row.bubble === 'only' ? 2 * BLOCK_SPACE : row.bubble === 'first' || row.bubble === 'last' ? BLOCK_SPACE : 0
      return estimateLines(row.section.source) * LINE_HEIGHT + space + bubble
    }
    case 'live':
      return estimateLines(row.block.text) * LINE_HEIGHT + space
    case 'activity':
      return ACTIVITY_HEIGHT + space
    case 'meta':
      return META_HEIGHT
  }
}

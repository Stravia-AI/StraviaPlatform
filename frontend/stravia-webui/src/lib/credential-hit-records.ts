import * as m from '#lib/paraglide/messages.js'
import type {
  CredentialDiscoveryRecord,
  CredentialHit,
  CredentialHitMatch,
  CredentialHitReuse,
  CredentialRuleSet,
} from '#lib/types.js'

/** 从命中记录进入请求记录时携带的 URL 参数；只有这个入口会触发命中高亮。 */
export const HIT_HIGHLIGHT_QUERY = 'highlight'
export const HIT_HIGHLIGHT_VALUE = 'credential-hits'

/** 列表停在顶部且页面可见时，两次自动刷新之间的间隔。 */
export const HIT_RECORDS_REFRESH_MS = 30_000

export const HIT_RECORDS_PAGE_SIZE = 30

/** 行内只并列展示前几个命中，其余收敛成计数。 */
export const HIT_PREVIEW_LIMIT = 3

export interface HitRecordsState {
  items: CredentialDiscoveryRecord[]
  nextCursor: string | null
}

/**
 * 把重新取得的第一页并入已加载列表。
 *
 * 映射行不会变化，服务端也按创建顺序倒序返回，因此第一页永远位于已加载行之前，
 * 合并时不需要重新排序，只需去重并沿用已加载列表的游标。
 * 第一页本身就是全部记录，或与已加载的行毫无交集（两次刷新之间新增了整整一页以上）时，
 * 已加载的行与第一页之间可能缺了没人取过的行，游标也无从补上，此时以第一页和它的游标为准重新开始。
 */
export function refreshHitRecords(
  loaded: HitRecordsState,
  head: { items: CredentialDiscoveryRecord[]; next_cursor: string | null },
): HitRecordsState {
  if (head.next_cursor === null) return { items: head.items, nextCursor: null }
  if (!loaded.items.length) return { items: head.items, nextCursor: head.next_cursor }
  const known = new Set(loaded.items.map((record) => record.id))
  if (!head.items.some((record) => known.has(record.id))) {
    return { items: head.items, nextCursor: head.next_cursor }
  }
  const refreshed = new Set(head.items.map((record) => record.id))
  return {
    items: [...head.items, ...loaded.items.filter((record) => !refreshed.has(record.id))],
    nextCursor: loaded.nextCursor,
  }
}

/** 追加下一页；翻页期间已被自动刷新移到头部的行不再重复追加。 */
export function appendHitRecords(
  loaded: readonly CredentialDiscoveryRecord[],
  page: readonly CredentialDiscoveryRecord[],
): CredentialDiscoveryRecord[] {
  const known = new Set(loaded.map((record) => record.id))
  return [...loaded, ...page.filter((record) => !known.has(record.id))]
}

/** 距离下一次自动刷新还需等待的毫秒数；已超过周期时为 0，即回到顶部或页面重新可见时立刻刷新。 */
export function hitRecordsRefreshDelay(lastFetchAt: number, now: number): number {
  return Math.max(0, lastFetchAt + HIT_RECORDS_REFRESH_MS - now)
}

/** 命中来源的展示分类：客户端重放的历史用户消息与本次输入分开。 */
export function hitSourceKind(hit: { source_type: string; latest_input?: boolean | null }): string {
  return hit.source_type === 'user_message' && hit.latest_input === false ? 'history_user_message' : hit.source_type
}

/** 同一规则与展示来源只显示一次：旧事件没有定位，会与新事件的「本次输入」落在同一个分类里。 */
export function uniqueHits(hits: readonly CredentialHit[]): CredentialHit[] {
  const seen = new Set<string>()
  return hits.filter((hit) => {
    const key = `${hit.rule_id}:${hitSourceKind(hit)}`
    if (seen.has(key)) return false
    seen.add(key)
    return true
  })
}

export interface HitPreview {
  hits: CredentialHit[]
  /** 未展示的配对数，含采集时就已省略的定位。 */
  hidden: number
}

/** 行内只保留前若干个去重后的配对，并把省略数量一起呈现，避免长记录把行撑得过高。 */
export function previewHits(hits: readonly CredentialHit[], omitted: number, limit = HIT_PREVIEW_LIMIT): HitPreview {
  const unique = uniqueHits(hits)
  return { hits: unique.slice(0, limit), hidden: Math.max(0, unique.length - limit) + Math.max(0, omitted) }
}

/** 规则集只可能有内置与自定义两种，展示时固定顺序并去重。 */
export function displayRuleSets(ruleSets: readonly CredentialRuleSet[]): CredentialRuleSet[] {
  return [...new Set(ruleSets)].sort((left, right) => (left === right ? 0 : left === 'builtin' ? -1 : 1))
}

export function ruleSetLabel(ruleSet: CredentialRuleSet): string {
  return ruleSet === 'builtin' ? m.credential_protection_rule_set_builtin() : m.credential_protection_rule_set_custom()
}

/** 命中来源文案；历史用户消息与本次输入共用同一套来源类型。 */
export function credentialSourceLabel(sourceKind: string): string {
  switch (sourceKind) {
    case 'user_message':
      return m.credential_protection_source_user_message()
    case 'history_user_message':
      return m.credential_protection_source_history_user_message()
    case 'system_or_history':
      return m.credential_protection_source_system_or_history()
    case 'tool_arguments':
      return m.credential_protection_source_tool_arguments()
    case 'tool_result':
      return m.credential_protection_source_tool_result()
    case 'other_text':
      return m.credential_protection_source_other_text()
    default:
      return m.credential_protection_source_unknown()
  }
}

/**
 * 命中列表已经列出了全部创建规则时，「创建规则」不再提供额外信息。
 * 旧记录没有命中定位、或定位超出上限被省略时，规则只出现在创建规则里，必须保留。
 */
export function hitsCoverRules(ruleIds: readonly string[], hits: readonly CredentialHit[]): boolean {
  return ruleIds.every((ruleId) => hits.some((hit) => hit.rule_id === ruleId))
}

export interface CredentialEntrySegment {
  text: string
  /** `hit` 是映射对应的命中，`match` 是同一检测器找到的其他匹配。 */
  kind: 'plain' | 'hit' | 'match'
  /** 指向 detail.matches 的下标，遮罩与定位都靠它。 */
  matchIndex: number | null
  revealed: boolean
}

/**
 * 按 UTF-16 偏移把条目正文切成可渲染的片段，偏移与「密钥匹配测试」一致（区间右端不含）。
 * 越界与零长度匹配会被丢弃；相互重叠时保留前面的匹配，后面的按剩余区间截短。
 */
export function entrySegments(
  text: string,
  matches: readonly CredentialHitMatch[],
  revealed: ReadonlySet<number> = new Set(),
): CredentialEntrySegment[] {
  const ordered = matches
    .map((match, index) => ({ match, index }))
    .filter(({ match }) => match.end > match.start && match.start < text.length && match.end > 0)
    .sort((left, right) => left.match.start - right.match.start || left.index - right.index)

  const segments: CredentialEntrySegment[] = []
  let cursor = 0
  for (const { match, index } of ordered) {
    const start = Math.max(cursor, match.start)
    const end = Math.min(text.length, match.end)
    if (end <= cursor) continue
    if (start > cursor)
      segments.push({ text: text.slice(cursor, start), kind: 'plain', matchIndex: null, revealed: false })
    segments.push({
      text: text.slice(start, end),
      kind: match.hit ? 'hit' : 'match',
      matchIndex: index,
      revealed: revealed.has(index),
    })
    cursor = end
  }
  if (cursor < text.length)
    segments.push({ text: text.slice(cursor), kind: 'plain', matchIndex: null, revealed: false })
  return segments
}

export const HIT_MASK = '••••••'

/**
 * 复用位置的展示去重：同一位置在多次扫描中可能重复出现，只保留最近一次；
 * 按最近复用倒序，便于把最近发生的位置放在前面。
 */
export function uniqueReuses(items: readonly CredentialHitReuse[]): CredentialHitReuse[] {
  const latest = new Map<string, CredentialHitReuse>()
  for (const item of items) {
    const key = JSON.stringify([item.interaction_id, item.source_type, item.tool_id ?? item.latest_input ?? null])
    const previous = latest.get(key)
    if (!previous || item.last_reused_at > previous.last_reused_at) latest.set(key, item)
  }
  return [...latest.values()].sort((left, right) => right.last_reused_at - left.last_reused_at)
}

/** 命中记录进入请求记录的路径；组件再交给 SvelteKit 的 `resolve` 处理部署前缀。 */
export function requestLogPath(interactionId: string): string {
  return `/logs?interaction=${encodeURIComponent(interactionId)}&${HIT_HIGHLIGHT_QUERY}=${HIT_HIGHLIGHT_VALUE}`
}

/** 侧栏请求失败时区分「记录已不存在」与「暂时取不到」，两者给出不同的恢复动作。 */
export function hitQueryFailure(error: unknown): 'gone' | 'failed' {
  if (typeof error === 'object' && error !== null && 'status' in error && error.status === 404) return 'gone'
  return 'failed'
}

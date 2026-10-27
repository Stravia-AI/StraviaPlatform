import type { CredentialDiscoverySummary, CredentialHit } from '#lib/types.js'

/** 从命中记录进入请求记录时携带的 URL 参数；只有这个入口会触发命中高亮。 */
export const HIT_HIGHLIGHT_QUERY = 'highlight'
export const HIT_HIGHLIGHT_VALUE = 'credential-hits'

/** 列表停在顶部且页面可见时，两次自动刷新之间的间隔。 */
export const HIT_RECORDS_REFRESH_MS = 30_000

export const HIT_RECORDS_PAGE_SIZE = 30

/** 与服务端分页顺序一致：最近命中在前，同一时刻按 Interaction ID 倒序。 */
function compareHitRecords(a: CredentialDiscoverySummary, b: CredentialDiscoverySummary): number {
  if (a.discovered_at !== b.discovered_at) return b.discovered_at - a.discovered_at
  if (a.interaction_id === b.interaction_id) return 0
  return a.interaction_id < b.interaction_id ? 1 : -1
}

export interface HitRecordsState {
  items: CredentialDiscoverySummary[]
  nextCursor: string | null
}

/**
 * 把重新取得的第一页并入已加载列表。
 *
 * 第一页与已加载的行有交集时，两段是连续的：按 Interaction ID 就地替换，更靠后的行与游标保持不变。
 * 第一页本身就是全部记录，或与已加载的行毫无交集（两次刷新之间新增了整整一页以上的命中）时，
 * 已加载的行与第一页之间可能缺了没人取过的行，游标也无从补上，此时以第一页和它的游标为准重新开始。
 */
export function refreshHitRecords(
  loaded: HitRecordsState,
  head: { items: CredentialDiscoverySummary[]; next_cursor: string | null },
): HitRecordsState {
  const known = new Set(loaded.items.map((record) => record.interaction_id))
  const overlaps = head.items.some((record) => known.has(record.interaction_id))
  if (head.next_cursor === null || !overlaps) return { items: head.items, nextCursor: head.next_cursor }
  const refreshed = new Set(head.items.map((record) => record.interaction_id))
  return {
    items: [...head.items, ...loaded.items.filter((record) => !refreshed.has(record.interaction_id))].sort(
      compareHitRecords,
    ),
    nextCursor: loaded.nextCursor,
  }
}

/** 追加下一页；翻页期间已被自动刷新移到头部的行不再重复追加。 */
export function appendHitRecords(
  loaded: readonly CredentialDiscoverySummary[],
  page: readonly CredentialDiscoverySummary[],
): CredentialDiscoverySummary[] {
  const known = new Set(loaded.map((record) => record.interaction_id))
  return [...loaded, ...page.filter((record) => !known.has(record.interaction_id))]
}

/** 距离下一次自动刷新还需等待的毫秒数；已超过周期时为 0，即回到顶部或页面重新可见时立刻刷新。 */
export function hitRecordsRefreshDelay(lastFetchAt: number, now: number): number {
  return Math.max(0, lastFetchAt + HIT_RECORDS_REFRESH_MS - now)
}

/** 命中来源的展示分类：客户端重放的历史用户消息与本次输入分开。 */
export function hitSourceKind(hit: CredentialHit): string {
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

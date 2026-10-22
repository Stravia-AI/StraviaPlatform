/** 资源明细（供应商、API 密钥）默认只展示排序后的前 8 项；排序始终作用于完整集合。 */
export const STATS_TOP_N = 8

export type StatsSortValue = string | number | null | undefined

export interface StatsSortKey {
  id: string
  desc: boolean
}

const textCollator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' })

function compareValues(left: string | number, right: string | number): number {
  if (typeof left === 'number' && typeof right === 'number') return left - right
  return textCollator.compare(String(left), String(right))
}

/**
 * 对完整集合稳定排序。未上报（null/undefined）的值无论升降序都排在最后；
 * 没有排序键或列未知时保持后端返回的顺序。
 */
export function sortStatsRows<T>(
  rows: readonly T[],
  sorting: readonly StatsSortKey[],
  values: Readonly<Record<string, (row: T) => StatsSortValue>>,
): T[] {
  const key = sorting[0]
  const read = key ? values[key.id] : undefined
  if (!key || !read) return [...rows]
  const direction = key.desc ? -1 : 1
  return rows
    .map((row, index) => ({ row, index, value: read(row) }))
    .sort((a, b) => {
      if (a.value == null || b.value == null) {
        if (a.value == null && b.value == null) return a.index - b.index
        return a.value == null ? 1 : -1
      }
      return compareValues(a.value, b.value) * direction || a.index - b.index
    })
    .map(({ row }) => row)
}

export function visibleStatsRows<T>(rows: readonly T[], showAll: boolean): T[] {
  return showAll ? [...rows] : rows.slice(0, STATS_TOP_N)
}

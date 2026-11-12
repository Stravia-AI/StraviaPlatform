export interface VirtualRow<T> {
  key: string
  index: number
  item: T
}

export interface VirtualSegment<T> {
  /** 分组段在滚动经过时保持稳定，行进出窗口不会重建包裹层。 */
  key: string
  group: string | undefined
  start: number
  rows: VirtualRow<T>[]
}

interface VirtualEntry {
  index: number
  start: number
  key: string | number | bigint
}

/**
 * 把已挂载的行切成定位段：下标连续且同组的行共用一个包裹层（如同一条消息的 `<article>`），
 * 其余行各自成段。组内被保留的远处行与主体不相邻，不能共用流式布局，只能另开一段。
 */
export function virtualSegments<T>(
  entries: readonly VirtualEntry[],
  items: readonly T[],
  itemKey: (item: T) => string,
  group?: (item: T) => string | undefined,
): VirtualSegment<T>[] {
  const segments: VirtualSegment<T>[] = []
  for (const entry of entries) {
    const item = items[entry.index]
    // 条目已更新而虚拟化器尚未收到新的行数时，下标可能越界或指向别的行。
    if (item === undefined || itemKey(item) !== String(entry.key)) continue
    const row: VirtualRow<T> = { key: itemKey(item), index: entry.index, item }
    const name = group?.(item)
    const last = segments.at(-1)
    if (name !== undefined && last?.group === name && last.rows.at(-1)!.index === entry.index - 1) {
      last.rows.push(row)
    } else {
      segments.push({ key: '', group: name, start: entry.start, rows: [row] })
    }
  }
  const seen = new Map<string, number>()
  for (const segment of segments) {
    const base = segment.group === undefined ? `row:${segment.rows[0].key}` : `group:${segment.group}`
    const ordinal = seen.get(base) ?? 0
    seen.set(base, ordinal + 1)
    segment.key = ordinal === 0 ? base : `${base}#${ordinal}`
  }
  return segments
}

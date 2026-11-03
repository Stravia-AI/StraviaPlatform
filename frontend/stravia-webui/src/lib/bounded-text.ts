const segmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' })

/** 取尾部至多 `limit` 个 UTF-16 码元，起点对齐到字素边界，不切开代理对或组合字符。 */
export function boundedTail(text: string, limit: number): string {
  let start = text.length
  for (let count = 0; count < limit && start > 0; count += 1) {
    start -= 1
    const unit = text.charCodeAt(start)
    if (unit >= 0xdc00 && unit <= 0xdfff && start > 0) start -= 1
  }
  if (start === 0) return text
  const boundary = segmenter.segment(text).containing(start)
  if (boundary && boundary.index < start) start = boundary.index + boundary.segment.length
  return text.slice(start)
}

/** 取头部至多 `limit` 个 UTF-16 码元，不切开代理对。 */
export function boundedHead(text: string, limit: number): string {
  if (text.length <= limit) return text
  const end = text.charCodeAt(limit - 1) >= 0xd800 && text.charCodeAt(limit - 1) <= 0xdbff ? limit - 1 : limit
  return text.slice(0, end)
}

import type {
  ObservationContent,
  ObservationContentRef,
  ObservationContentsPage,
} from './types/observation'

/** 服务端单次请求最多接受的 id 数。 */
export const CONTENTS_REQUEST_LIMIT = 256
/** 内存缓存按原始字节数计的上限；超过后淘汰最久未使用的正文。 */
const CACHE_BUDGET_BYTES = 8 * 1024 * 1024
/** 预览逐窗口读取引用；窗口内的 id 合并为一次请求。 */
const PREVIEW_WINDOW = 8

export type ContentsFetcher = (ids: number[]) => Promise<ObservationContentsPage>

/** 文本单元直接取字符串，JSON 单元取紧凑序列化，与服务端拼接语义一致。 */
export function contentText(content: ObservationContent): string {
  return content.format === 'text' && typeof content.body === 'string' ? content.body : JSON.stringify(content.body)
}

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

interface Pending {
  resolve: (content: ObservationContent | null) => void
  reject: (error: unknown) => void
}

/**
 * 按 content_id 缓存并批量读取 Observation 正文。
 * content_id 不复用，同一 id 在一次会话内只请求一次；并发请求在同一微任务内合并，
 * 每次请求不超过服务端上限，id 升序以复用浏览器按 URL 的 ETag/304 缓存。
 */
export class ObservationContents {
  readonly #fetch: ContentsFetcher
  readonly #budget: number
  /** `null` 表示服务端按不存在处理（已过期或不可见），同样只请求一次。 */
  readonly #cache = new Map<number, ObservationContent | null>()
  readonly #inflight = new Map<number, Promise<ObservationContent | null>>()
  readonly #pending = new Map<number, Pending>()
  #size = 0
  #scheduled = false

  constructor(fetcher: ContentsFetcher, budget = CACHE_BUDGET_BYTES) {
    this.#fetch = fetcher
    this.#budget = budget
  }

  /** 读取一组正文；返回的 Map 对服务端不返回的 id 取 `null`。 */
  async load(ids: readonly number[]): Promise<Map<number, ObservationContent | null>> {
    const entries = await Promise.all([...new Set(ids)].map(async (id) => [id, await this.#one(id)] as const))
    return new Map(entries)
  }

  /** 按引用顺序拼接全文；服务端不返回的单元跳过。 */
  async read(refs: readonly ObservationContentRef[], separator: string): Promise<string> {
    const loaded = await this.load(refs.map((ref) => ref.content_id))
    return refs.flatMap((ref) => {
      const content = loaded.get(ref.content_id)
      return content ? [contentText(content)] : []
    }).join(separator)
  }

  /**
   * 卡片预览：只读取靠近 `edge` 的引用，直到取得足够的非空文本，再裁剪到 `limit`。
   * 不读取其余引用，列表因此不随全文长度膨胀。
   */
  async preview(
    refs: readonly ObservationContentRef[],
    options: { separator: string; edge: 'head' | 'tail'; limit: number },
  ): Promise<string> {
    const ordered = options.edge === 'head' ? refs : refs.toReversed()
    const parts: string[] = []
    let length = 0
    for (let offset = 0; offset < ordered.length && length < options.limit; offset += PREVIEW_WINDOW) {
      const window = ordered.slice(offset, offset + PREVIEW_WINDOW)
      const loaded = await this.load(window.map((ref) => ref.content_id))
      for (const ref of window) {
        const content = loaded.get(ref.content_id)
        if (!content) continue
        const text = contentText(content)
        parts.push(text)
        length += text.trim() ? text.length : 0
        if (length >= options.limit) break
      }
    }
    if (options.edge === 'tail') parts.reverse()
    const text = parts.join(options.separator)
    return options.edge === 'head' ? boundedHead(text, options.limit) : boundedTail(text, options.limit)
  }

  #one(id: number): Promise<ObservationContent | null> {
    if (this.#cache.has(id)) {
      const hit = this.#cache.get(id)!
      this.#cache.delete(id)
      this.#cache.set(id, hit)
      return Promise.resolve(hit)
    }
    const inflight = this.#inflight.get(id)
    if (inflight) return inflight
    const { promise: request, resolve, reject } = Promise.withResolvers<ObservationContent | null>()
    this.#pending.set(id, { resolve, reject })
    this.#inflight.set(id, request)
    if (!this.#scheduled) {
      this.#scheduled = true
      queueMicrotask(() => this.#flush())
    }
    return request
  }

  #flush() {
    this.#scheduled = false
    const ids = [...this.#pending.keys()].sort((a, b) => a - b)
    const waiting = new Map(this.#pending)
    this.#pending.clear()
    for (let index = 0; index < ids.length; index += CONTENTS_REQUEST_LIMIT) {
      void this.#request(ids.slice(index, index + CONTENTS_REQUEST_LIMIT), waiting)
    }
  }

  async #request(ids: number[], waiting: Map<number, Pending>) {
    try {
      const page = await this.#fetch(ids)
      const found = new Map(page.contents.map((content) => [content.content_id, content]))
      for (const id of ids) {
        const content = found.get(id) ?? null
        this.#remember(id, content)
        this.#inflight.delete(id)
        waiting.get(id)!.resolve(content)
      }
    } catch (error) {
      for (const id of ids) {
        this.#inflight.delete(id)
        waiting.get(id)!.reject(error)
      }
    }
  }

  #remember(id: number, content: ObservationContent | null) {
    const size = content?.raw_len ?? 0
    if (size > this.#budget) return
    this.#cache.set(id, content)
    this.#size += size
    for (const [oldest, entry] of this.#cache) {
      if (this.#size <= this.#budget) break
      if (oldest === id) continue
      this.#cache.delete(oldest)
      this.#size -= entry?.raw_len ?? 0
    }
  }
}

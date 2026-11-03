import { boundedHead, boundedTail } from './bounded-text'
import type { ObservationContent, ObservationContentRef, ObservationContentsResponse } from './types/observation'

/** 服务端单次请求最多接受的 id 数。 */
export const CONTENTS_REQUEST_LIMIT = 256
/** 内存缓存按原始字节数计的上限；超过后淘汰最久未使用的请求快照。 */
const CACHE_BUDGET_BYTES = 8 * 1024 * 1024
/** 每份快照的固定开销（键、ETag、Map），避免只含「不存在」的快照不计入预算。 */
const SNAPSHOT_OVERHEAD_BYTES = 256
/** 预览逐窗口读取引用；窗口内的 id 合并为一次请求。 */
const PREVIEW_WINDOW = 8

/** 带验证器发起一次内容请求；`etag` 为 `null` 表示没有可验证的快照。 */
export type ContentsFetcher = (ids: number[], etag: string | null) => Promise<ObservationContentsResponse>

/** 引用的 content_id 序列；引用序列相同即卡片展示的正文相同。 */
export function contentRefsKey(summary: { refs: readonly ObservationContentRef[] }): string {
  return summary.refs.map((ref) => ref.content_id).join(',')
}

/** 文本单元直接取字符串，JSON 单元取紧凑序列化，与服务端拼接语义一致。 */
export function contentText(content: ObservationContent): string {
  return content.format === 'text' && typeof content.body === 'string' ? content.body : JSON.stringify(content.body)
}

interface Pending {
  resolve: (content: ObservationContent | null) => void
  reject: (error: unknown) => void
}

/** 某个请求键（升序 id 块）最近一次成功响应：服务端 ETag 与当时返回的正文。 */
interface Snapshot {
  etag: string
  bodies: Map<number, ObservationContent>
  bytes: number
}

/**
 * 批量读取 Observation 正文。
 * 正文按当前凭据映射遮蔽，同一 id 在映射变化后内容会不同，所以缓存不以 id 为键：
 * 每次请求按「升序 id 块」缓存 ETag 与正文，并总是带 `If-None-Match` 重新验证，
 * 304 复用快照正文，200 以新响应整体替换；服务端不返回的 id 不做记忆，下次读取重新询问。
 * 同一 id 在请求进行中的重复读取共享该请求；并发读取在同一微任务内合并，
 * 每次请求不超过服务端上限。
 */
export class ObservationContents {
  readonly #fetch: ContentsFetcher
  readonly #budget: number
  /** 键为升序 id 用逗号连接；Map 的插入顺序即最近使用顺序。 */
  readonly #snapshots = new Map<string, Snapshot>()
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
    return refs
      .flatMap((ref) => {
        const content = loaded.get(ref.content_id)
        return content ? [contentText(content)] : []
      })
      .join(separator)
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
    const key = ids.join(',')
    const known = this.#snapshots.get(key)
    try {
      const response = await this.#fetch(ids, known?.etag ?? null)
      let bodies: Map<number, ObservationContent>
      if (response.status === 'not-modified') {
        if (!known) throw new Error('Observation contents answered 304 without a validator')
        bodies = known.bodies
        this.#touch(key, known)
      } else {
        bodies = new Map(response.contents.map((content) => [content.content_id, content]))
        this.#replace(key, response.etag, bodies)
      }
      for (const id of ids) {
        this.#inflight.delete(id)
        waiting.get(id)!.resolve(bodies.get(id) ?? null)
      }
    } catch (error) {
      for (const id of ids) {
        this.#inflight.delete(id)
        waiting.get(id)!.reject(error)
      }
    }
  }

  #touch(key: string, snapshot: Snapshot) {
    if (this.#snapshots.get(key) !== snapshot) return
    this.#snapshots.delete(key)
    this.#snapshots.set(key, snapshot)
  }

  /** 以新响应替换该键的快照；没有 ETag 的响应无法验证，直接丢弃旧快照。 */
  #replace(key: string, etag: string | null, bodies: Map<number, ObservationContent>) {
    this.#drop(key)
    if (etag === null) return
    let bytes = SNAPSHOT_OVERHEAD_BYTES
    for (const content of bodies.values()) bytes += content.raw_len
    if (bytes > this.#budget) return
    this.#snapshots.set(key, { etag, bodies, bytes })
    this.#size += bytes
    for (const [oldest, snapshot] of this.#snapshots) {
      if (this.#size <= this.#budget) break
      if (oldest === key) continue
      this.#snapshots.delete(oldest)
      this.#size -= snapshot.bytes
    }
  }

  #drop(key: string) {
    const previous = this.#snapshots.get(key)
    if (!previous) return
    this.#snapshots.delete(key)
    this.#size -= previous.bytes
  }
}

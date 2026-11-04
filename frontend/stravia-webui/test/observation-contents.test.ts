import { describe, expect, test } from 'bun:test'
import { CONTENTS_REQUEST_LIMIT, ObservationContents, contentRefsKey } from '../src/lib/observation-contents'
import type { ObservationContent, ObservationContentsResponse } from '../src/lib/types/observation'

function text(id: number, body: string): ObservationContent {
  return { content_id: id, format: 'text', raw_len: body.length, body }
}

/**
 * 模拟带 ETag 的内容端点：`version` 代表凭据映射版本，`bodies` 缺失的 id 不返回。
 * ETag 取自「版本 + 返回的 id/正文」，与服务端「映射版本 + 返回正文摘要」同构；
 * `If-None-Match` 与当前 ETag 相同才应答 304。
 */
function server(bodies: Map<number, ObservationContent>) {
  const state = { version: 1, bodies }
  const calls: Array<{ ids: number[]; etag: string | null }> = []
  const fetcher = (ids: number[], etag: string | null): Promise<ObservationContentsResponse> => {
    calls.push({ ids, etag })
    const contents = ids.flatMap((id) => (state.bodies.has(id) ? [state.bodies.get(id)!] : []))
    const current = `"v${state.version}:${JSON.stringify(contents)}"`
    if (etag === current) return Promise.resolve({ status: 'not-modified' })
    return Promise.resolve({ status: 'modified', etag: current, contents })
  }
  return { state, calls, fetcher }
}

const ref = (content_id: number) => ({ content_id, raw_len: 1 })

describe('observation contents', () => {
  test('batches concurrent reads into one sorted request', async () => {
    const { calls, fetcher } = server(new Map([1, 2, 3].map((id) => [id, text(id, `t${id}`)])))
    const contents = new ObservationContents(fetcher)
    const [a, b] = await Promise.all([contents.read([ref(3), ref(1)], '\n'), contents.read([ref(2), ref(3)], '\n')])
    expect(a).toBe('t3\nt1')
    expect(b).toBe('t2\nt3')
    expect(calls).toEqual([{ ids: [1, 2, 3], etag: null }])
  })

  test('shares an in-flight request instead of asking again', async () => {
    const { calls, fetcher } = server(new Map([[1, text(1, 'a')]]))
    const contents = new ObservationContents(fetcher)
    const first = contents.read([ref(1)], '')
    await Promise.resolve()
    const second = contents.read([ref(1)], '')
    expect(await Promise.all([first, second])).toEqual(['a', 'a'])
    expect(calls).toHaveLength(1)
  })

  test('revalidates with the stored ETag and reuses the cached bodies on 304', async () => {
    const { calls, fetcher } = server(new Map([1, 2].map((id) => [id, text(id, `t${id}`)])))
    const contents = new ObservationContents(fetcher)
    expect(await contents.read([ref(1), ref(2)], '|')).toBe('t1|t2')
    expect(await contents.read([ref(2), ref(1)], '|')).toBe('t2|t1')
    expect(calls).toHaveLength(2)
    expect(calls[0].etag).toBeNull()
    expect(calls[1].etag).toBe(`"v1:${JSON.stringify([text(1, 't1'), text(2, 't2')])}"`)
  })

  test('shows the re-masked body after the credential mapping changes', async () => {
    const { state, calls, fetcher } = server(new Map([[1, text(1, 'token sk-secret')]]))
    const contents = new ObservationContents(fetcher)
    expect(await contents.read([ref(1)], '')).toBe('token sk-secret')
    state.version = 2
    state.bodies = new Map([[1, text(1, 'token ***')]])
    expect(await contents.read([ref(1)], '')).toBe('token ***')
    expect(await contents.read([ref(1)], '')).toBe('token ***')
    expect(calls.map((call) => call.etag === null)).toEqual([true, false, false])
  })

  test('does not remember a missing id: it is asked again and shows once visible', async () => {
    const { state, calls, fetcher } = server(new Map([[1, text(1, 'a')]]))
    const contents = new ObservationContents(fetcher)
    expect(await contents.read([ref(1), ref(9)], '\n')).toBe('a')
    expect((await contents.load([9])).get(9)).toBeNull()
    state.bodies.set(9, text(9, 'late'))
    expect(await contents.read([ref(1), ref(9)], '\n')).toBe('a\nlate')
    expect(await contents.read([ref(9)], '')).toBe('late')
    expect(calls.map((call) => call.ids)).toEqual([[1, 9], [9], [1, 9], [9]])
  })

  test('keeps separate validators per request key, never across batch compositions', async () => {
    const { calls, fetcher } = server(new Map([1, 2, 3].map((id) => [id, text(id, `t${id}`)])))
    const contents = new ObservationContents(fetcher)
    await contents.load([1, 2, 3])
    await contents.load([1])
    expect(calls.map((call) => call.etag === null)).toEqual([true, true])
  })

  test('joins json units compactly and skips missing ids', async () => {
    const { fetcher } = server(
      new Map<number, ObservationContent>([
        [1, text(1, 'a')],
        [2, { content_id: 2, format: 'json', raw_len: 7, body: { k: 1 } }],
      ]),
    )
    expect(await new ObservationContents(fetcher).read([ref(1), ref(9), ref(2)], '\n\n')).toBe('a\n\n{"k":1}')
  })

  test('splits requests above the server id limit', async () => {
    const ids = Array.from({ length: CONTENTS_REQUEST_LIMIT + 5 }, (_, index) => index + 1)
    const { calls, fetcher } = server(new Map(ids.map((id) => [id, text(id, 'x')])))
    await new ObservationContents(fetcher).load(ids)
    expect(calls.map((call) => call.ids.length)).toEqual([CONTENTS_REQUEST_LIMIT, 5])
  })

  test('evicts the least recently used snapshot beyond the byte budget', async () => {
    // 每份正文 1000 字节、预算 3000：两份快照放得下，三份放不下（固定开销远小于 500 的余量）。
    const big = (id: number): ObservationContent => ({ ...text(id, 'abcd'), raw_len: 1000 })
    const { calls, fetcher } = server(new Map([1, 2, 3].map((id) => [id, big(id)])))
    const contents = new ObservationContents(fetcher, 3000)
    await contents.load([1])
    await contents.load([2])
    await contents.load([3]) // 淘汰最久未用的 1
    await contents.load([2]) // 仍在缓存：带验证器
    await contents.load([1]) // 已被淘汰：无验证器
    expect(calls.map((call) => call.etag === null)).toEqual([true, true, true, false, true])
  })

  test('a failed request rejects its readers and is retried by the next read', async () => {
    let fail = true
    const contents = new ObservationContents((ids) =>
      fail
        ? Promise.reject(new Error('offline'))
        : Promise.resolve({ status: 'modified', etag: '"e"', contents: ids.map((id) => text(id, 'ok')) }),
    )
    const error = await contents.read([ref(1)], '').then(
      () => null,
      (reason: unknown) => reason,
    )
    expect(error).toEqual(new Error('offline'))
    fail = false
    expect(await contents.read([ref(1)], '')).toBe('ok')
  })

  test('rejects a 304 that answers a request without a validator', async () => {
    const contents = new ObservationContents(() => Promise.resolve({ status: 'not-modified' }))
    const error = await contents.read([ref(1)], '').then(
      () => null,
      (reason: unknown) => reason,
    )
    expect(String(error)).toContain('304')
  })

  test('a response without an ETag is not cached for validation', async () => {
    const calls: Array<string | null> = []
    const contents = new ObservationContents((ids, etag) => {
      calls.push(etag)
      return Promise.resolve({ status: 'modified', etag: null, contents: ids.map((id) => text(id, 'x')) })
    })
    await contents.read([ref(1)], '')
    await contents.read([ref(1)], '')
    expect(calls).toEqual([null, null])
  })

  test('preview reads only the references next to the requested edge', async () => {
    const { calls, fetcher } = server(
      new Map(Array.from({ length: 40 }, (_, index) => [index + 1, text(index + 1, `chunk-${index + 1}`)])),
    )
    const contents = new ObservationContents(fetcher)
    const refs = Array.from({ length: 40 }, (_, index) => ref(index + 1))
    expect(await contents.preview(refs, { separator: '\n', edge: 'head', limit: 12 })).toBe('chunk-1\nchun')
    expect(await contents.preview(refs, { separator: '\n', edge: 'tail', limit: 12 })).toBe(
      'chunk-39\nchunk-40'.slice(-12),
    )
    // 每个方向只发一次请求，只含靠近该边缘的引用，不读取中间部分。
    expect(calls).toHaveLength(2)
    const [head, tail] = calls.map((call) => call.ids)
    expect(head.length).toBeLessThan(20)
    expect(Math.max(...head)).toBeLessThan(20)
    expect(tail.length).toBeLessThan(20)
    expect(Math.min(...tail)).toBeGreaterThan(20)
    expect(head).toContain(1)
    expect(tail).toContain(40)
  })

  test('preview keeps reading further windows until it has enough text', async () => {
    const { calls, fetcher } = server(
      new Map(Array.from({ length: 40 }, (_, index) => [index + 1, text(index + 1, 'x')])),
    )
    const refs = Array.from({ length: 40 }, (_, index) => ref(index + 1))
    const preview = await new ObservationContents(fetcher).preview(refs, { separator: '', edge: 'head', limit: 30 })
    expect(preview).toBe('x'.repeat(30))
    expect(calls.length).toBeGreaterThan(1)
    // 窗口按顺序向后推进，已读的 id 不重复请求。
    const asked = calls.flatMap((call) => call.ids)
    expect(new Set(asked).size).toBe(asked.length)
  })
})

describe('content refs key', () => {
  test('is the ordered content id sequence', () => {
    expect(contentRefsKey({ refs: [ref(3), ref(1)] })).toBe('3,1')
    expect(contentRefsKey({ refs: [] })).toBe('')
  })
})

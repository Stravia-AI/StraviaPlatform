import { describe, expect, test } from 'bun:test'
import { CONTENTS_REQUEST_LIMIT, ObservationContents, boundedHead, boundedTail } from '../src/lib/observation-contents'
import type { ObservationContent, ObservationContentsPage } from '../src/lib/types/observation'

function text(id: number, body: string): ObservationContent {
  return { content_id: id, format: 'text', raw_len: body.length, body }
}

/** 记录每次请求的 id；`bodies` 缺失的 id 不返回，模拟服务端按不存在处理。 */
function store(bodies: Map<number, ObservationContent>) {
  const calls: number[][] = []
  const fetcher = (ids: number[]): Promise<ObservationContentsPage> => {
    calls.push(ids)
    return Promise.resolve({ contents: ids.flatMap((id) => (bodies.has(id) ? [bodies.get(id)!] : [])) })
  }
  return { calls, fetcher }
}

const ref = (content_id: number) => ({ content_id, raw_len: 1 })

describe('observation contents', () => {
  test('batches concurrent reads into one sorted request and never refetches a cached id', async () => {
    const { calls, fetcher } = store(new Map([1, 2, 3].map((id) => [id, text(id, `t${id}`)])))
    const contents = new ObservationContents(fetcher)
    const [a, b] = await Promise.all([contents.read([ref(3), ref(1)], '\n'), contents.read([ref(2), ref(3)], '\n')])
    expect(a).toBe('t3\nt1')
    expect(b).toBe('t2\nt3')
    expect(calls).toEqual([[1, 2, 3]])
    await contents.read([ref(1), ref(2)], '')
    expect(calls).toHaveLength(1)
  })

  test('splits requests above the server id limit', async () => {
    const ids = Array.from({ length: CONTENTS_REQUEST_LIMIT + 5 }, (_, index) => index + 1)
    const { calls, fetcher } = store(new Map(ids.map((id) => [id, text(id, 'x')])))
    await new ObservationContents(fetcher).load(ids)
    expect(calls.map((call) => call.length)).toEqual([CONTENTS_REQUEST_LIMIT, 5])
    expect(calls.every((call) => call.length <= CONTENTS_REQUEST_LIMIT)).toBe(true)
  })

  test('joins json units compactly, skips missing ids and remembers them as missing', async () => {
    const { calls, fetcher } = store(
      new Map<number, ObservationContent>([
        [1, text(1, 'a')],
        [2, { content_id: 2, format: 'json', raw_len: 7, body: { k: 1 } }],
      ]),
    )
    const contents = new ObservationContents(fetcher)
    expect(await contents.read([ref(1), ref(9), ref(2)], '\n\n')).toBe('a\n\n{"k":1}')
    await contents.read([ref(9)], '')
    expect(calls).toEqual([[1, 2, 9]])
  })

  test('evicts the least recently used body beyond the byte budget', async () => {
    const { calls, fetcher } = store(new Map([1, 2, 3].map((id) => [id, text(id, 'abcd')])))
    const contents = new ObservationContents(fetcher, 8)
    await contents.load([1])
    await contents.load([2])
    await contents.load([3])
    await contents.load([1])
    expect(calls).toEqual([[1], [2], [3], [1]])
    await contents.load([3])
    expect(calls).toHaveLength(4)
  })

  test('a failed request rejects its readers and is retried by the next read', async () => {
    let fail = true
    const contents = new ObservationContents((ids) =>
      fail ? Promise.reject(new Error('offline')) : Promise.resolve({ contents: ids.map((id) => text(id, 'ok')) }),
    )
    const error = await contents.read([ref(1)], '').then(
      () => null,
      (reason: unknown) => reason,
    )
    expect(error).toEqual(new Error('offline'))
    fail = false
    expect(await contents.read([ref(1)], '')).toBe('ok')
  })

  test('preview reads only the references next to the requested edge', async () => {
    const { calls, fetcher } = store(new Map(Array.from({ length: 40 }, (_, index) => [index + 1, text(index + 1, `chunk-${index + 1}`)])))
    const contents = new ObservationContents(fetcher)
    const refs = Array.from({ length: 40 }, (_, index) => ref(index + 1))
    expect(await contents.preview(refs, { separator: '\n', edge: 'head', limit: 12 })).toBe('chunk-1\nchun')
    expect(await contents.preview(refs, { separator: '\n', edge: 'tail', limit: 12 })).toBe('chunk-39\nchunk-40'.slice(-12))
    expect(calls).toEqual([[1, 2, 3, 4, 5, 6, 7, 8], [33, 34, 35, 36, 37, 38, 39, 40]])
  })
})

describe('bounded text', () => {
  test('never splits a surrogate pair or grapheme at the cut', () => {
    expect(boundedHead('ab😀cd', 3)).toBe('ab')
    expect(boundedTail('ab😀cd', 3)).toBe('cd')
    expect(boundedTail('short', 100)).toBe('short')
  })
})

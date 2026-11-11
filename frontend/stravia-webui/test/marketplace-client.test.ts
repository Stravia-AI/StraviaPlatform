import { afterEach, beforeEach, expect, test } from 'bun:test'
import { admin } from '../src/lib/admin-client'

const originalFetch = globalThis.fetch
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window')
const input = { source_id: 'source', release_id: 1, vendor_id: 'vendor', version: '1.0.0', catalog_sha256: 'digest' }

beforeEach(() => {
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: {
      __TAURI_INTERNALS__: {
        invoke(command: string) {
          if (command === 'get_server_port') return Promise.resolve(18473)
          if (command === 'get_admin_session') return Promise.resolve({ access_token: 'test-session' })
          return Promise.reject(new Error(`Unexpected native command: ${command}`))
        },
      },
    },
  })
})
afterEach(() => {
  globalThis.fetch = originalFetch
  if (originalWindow) Object.defineProperty(globalThis, 'window', originalWindow)
  else Reflect.deleteProperty(globalThis, 'window')
})

test('preparation parses split SSE events', async () => {
  globalThis.fetch = (() => {
    const encoder = new TextEncoder()
    const body = new ReadableStream({
      start(controller) {
        for (const chunk of [
          'event: progress\r\ndata: {"stage":"download",',
          '"received":3,"total":4}\r\n\r\nevent: preview\ndata: {"id":"preview"}\n\n',
        ]) {
          controller.enqueue(encoder.encode(chunk))
        }
        controller.close()
      },
    })
    return Promise.resolve(new Response(body, { headers: { 'content-type': 'text/event-stream' } }))
  }) as unknown as typeof fetch
  const progress: unknown[] = []
  const preview = await admin.pluginMarketplace.prepareInstall(input, (value) => progress.push(value))
  expect(preview.id).toBe('preview')
  expect(progress).toEqual([{ stage: 'download', received: 3, total: 4 }])
})

test('marketplace errors retain structured codes', async () => {
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ error: { code: 'key_changed', message: 'Key changed' } }), { status: 409 }),
    )) as unknown as typeof fetch
  try {
    await admin.pluginMarketplace.get()
    throw new Error('expected failure')
  } catch (error) {
    expect((error as Error & { code: string }).code).toBe('key_changed')
    expect((error as Error).message).toBe('Key changed')
  }
})

test('stream errors preserve their code', async () => {
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response('event: error\ndata: {"code":"artifact_corrupt","message":"Bad digest"}\n\n', {
        headers: { 'content-type': 'text/event-stream' },
      }),
    )) as unknown as typeof fetch
  try {
    await admin.pluginMarketplace.prepareInstall(input, () => {})
    throw new Error('expected failure')
  } catch (error) {
    expect((error as Error & { code: string }).code).toBe('artifact_corrupt')
    expect((error as Error).message).toBe('Bad digest')
  }
})

test('cancelled preparation rejects without returning a partial preview', async () => {
  const reading = Promise.withResolvers<void>()
  globalThis.fetch = ((_: unknown, init?: RequestInit) => {
    const body = new ReadableStream<Uint8Array>({
      start(stream) {
        stream.enqueue(new TextEncoder().encode('event: preview\ndata: {"id":'))
        init?.signal?.addEventListener('abort', () => stream.error(new DOMException('Aborted', 'AbortError')), {
          once: true,
        })
        reading.resolve()
      },
    })
    return Promise.resolve(new Response(body, { headers: { 'content-type': 'text/event-stream' } }))
  }) as unknown as typeof fetch
  const controller = new AbortController()
  const result = admin.pluginMarketplace.prepareInstall(input, () => {}, controller.signal)
  await reading.promise
  controller.abort()
  await Promise.resolve(expect(result).rejects.toThrow())
})

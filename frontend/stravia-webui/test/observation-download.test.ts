import { afterAll, beforeEach, expect, test } from 'bun:test'

const openedUrls: string[] = []
let openerError: Error | undefined
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window')
const originalDocument = Object.getOwnPropertyDescriptor(globalThis, 'document')

Object.defineProperty(globalThis, 'window', {
  configurable: true,
  value: {
    __TAURI_INTERNALS__: {
      async invoke(command: string, args?: { url: string }) {
        if (command === 'get_server_port') return 18473
        if (command === 'plugin:opener|open_url') {
          if (openerError) throw openerError
          openedUrls.push(args!.url)
          return
        }
        throw new Error(`Unexpected native command: ${command}`)
      },
    },
  },
})
Object.defineProperty(globalThis, 'document', {
  configurable: true,
  value: {
    createElement() {
      throw new Error('Desktop bundle export must not navigate the embedded WebView')
    },
  },
})

// 宿主检测在模块加载时执行，必须先安装桌面环境再加载被测模块。
const { navigateToBundle } = await import('../src/lib/observation-stream')

beforeEach(() => {
  openedUrls.length = 0
  openerError = undefined
})
afterAll(() => {
  if (originalWindow) Object.defineProperty(globalThis, 'window', originalWindow)
  else Reflect.deleteProperty(globalThis, 'window')
  if (originalDocument) Object.defineProperty(globalThis, 'document', originalDocument)
  else Reflect.deleteProperty(globalThis, 'document')
})

const ticket = { download_url: '/api/v1/observations/debug-bundles/test-ticket', expires_at: 1, through_sequence: 10 }

test('desktop exports a bundle through the system browser without navigating the WebView', async () => {
  await navigateToBundle(ticket)
  expect(openedUrls).toEqual(['http://127.0.0.1:18473/api/v1/observations/debug-bundles/test-ticket'])
})

test('desktop reports a failed browser launch to the caller', async () => {
  openerError = new Error('System browser could not be opened')
  await expect(navigateToBundle(ticket)).rejects.toThrow('System browser could not be opened')
  expect(openedUrls).toEqual([])
})

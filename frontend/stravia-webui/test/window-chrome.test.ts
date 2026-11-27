import { afterEach, expect, test } from 'bun:test'
import { createWindowChrome, observeWindowMaximized, resolveWindowMaterial } from '../src/lib/window-chrome'

const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window')
const originalNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator')

function setGlobal(name: 'window' | 'navigator', value: unknown): void {
  Object.defineProperty(globalThis, name, { configurable: true, value })
}

afterEach(() => {
  if (originalWindow) Object.defineProperty(globalThis, 'window', originalWindow)
  else Reflect.deleteProperty(globalThis, 'window')
  if (originalNavigator) Object.defineProperty(globalThis, 'navigator', originalNavigator)
  else Reflect.deleteProperty(globalThis, 'navigator')
})

test('resolveWindowMaterial trusts host-injected capability and falls back conservatively on Windows', () => {
  expect(resolveWindowMaterial('windows', 'mica')).toBe('mica')
  expect(resolveWindowMaterial('windows', 'opaque')).toBe('opaque')
  expect(resolveWindowMaterial('windows', undefined)).toBe('opaque')
  expect(resolveWindowMaterial('windows', 'acrylic')).toBe('opaque')
  expect(resolveWindowMaterial('macos', undefined)).toBe('sidebar')
  expect(resolveWindowMaterial('macos', 'opaque')).toBe('opaque')
  expect(resolveWindowMaterial('linux', undefined)).toBe('opaque')
})

test('createWindowChrome reflects Windows 11 mica and Windows 10 opaque host capabilities', () => {
  setGlobal('navigator', { platform: 'Win32', userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)' })

  setGlobal('window', { __TAURI_INTERNALS__: {}, __STRAVIA_WINDOW_MATERIAL__: 'mica' })
  const win11 = createWindowChrome()
  expect(win11.material).toBe('mica')
  expect(win11.controls).toBe('custom')

  setGlobal('window', { __TAURI_INTERNALS__: {}, __STRAVIA_WINDOW_MATERIAL__: 'opaque' })
  const win10 = createWindowChrome()
  expect(win10.material).toBe('opaque')
  expect(win10.controls).toBe('custom')

  setGlobal('window', {})
  const web = createWindowChrome()
  expect(web.material).toBe('opaque')
  expect(web.controls).toBe('none')
})

test('observeWindowMaximized subscribes only to resize events and coalesces concurrent reads', async () => {
  let resizeHandler: (() => void) | undefined
  let unlistenCount = 0
  let onMovedCalled = false
  const pendingReads: Array<PromiseWithResolvers<boolean>> = []
  const reported: boolean[] = []

  const target = {
    isMaximized() {
      const deferred = Promise.withResolvers<boolean>()
      pendingReads.push(deferred)
      return deferred.promise
    },
    onResized(handler: () => void) {
      resizeHandler = handler
      return Promise.resolve(() => {
        unlistenCount++
      })
    },
    onMoved() {
      onMovedCalled = true
      return Promise.resolve(() => {})
    },
  }

  const observerPromise = observeWindowMaximized(target, (maximized) => {
    reported.push(maximized)
  })
  await Promise.resolve()
  expect(onMovedCalled).toBe(false)
  expect(pendingReads).toHaveLength(1)

  pendingReads[0].resolve(false)
  const dispose = await observerPromise
  expect(reported).toEqual([false])

  // 连续触发多次尺寸事件只应单飞发起查询，并在首次完成后补查一次最终状态。
  resizeHandler?.()
  resizeHandler?.()
  resizeHandler?.()
  expect(pendingReads).toHaveLength(2)

  pendingReads[1].resolve(false)
  await Promise.resolve()
  await Promise.resolve()
  expect(pendingReads).toHaveLength(3)
  expect(reported).toEqual([false])

  pendingReads[2].resolve(true)
  await Promise.resolve()
  await Promise.resolve()
  expect(reported).toEqual([false, true])

  dispose()
  expect(unlistenCount).toBe(1)

  resizeHandler?.()
  expect(pendingReads).toHaveLength(3)
})

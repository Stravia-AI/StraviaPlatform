import { getCurrentWindow } from '@tauri-apps/api/window'
import type { Window as TauriWindow } from '@tauri-apps/api/window'

type ShellPlatform = 'windows' | 'macos' | 'linux' | 'other'
export type WindowMaterial = 'opaque' | 'mica' | 'sidebar'
export type WindowControlsMode = 'none' | 'custom' | 'native'

export interface WindowChrome {
  readonly material: WindowMaterial
  readonly controls: WindowControlsMode
  syncTheme: (theme: 'light' | 'dark' | null) => void
  startDrag: (event: MouseEvent) => void
  toggleMaximize: (event?: MouseEvent) => void
  minimize: () => void
  close: () => void
  isMaximized: () => Promise<boolean>
  observeMaximized: (listener: (maximized: boolean) => void) => Promise<() => void>
}

interface MaximizedWindowObserverTarget {
  isMaximized: () => Promise<boolean>
  onResized: (handler: () => void) => Promise<() => void>
}

const DRAG_SUPPRESSION_SELECTOR =
  "button,a,input,textarea,select,[role='button'],[data-no-drag],[data-window-drag='false']"

function detectPlatform(): ShellPlatform {
  if (typeof navigator === 'undefined') return 'other'

  const fingerprint = `${navigator.platform} ${navigator.userAgent}`.toLowerCase()
  if (fingerprint.includes('mac')) return 'macos'
  if (fingerprint.includes('win')) return 'windows'
  if (fingerprint.includes('linux')) return 'linux'
  return 'other'
}

function isWindowMaterial(value: unknown): value is WindowMaterial {
  return value === 'opaque' || value === 'mica' || value === 'sidebar'
}

export function resolveWindowMaterial(platform: ShellPlatform, hostMaterial: unknown): WindowMaterial {
  if (isWindowMaterial(hostMaterial)) return hostMaterial
  // Windows User-Agent 已冻结为 NT 10.0，无法区分 Win10/Win11；未注入宿主能力时保守回退实色。
  return platform === 'macos' ? 'sidebar' : 'opaque'
}

export async function observeWindowMaximized(
  target: MaximizedWindowObserverTarget,
  listener: (maximized: boolean) => void,
): Promise<() => void> {
  let disposed = false
  let unlistenResize: (() => void) | undefined
  let inFlight = false
  let pending = false
  let lastReported: boolean | undefined

  const sync = async () => {
    if (disposed) return
    if (inFlight) {
      pending = true
      return
    }
    inFlight = true
    try {
      do {
        pending = false
        try {
          const maximized = await target.isMaximized()
          if (!disposed && maximized !== lastReported) {
            lastReported = maximized
            listener(maximized)
          }
        } catch (error) {
          console.error('Stravia window state read failed', error)
        }
      } while (pending && !disposed)
    } finally {
      inFlight = false
    }
  }

  try {
    // 仅监听 onResized：最大化/还原与贴边均伴随尺寸变化；不监听 onMoved，避免拖拽时触发高频 IPC。
    unlistenResize = await target.onResized(() => {
      void sync()
    })
    await sync()
  } catch (error) {
    unlistenResize?.()
    console.error('Stravia window state observer failed', error)
  }

  return () => {
    disposed = true
    unlistenResize?.()
  }
}

function isEligibleTitlebarEvent(event: MouseEvent, allowDoubleClick: boolean): boolean {
  if (event.button !== 0 || (!allowDoubleClick && event.detail >= 2)) return false
  return !(event.target instanceof Element && event.target.closest(DRAG_SUPPRESSION_SELECTOR) !== null)
}

async function withCurrentWindow(operation: string, action: (current: TauriWindow) => Promise<void>): Promise<void> {
  try {
    await action(getCurrentWindow())
  } catch (error) {
    console.error(`Stravia window ${operation} failed`, error)
  }
}

const webWindowChrome: WindowChrome = {
  material: 'opaque',
  controls: 'none',
  syncTheme: () => {},
  startDrag: () => {},
  toggleMaximize: () => {},
  minimize: () => {},
  close: () => {},
  isMaximized: () => Promise.resolve(false),
  observeMaximized: () => Promise.resolve(() => {}),
}

function createDesktopWindowChrome(platform: ShellPlatform): WindowChrome {
  const controls: WindowControlsMode = platform === 'macos' ? 'native' : 'custom'
  const hostMaterial = typeof window === 'undefined' ? undefined : window.__STRAVIA_WINDOW_MATERIAL__
  const material = resolveWindowMaterial(platform, hostMaterial)

  return {
    material,
    controls,
    syncTheme(theme) {
      void withCurrentWindow('theme sync', (current) => current.setTheme(theme))
    },
    startDrag(event) {
      if (!isEligibleTitlebarEvent(event, false)) return
      void withCurrentWindow('drag', (current) => current.startDragging())
    },
    toggleMaximize(event) {
      if (event && !isEligibleTitlebarEvent(event, true)) return
      if (event) event.preventDefault()
      void withCurrentWindow('maximize toggle', (current) => current.toggleMaximize())
    },
    minimize() {
      void withCurrentWindow('minimize', (current) => current.minimize())
    },
    close() {
      void withCurrentWindow('close', (current) => current.close())
    },
    async isMaximized() {
      try {
        return await getCurrentWindow().isMaximized()
      } catch (error) {
        console.error('Stravia window state read failed', error)
        return false
      }
    },
    async observeMaximized(listener) {
      return observeWindowMaximized(getCurrentWindow(), listener)
    },
  }
}

export function createWindowChrome(): WindowChrome {
  const isNative = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
  return isNative ? createDesktopWindowChrome(detectPlatform()) : webWindowChrome
}

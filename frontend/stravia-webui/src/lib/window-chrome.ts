import { getCurrentWindow } from '@tauri-apps/api/window'
import type { Window as TauriWindow } from '@tauri-apps/api/window'

type ShellPlatform = 'windows' | 'macos' | 'linux' | 'other'
export type WindowMaterial = 'opaque' | 'acrylic' | 'sidebar'
export type WindowControlsMode = 'none' | 'custom' | 'native'

export interface WindowChrome {
  readonly material: WindowMaterial
  readonly controls: WindowControlsMode
  syncTheme(theme: 'light' | 'dark' | null): void
  startDrag(event: MouseEvent): void
  toggleMaximize(event?: MouseEvent): void
  minimize(): void
  close(): void
  isMaximized(): Promise<boolean>
  observeMaximized(listener: (maximized: boolean) => void): Promise<() => void>
}

const DRAG_SUPPRESSION_SELECTOR =
  "button,a,input,textarea,select,[role='button'],[data-no-drag],[data-window-drag='false']"
const isNativeRuntime = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

function detectPlatform(): ShellPlatform {
  if (typeof navigator === 'undefined') return 'other'

  const fingerprint = `${navigator.platform} ${navigator.userAgent}`.toLowerCase()
  if (fingerprint.includes('mac')) return 'macos'
  if (fingerprint.includes('win')) return 'windows'
  if (fingerprint.includes('linux')) return 'linux'
  return 'other'
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
  isMaximized: async () => false,
  observeMaximized: async () => () => {},
}

function createDesktopWindowChrome(platform: ShellPlatform): WindowChrome {
  const controls: WindowControlsMode = platform === 'macos' ? 'native' : 'custom'
  const material: WindowMaterial = platform === 'windows' ? 'acrylic' : platform === 'macos' ? 'sidebar' : 'opaque'

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
      let disposed = false
      let unlistenResize: (() => void) | undefined
      let unlistenMove: (() => void) | undefined

      try {
        const current = getCurrentWindow()
        const notify = async () => {
          try {
            const maximized = await current.isMaximized()
            if (!disposed) listener(maximized)
          } catch (error) {
            console.error('Stravia window state read failed', error)
          }
        }

        unlistenResize = await current.onResized(notify)
        unlistenMove = await current.onMoved(notify)
        await notify()
      } catch (error) {
        unlistenResize?.()
        unlistenMove?.()
        console.error('Stravia window state observer failed', error)
      }

      return () => {
        disposed = true
        unlistenResize?.()
        unlistenMove?.()
      }
    },
  }
}

export function createWindowChrome(): WindowChrome {
  return isNativeRuntime ? createDesktopWindowChrome(detectPlatform()) : webWindowChrome
}

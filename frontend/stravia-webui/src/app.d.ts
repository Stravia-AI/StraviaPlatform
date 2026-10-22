import type { RouteErrorReason } from '#lib/route-error.js'

declare global {
  interface Window {
    __TAURI_INTERNALS__?: unknown
  }

  namespace App {
    interface Error {
      message: string
      /** 经分类的安全失败原因；缺省时错误页只显示通用恢复说明。 */
      reason?: RouteErrorReason
    }
  }
}

export {}

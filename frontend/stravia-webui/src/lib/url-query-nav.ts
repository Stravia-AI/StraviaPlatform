import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import type { Path } from '$app/types'
import { sameRelativeUrl, withQueryParams } from '#lib/url-query-state.js'

export interface SyncQueryParamsOptions {
  /** 等于默认值的参数会从 URL 中省略。 */
  defaults?: Readonly<Record<string, string>>
  /** 默认替换当前历史记录，避免同页频繁切 Tab 污染浏览器返回栈。 */
  replace?: boolean
  /** 默认非浅更新，确保 `page.url` 与地址栏同步且保持 `reset: false` 不重置滚动/焦点。 */
  shallow?: boolean
}

/**
 * 将当前路由的查询参数同步到地址栏；若目标 URL 与当前一致则直接跳过。
 */
export async function syncQueryParams(
  updates: Record<string, string | null | undefined>,
  options: SyncQueryParamsOptions = {},
): Promise<void> {
  const current = page.shallow?.url ?? page.url
  const next = withQueryParams(current, updates, options.defaults)
  if (sameRelativeUrl(current, next)) return
  const shallow = options.shallow ?? false
  const href = `${next.pathname.slice(resolve('').length)}${next.search}${next.hash}` as Path
  await goto(resolve(href), { replace: options.replace ?? true, shallow, reset: false })
}

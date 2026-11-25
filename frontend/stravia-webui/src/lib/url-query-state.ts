/**
 * 解析 URL 查询参数中的枚举值；缺失或不在白名单内时回退到默认值。
 */
export function parseEnumParam<T extends string>(
  params: Pick<URLSearchParams, 'get'>,
  key: string,
  allowed: readonly T[],
  fallback: T,
): T {
  const value = params.get(key)
  if (value !== null && (allowed as readonly string[]).includes(value)) {
    return value as T
  }
  return fallback
}

/**
 * 基于当前 URL 构造更新查询参数后的新 URL（不修改原对象）。
 * 值为 `null`、`undefined`、`''` 或等于 `defaults[key]` 时移除该参数，保持默认 URL 整洁。
 */
export function withQueryParams(
  current: Pick<URL, 'href'>,
  updates: Record<string, string | null | undefined>,
  defaults?: Readonly<Record<string, string>>,
): URL {
  const next = new URL(current.href)
  for (const [key, value] of Object.entries(updates)) {
    const defaultValue = defaults?.[key]
    if (value == null || value === '' || (defaultValue !== undefined && value === defaultValue)) {
      next.searchParams.delete(key)
    } else {
      next.searchParams.set(key, value)
    }
  }
  return next
}

/**
 * 比较两个 URL 的站内路径、查询串与 hash 是否完全一致。
 */
export function sameRelativeUrl(
  left: Pick<URL, 'pathname' | 'search' | 'hash'>,
  right: Pick<URL, 'pathname' | 'search' | 'hash'>,
): boolean {
  return left.pathname === right.pathname && left.search === right.search && left.hash === right.hash
}

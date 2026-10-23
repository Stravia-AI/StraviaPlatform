/** 平台功能的读取状态：只有读到明确启用时才允许子项开关；加载中与读取失败都不可当作「未启用」。 */
export type CapabilityStatus = 'enabled' | 'disabled' | 'loading' | 'unavailable'

export function capabilityStatus<T>(
  query: { isPending: boolean; isError: boolean; data: T | undefined },
  isEnabled: (data: T) => boolean,
): CapabilityStatus {
  if (query.isPending) return 'loading'
  if (query.isError) return 'unavailable'
  return query.data !== undefined && isEnabled(query.data) ? 'enabled' : 'disabled'
}

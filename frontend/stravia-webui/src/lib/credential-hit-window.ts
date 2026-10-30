import type { CredentialDiscoveryQuery } from '#lib/types.js'

/**
 * 命中记录按创建时间筛选的时间窗口；起点含、终点不含。
 *
 * 实时窗口没有固定起止：终点始终是现在，新命中持续进入；固定窗口的起止由用户选定。
 * `custom` 只影响展示：窗口长度是用户自己填的，而不是某个预设。
 */
export type HitWindow =
  | { live: true; spanMs: number; custom: boolean }
  | { live: false; spanMs: number; custom: boolean; startAt: number; endAt: number }

export function customHitWindow(startAt: number, endAt: number): HitWindow {
  return { live: false, custom: true, spanMs: endAt - startAt, startAt, endAt }
}

export function isValidHitRange(startAt: number, endAt: number): boolean {
  return Number.isFinite(startAt) && Number.isFinite(endAt) && endAt > startAt
}

export function hitWindowBounds(window: HitWindow, now: number): { startAt: number; endAt: number } {
  return window.live ? { startAt: now - window.spanMs, endAt: now } : window
}

/**
 * 按窗口长度整段平移。向较早平移时，从实时窗口出发的新窗口紧邻实时窗口的起点；
 * 向较新平移时，终点落在最近一个窗口长度之内的窗口就是实时窗口：回到实时窗口，
 * 而不是停在一个比现在早几秒、终点在“刚才”的固定窗口上，也不会出现终点在未来的窗口。
 */
export function shiftHitWindow(window: HitWindow, direction: -1 | 1, now: number): HitWindow {
  const endAt = hitWindowBounds(window, now).endAt + direction * window.spanMs
  if (direction > 0 && endAt > now - window.spanMs) return { live: true, custom: window.custom, spanMs: window.spanMs }
  return { live: false, custom: window.custom, spanMs: window.spanMs, startAt: endAt - window.spanMs, endAt }
}

/** 查询参数。实时窗口只带起点，起点随时间滚动，新命中不会被终点挡在外面。 */
export function hitWindowQuery(
  window: HitWindow,
  now: number,
): Required<Pick<CredentialDiscoveryQuery, 'start_at'>> & Pick<CredentialDiscoveryQuery, 'end_at'> {
  return window.live ? { start_at: now - window.spanMs } : { start_at: window.startAt, end_at: window.endAt }
}

/** `datetime-local` 输入框使用的本地时间，精确到秒。 */
export function toLocalDateTimeInput(timestamp: number): string {
  const date = new Date(timestamp)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}

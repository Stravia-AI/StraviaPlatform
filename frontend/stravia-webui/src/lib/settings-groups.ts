import * as m from '#lib/paraglide/messages.js'

/** 设置分组：按用户任务划分，用 `?view=` 切换。分组的宿主可用性和显示名在此集中判断，页面只渲染可用分组。 */

export const settingsGroups = ['general', 'network', 'data', 'account', 'advanced'] as const

export type SettingsGroup = (typeof settingsGroups)[number]

export interface SettingsHost {
  /** 提供管理员账号的 Server 宿主。 */
  server: boolean
}

/** 当前宿主可用的分组，按任务顺序排列；不适用的分组直接缺席。 */
export function availableSettingsGroups(host: SettingsHost): SettingsGroup[] {
  return settingsGroups.filter((group) => group !== 'account' || host.server)
}

/** 分组显示名；导航与页面标题共用同一份映射。 */
export const settingsGroupLabels = {
  general: m.settings_group_general,
  network: m.settings_group_network,
  data: m.settings_group_data,
  account: m.settings_group_account,
  advanced: m.settings_group_advanced,
} as const satisfies Record<SettingsGroup, () => string>

/** 缺省、未知或当前宿主不可用的值显示「通用」；调用方不改写地址。 */
export function resolveSettingsGroup(value: string | null, available: readonly SettingsGroup[]): SettingsGroup {
  return available.find((group) => group === value) ?? 'general'
}

/** 指向分组（可选分区锚点）的站内相对地址（交给 `resolve()`），供导航与跨页入口使用。 */
export function settingsGroupHref(group: SettingsGroup, anchor?: string): string {
  return `settings?view=${group}${anchor ? `#${anchor}` : ''}`
}

import * as m from '#lib/paraglide/messages.js'
import type { Locale } from '#lib/localization.svelte.js'
import { resolvePluginText } from '#lib/plugin-text.js'
import type { ProviderMetadata, VendorChannelDescriptor } from '#lib/types.js'

/**
 * One selectable card: a provider profile channel as supplied by one plugin.
 * `metadata.provider_id` stays the execution identity; the card key also carries
 * the plugin so profiles from different plugins never collapse into one option.
 */
export interface ProviderOption {
  key: string
  metadata: ProviderMetadata
  channel: VendorChannelDescriptor
}

export function buildProviderOptions(entries: ProviderMetadata[]): ProviderOption[] {
  const options = entries.flatMap((metadata) =>
    metadata.channels.map((channel) => ({
      key: `${metadata.plugin.id}/${metadata.provider_id}/${channel.id}`,
      metadata,
      channel,
    })),
  )
  // The generic Custom profile covers any endpoint; pin it ahead of the branded vendors.
  return options.toSorted(
    (left, right) => Number(right.metadata.provider_id === 'custom') - Number(left.metadata.provider_id === 'custom'),
  )
}

export function optionLabel(option: ProviderOption, locale: Locale): string {
  return option.metadata.provider_id === 'custom'
    ? m.provider_options_custom({}, { locale })
    : option.metadata.display_name
}

/** Directory identity shown on cards and used for the logo; never used to dispatch. */
export function optionDirectoryId(option: ProviderOption): string {
  return option.metadata.catalog_id ?? option.metadata.provider_id
}

export function optionPluginSource(option: ProviderOption, locale: Locale): string {
  const { name, id } = option.metadata.plugin
  return m.provider_options_plugin_source({ name, id }, { locale })
}

/**
 * `base` 随宿主发布，覆盖绝大多数卡片；只有其余插件提供的入口才需要在卡片上标注来源，
 * 避免同一来源在每张卡片上重复。完整的插件名称与 ID 仍在连接信息步骤和搜索中可用。
 */
export function optionExternalPlugin(option: ProviderOption): ProviderMetadata['plugin'] | undefined {
  return option.metadata.plugin.id === 'base' ? undefined : option.metadata.plugin
}

function optionAuthKind(option: ProviderOption, locale: Locale): string {
  return option.channel.auth
    ? m.common_oauth_account({}, { locale })
    : option.metadata.config_fields.some((field) => field.secret && field.key === 'setup_token')
      ? m.provider_options_setup_token({}, { locale })
      : m.provider_options_api_key({}, { locale })
}

export function optionDescription(option: ProviderOption, locale: Locale): string {
  const auth = optionAuthKind(option, locale)
  return option.channel.id === 'default' ? auth : `${resolvePluginText(option.channel.name, locale)} · ${auth}`
}

/** Search covers the displayed text plus the real provider id, directory id, and source plugin. */
export function providerOptionMatches(option: ProviderOption, query: string, locale: Locale): boolean {
  const needle = query.trim().toLocaleLowerCase(locale)
  if (!needle) return true

  const text = [
    option.metadata.provider_id,
    option.metadata.catalog_id ?? '',
    option.metadata.plugin.name,
    option.metadata.plugin.id,
    optionLabel(option, locale),
    option.channel.id,
    resolvePluginText(option.channel.name, locale),
    optionDescription(option, locale),
    option.channel.auth ? 'oauth account 账号' : 'api key',
  ].join(' ')

  return text.toLocaleLowerCase(locale).includes(needle)
}

export function defaultProviderName(option: ProviderOption): string {
  return option.metadata.channels.length > 1
    ? `${option.metadata.display_name} ${resolvePluginText(option.channel.name, 'en-US')}`
    : option.metadata.display_name
}

export function providerNameAfterOptionChange(
  currentName: string,
  previousOption: ProviderOption | undefined,
  nextOption: ProviderOption,
): string {
  const trimmedName = currentName.trim()
  const previousDefault = previousOption ? defaultProviderName(previousOption) : ''
  return trimmedName && trimmedName !== previousDefault ? trimmedName : defaultProviderName(nextOption)
}

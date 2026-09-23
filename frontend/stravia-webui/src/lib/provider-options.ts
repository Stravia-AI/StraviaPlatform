import * as m from '$lib/paraglide/messages.js'
import type { Locale } from '$lib/localization.svelte'
import { resolvePluginText } from '$lib/plugin-text'
import type { ProviderDescriptor, VendorChannelDescriptor } from '$lib/types'

export interface ProviderOption {
  key: string
  descriptor: ProviderDescriptor
  channel: VendorChannelDescriptor
}

export function buildProviderOptions(descriptors: ProviderDescriptor[]): ProviderOption[] {
  return descriptors.flatMap((descriptor) =>
    descriptor.channels.map((channel) => ({ key: `${descriptor.provider_id}/${channel.id}`, descriptor, channel })),
  )
}

export function optionLabel(option: ProviderOption): string {
  return option.descriptor.display_name
}

export function optionDescription(option: ProviderOption, locale: Locale): string {
  const auth = option.channel.auth
    ? m.common_oauth_account({}, { locale })
    : option.descriptor.config_fields.some((field) => field.secret && field.key === 'setup_token')
      ? m.provider_options_setup_token({}, { locale })
      : m.provider_options_api_key({}, { locale })
  return option.channel.id === 'default' ? auth : `${resolvePluginText(option.channel.name, locale)} · ${auth}`
}

export function defaultProviderName(option: ProviderOption): string {
  return option.descriptor.channels.length > 1
    ? `${option.descriptor.display_name} ${resolvePluginText(option.channel.name, 'en-US')}`
    : option.descriptor.display_name
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

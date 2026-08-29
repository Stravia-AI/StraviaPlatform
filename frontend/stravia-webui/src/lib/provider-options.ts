import * as m from '$lib/paraglide/messages.js'
import { type Locale } from '$lib/localization.svelte'
import { resolveProtocol } from '$lib/protocol'
import type { CatalogAuthMode, CatalogChannel, CatalogProvider, ProviderProtocol } from '$lib/types'

const CUSTOM_PROVIDER: CatalogProvider = {
  id: 'custom',
  name: 'Custom',
  npm: '',
  vendor_id: 'custom',
  protocol: 'openai-compatible',
  base_url: '',
  channels: [
    {
      id: 'default',
      label: 'Default',
      protocol: 'openai-compatible',
      base_url: '',
      auth_mode: 'optional_api_key',
      fingerprint: 'custom',
    },
  ],
}

export interface ProviderOption {
  key: string
  preset: CatalogProvider
  channel: CatalogChannel
  presetKey: string
  channelKey: string
  authMode: 'apikey' | 'oauth'
  credentialMode: CatalogAuthMode
  isCustom: boolean
  protocols: Array<{ protocol: ProviderProtocol; baseUrl: string }>
}

export function buildProviderOptions(catalogProviders: CatalogProvider[]): ProviderOption[] {
  return [...catalogProviders, CUSTOM_PROVIDER].flatMap((preset) =>
    preset.channels.map((channel) => {
      const key = `${preset.id}/${channel.id}`
      const protocol = resolveProtocol(channel.protocol)
      return {
        key,
        preset,
        channel,
        presetKey: preset.id,
        channelKey: channel.id,
        authMode: channel.auth_mode === 'oauth' ? 'oauth' : 'apikey',
        credentialMode: channel.auth_mode,
        isCustom: preset.id === 'custom',
        protocols: protocol ? [{ protocol, baseUrl: channel.base_url }] : [],
      }
    }),
  )
}

export function optionLabel(option: ProviderOption, locale: Locale): string {
  return option.isCustom ? m.provider_options_custom({}, { locale }) : option.preset.name
}

export function optionDescription(option: ProviderOption, locale: Locale): string {
  if (option.isCustom) {
    return m.provider_options_bring_own_openai_compatible_endpoint({}, { locale: locale })
  }
  const auth =
    option.authMode === 'oauth'
      ? m.common_oauth_account({}, { locale: locale })
      : option.credentialMode === 'setup_token'
        ? m.provider_options_setup_token({}, { locale: locale })
        : m.provider_options_api_key({}, { locale: locale })
  return option.channelKey === 'default' ? auth : `${option.channel.label} · ${auth}`
}

export function defaultProviderName(option: ProviderOption, locale: Locale): string {
  if (option.authMode === 'oauth') return option.channel.label
  if (option.isCustom) return m.provider_options_custom({}, { locale })
  if (option.channelKey === 'default') return option.preset.name
  return `${option.preset.name} ${option.channel.label}`
}

export function providerNameAfterOptionChange(
  currentName: string,
  previousOption: ProviderOption | undefined,
  nextOption: ProviderOption,
  locale: Locale,
): string {
  const trimmedName = currentName.trim()
  const previousDefault = previousOption ? defaultProviderName(previousOption, locale) : ''
  return trimmedName && trimmedName !== previousDefault ? trimmedName : defaultProviderName(nextOption, locale)
}

export function oauthDriverKey(option: ProviderOption): string {
  return option.authMode === 'oauth' ? option.channelKey : option.presetKey
}

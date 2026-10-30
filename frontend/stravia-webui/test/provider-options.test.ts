import { describe, expect, test } from 'bun:test'

import { buildProviderOptions, optionExternalPlugin, providerOptionMatches } from '../src/lib/provider-options'
import type { ProviderMetadata } from '../src/lib/types'

function metadata(pluginId: string, overrides: Partial<ProviderMetadata> = {}): ProviderMetadata {
  return {
    provider_id: 'command-code',
    catalog_id: 'commandcode',
    display_name: 'CommandCode',
    description: null,
    plugin: { id: pluginId, name: `Plugin ${pluginId}` },
    channels: [{ id: 'default', name: { 'en-US': 'Default' }, capabilities: ['infer'] }],
    capabilities: ['infer'],
    config_fields: [],
    config_groups: [],
    network: { extra_origins: [], field_origins: [] },
    data_compat: { config_fields_format: 1, private_state_format: 1, credentials_format: 1, model_metadata_format: 1 },
    ...overrides,
  }
}

describe('provider options', () => {
  test('search matches the real provider id, directory id, and source plugin', () => {
    const [option, base] = buildProviderOptions([
      metadata('command-code', { plugin: { id: 'command-code', name: 'Community CLI' } }),
      metadata('base', { provider_id: 'openai', catalog_id: 'openai', display_name: 'OpenAI' }),
    ])

    for (const query of ['command-code', 'commandcode', ' Community CLI ']) {
      expect(providerOptionMatches(option, query, 'en-US')).toBe(true)
      expect(providerOptionMatches(base, query, 'en-US')).toBe(false)
    }
    expect(providerOptionMatches(base, 'base', 'zh-CN')).toBe(true)
    expect(providerOptionMatches(option, 'base', 'zh-CN')).toBe(false)
    expect(providerOptionMatches(option, 'other-plugin', 'en-US')).toBe(false)
  })

  test('only plugins other than the bundled base are surfaced as card sources', () => {
    const [external, base] = buildProviderOptions([
      metadata('command-code', { plugin: { id: 'command-code', name: 'Community CLI' } }),
      metadata('base', { plugin: { id: 'base', name: 'Stravia Base Providers' } }),
    ])

    expect(optionExternalPlugin(external)).toEqual({ id: 'command-code', name: 'Community CLI' })
    expect(optionExternalPlugin(base)).toBeUndefined()
  })
})

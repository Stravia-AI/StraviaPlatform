import { describe, expect, test } from 'bun:test'

import {
  buildProviderOptions,
  defaultProviderName,
  oauthDriverKey,
  providerNameAfterOptionChange,
} from '../src/lib/provider-options'
import type { CatalogProvider } from '../src/lib/types'

const xai: CatalogProvider = {
  id: 'xai',
  name: 'xAI',
  npm: '@ai-sdk/xai',
  vendor_id: 'xai',
  protocol: 'openai-compatible',
  base_url: 'https://api.x.ai/v1',
  channels: [
    {
      id: 'grok',
      label: 'grok',
      protocol: 'open-responses',
      base_url: 'https://cli-chat-proxy.grok.com/v1',
      auth_mode: 'oauth',
      fingerprint: 'grok',
    },
  ],
}

describe('provider option names', () => {
  test('switching from the custom default uses the Grok default name', () => {
    const options = buildProviderOptions([xai])
    const custom = options.find((option) => option.isCustom)
    const grok = options.find((option) => option.channelKey === 'grok')

    expect(custom).toBeDefined()
    expect(grok).toBeDefined()
    const customName = defaultProviderName(custom!, 'zh-CN')
    expect(providerNameAfterOptionChange(customName, custom, grok!, 'zh-CN')).toBe('grok')
  })

  test('switching options preserves a user-defined connection name', () => {
    const options = buildProviderOptions([xai])
    const custom = options.find((option) => option.isCustom)
    const grok = options.find((option) => option.channelKey === 'grok')

    expect(providerNameAfterOptionChange('My Grok', custom, grok!, 'en-US')).toBe('My Grok')
  })

  // The catalog synthesizes Devin as a built-in entry whose single channel id
  // doubles as the OAuth driver key (same convention as openai/codex and
  // xai/grok), so the connect flow must resolve driver "devin" and a "Devin"
  // default name without any special-casing.
  test('devin OAuth option resolves the devin driver and provider name', () => {
    const devin: CatalogProvider = {
      id: 'devin',
      name: 'Devin',
      npm: '',
      vendor_id: 'devin',
      protocol: 'devin-connect',
      base_url: 'https://server.codeium.com',
      channels: [
        {
          id: 'devin',
          label: 'Devin',
          protocol: 'devin-connect',
          base_url: 'https://server.codeium.com',
          auth_mode: 'oauth',
          fingerprint: 'devin',
        },
      ],
    }
    const options = buildProviderOptions([devin])
    const option = options.find((option) => option.presetKey === 'devin')

    expect(option).toBeDefined()
    expect(option!.authMode).toBe('oauth')
    expect(oauthDriverKey(option!)).toBe('devin')
    expect(defaultProviderName(option!, 'zh-CN')).toBe('Devin')
  })
})

import { describe, expect, test } from 'bun:test'

import {
  buildProviderOptions,
  defaultProviderName,
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
})

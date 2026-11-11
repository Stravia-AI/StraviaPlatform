import { expect, test } from 'bun:test'

import { localizeMarketplaceError } from '#lib/backend-error.js'

test('marketplace failures are localized for direct and encoded errors', () => {
  for (const code of [
    'version_conflict',
    'incompatible',
    'artifact_corrupt',
    'preview_expired',
    'source_disabled',
    'resource_limit',
    'network',
    'signature_invalid',
  ]) {
    const payload = { code, message: 'UNLOCALIZED_MARKETPLACE_ERROR' }
    for (const locale of ['en-US', 'zh-CN'] as const) {
      const direct = localizeMarketplaceError(payload, locale)
      const encoded = localizeMarketplaceError(new Error(JSON.stringify(payload)), locale)
      expect(direct).not.toContain(payload.message)
      expect(encoded).toBe(direct)
    }
    expect(localizeMarketplaceError(payload, 'en-US')).not.toBe(localizeMarketplaceError(payload, 'zh-CN'))
  }
})

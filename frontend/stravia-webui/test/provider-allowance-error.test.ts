import { describe, expect, test } from 'bun:test'

import { localizeBackendErrorMessage } from '#lib/backend-error.js'

describe('route purpose recovery errors', () => {
  test('preserves Core error destination and route identities through encoded error messages', () => {
    const params = {
      provider_id: 'provider-unique-937',
      model_id: 'model-unique-482',
      route_id: 'route-unique-651',
      purpose: 'image_generation',
    }
    for (const locale of ['en-US', 'zh-CN'] as const) {
      for (const code of ['ROUTE_PURPOSE_MISMATCH', 'ROUTE_PURPOSE_UNREGISTERED']) {
        const error = new Error(JSON.stringify({ code, params, message: 'RAW_TARGET_SENTINEL' }))
        const message = localizeBackendErrorMessage(error, locale)
        expect(message).toContain(params.provider_id)
        expect(message).toContain(params.model_id)
        expect(message).not.toContain('RAW_TARGET_SENTINEL')
        if (code === 'ROUTE_PURPOSE_MISMATCH') {
          const searchError = new Error(JSON.stringify({ code, params: { ...params, purpose: 'web_search' } }))
          expect(localizeBackendErrorMessage(searchError, locale)).toContain(locale === 'en-US' ? 'Web Search' : '联网搜索')
        }
      }
      for (const code of ['ROUTE_IMAGE_GENERATION_BOUND', 'ROUTE_WEB_SEARCH_BOUND', 'ROUTE_PURPOSE_UNCONFIGURED', 'ROUTE_NOT_FOUND']) {
        const error = new Error(JSON.stringify({ code, params, message: 'RAW_ROUTE_SENTINEL' }))
        const message = localizeBackendErrorMessage(error, locale)
        expect(message).toContain(params.route_id)
        expect(message).not.toContain('RAW_ROUTE_SENTINEL')
      }
    }
  })

  test('localizes historical and media errors without leaking backend fallback messages', () => {
    const codes = [
      'ROUTE_PURPOSE_UNCONFIGURED',
      'ROUTE_PURPOSE_UNREGISTERED',
      'ROUTE_PURPOSE_MISMATCH',
      'ROUTE_IMAGE_GENERATION_BOUND',
      'ROUTE_WEB_SEARCH_BOUND',
      'route_purpose_unconfigured',
      'route_purpose_unavailable',
      'route_purpose_mismatch',
      'WEB_SEARCH_MODEL_INELIGIBLE',
      'WEB_SEARCH_ROUTE_UNAVAILABLE',
      'target_purpose_mismatch',
      'media_generation_route_purpose_mismatch',
      'media_generation_targets_purpose_mismatch',
    ]
    for (const locale of ['en-US', 'zh-CN'] as const) {
      for (const code of codes) {
        const error = { code, message: `UNLOCALIZED_BACKEND_FALLBACK_${code}` }
        expect(localizeBackendErrorMessage(error, locale)).not.toContain(error.message)
      }
      const drift = localizeBackendErrorMessage({ code: 'media_generation_targets_purpose_mismatch' }, locale)
      const missing = localizeBackendErrorMessage({ code: 'media_generation_targets_missing' }, locale)
      expect(drift).not.toBe(missing)
    }
  })
})

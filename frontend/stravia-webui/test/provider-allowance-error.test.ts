import { describe, expect, test } from 'bun:test'

import { localizeBackendErrorMessage } from '$lib/backend-error'

describe('provider allowance backend errors', () => {
  test('localizes an unavailable saved provider without exposing the backend fallback', () => {
    const error = { code: 'PROVIDER_ALLOWANCE_UNAVAILABLE', message: 'provider allowance is unavailable' }

    expect(localizeBackendErrorMessage(error, 'en-US')).toBe(
      'Allowance data is no longer available for this model service.',
    )
    expect(localizeBackendErrorMessage(error, 'zh-CN')).toBe('此模型服务已无可用额度数据。')
  })
})

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

  test('tells the user which thinking rows to change', () => {
    const error = {
      code: 'THINKING_CONTROL_UNREPRESENTABLE',
      message: 'Target protocol cannot write this Target Thinking Control',
      params: {
        provider_id: 'ollama-cloud',
        model_id: 'gemma4:31b',
        levels: ['off', 'medium'],
        controls: ['disabled', 'enabled'],
        supported_controls: ['effort', 'hidden'],
      },
    }

    expect(localizeBackendErrorMessage(error, 'en-US')).toBe(
      'Open destination ollama-cloud/gemma4:31b. Thinking levels off, medium currently use Disable thinking, Enable thinking, which this model service cannot send. In the Thinking Level Map, change them to: Effort value, Level unsupported.',
    )
    expect(localizeBackendErrorMessage(error, 'zh-CN')).toBe(
      '打开请求目标 ollama-cloud/gemma4:31b。思考等级 off、medium 当前使用 明确关闭思考、仅开启思考，此模型服务无法发送。请在思考等级映射中改成：按强度值、此等级不支持。',
    )
  })
})

import { describe, expect, test } from 'bun:test'

import { apiKeyAllowsModel, buildCode, CLI_TOOLS, defineClientModel, maskApiKey } from '../src/lib/connect'
import type { Route } from '../src/lib/types'

describe('Connect clients', () => {
  test('treats an empty API Key model scope as unrestricted', () => {
    expect(apiKeyAllowsModel([], 'model-id')).toBe(true)
    expect(apiKeyAllowsModel(['other-model-id'], 'model-id')).toBe(false)
  })

  test('masks API Keys consistently without exposing most of the prefix', () => {
    expect(maskApiKey('sk-d787f8575abcdef4482')).toBe('sk-d78••••••••4482')
    expect(maskApiKey('sk-short-key')).toBe('••••••••••••')
  })

  test('keeps code examples unchanged for every language', () => {
    const base = {
      protocol: 'open-responses' as const,
      model: 'gpt-5.6-sol',
      apiKey: 'sk-client',
      host: 'http://localhost:5174',
    }

    expect(buildCode({ ...base, language: 'curl' })).toContain('http://localhost:5174/v1/responses')
    expect(buildCode({ ...base, language: 'python' })).toContain('client.responses.create')
    expect(buildCode({ ...base, language: 'typescript' })).toContain('client.responses.create')
  })

  test('lists every supported Connect Client in product order', () => {
    expect(CLI_TOOLS.map((tool) => tool.name)).toEqual([
      'Codex',
      'Claude Code',
      'OpenCode',
      'OpenClaw',
      'Hermes Agent',
      'TRAE',
      'WorkBuddy',
      'ZCode',
      'DeepSeek Harness',
      'Pi',
      'OMP',
    ])
  })

  test('uses Route capabilities and falls back to Route ID for display', () => {
    const route = {
      id: 'route',
      model_id: 'shared-model',
      display_name: 'Shared Model',
      balance: 'weighted',
      target_provider: 'provider-a',
      target_model: 'upstream-a',
      is_enabled: true,
      created_at: '2026-08-05T00:00:00Z',
      supported_thinking_levels: ['off', 'low', 'high'],
      context_window: 128_000,
      output_max_tokens: 32_000,
      supports_image_input: true,
      targets: [],
    } satisfies Route
    const unnamed = { ...route, id: 'unnamed', model_id: 'custom/unnamed', display_name: null } satisfies Route

    expect(defineClientModel(route)).toEqual({
      modelId: 'shared-model',
      displayName: 'Shared Model',
      supportedThinkingLevels: ['off', 'low', 'high'],
      supportsImageInput: true,
      contextWindow: 128_000,
      outputMaxTokens: 32_000,
    })
    expect(defineClientModel(unnamed).displayName).toBe('custom/unnamed')
  })
})

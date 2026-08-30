import { describe, expect, test } from 'bun:test'

import { buildCliConfig, buildCode, defineClientModel, type ClientModelDefinition } from '../src/lib/connect'
import type { Model } from '../src/lib/types'

const models: ClientModelDefinition[] = [
  {
    name: 'claude-opus',
    supportedThinkingLevels: ['off', 'high', 'max'],
    contextWindow: 200_000,
    outputMaxTokens: 32_000,
  },
  {
    name: 'gpt-5.6-sol',
    supportedThinkingLevels: ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'],
    contextWindow: 272_000,
    outputMaxTokens: 128_000,
  },
  {
    name: 'gpt-5.6-luna',
    supportedThinkingLevels: ['off', 'low', 'medium', 'high'],
    contextWindow: 196_000,
    outputMaxTokens: 64_000,
  },
]
const modelNames = models.map((model) => model.name)

describe('client configuration generation', () => {
  test('generates Open Responses examples for every code language', () => {
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

  test('maps each Claude model family independently', () => {
    const config = buildCliConfig({
      tool: 'claude-code',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      mappings: {
        defaultModel: 'claude-opus',
        haikuModel: 'gpt-5.6-luna',
        sonnetModel: 'gpt-5.6-sol',
        opusModel: 'claude-opus',
      },
    })

    expect(config).toContain('"ANTHROPIC_MODEL": "claude-opus"')
    expect(config).toContain('"ANTHROPIC_DEFAULT_HAIKU_MODEL": "gpt-5.6-luna"')
    expect(config).toContain('"ANTHROPIC_DEFAULT_SONNET_MODEL": "gpt-5.6-sol"')
    expect(config).toContain('"ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus"')
    expect(config).toContain('"effortLevel": "high"')
    expect(config).toContain('"autoCompactWindow": 200000')
  })

  test('omits Claude settings that the selected model cannot represent', () => {
    const config = buildCliConfig({
      tool: 'claude-code',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models: [
        {
          name: 'small-model',
          supportedThinkingLevels: ['off', 'minimal', 'max'],
          contextWindow: 64_000,
          outputMaxTokens: 8_000,
        },
      ],
      mappings: {
        defaultModel: 'small-model',
        haikuModel: 'small-model',
        sonnetModel: 'small-model',
        opusModel: 'small-model',
      },
    })

    expect(config).not.toContain('"effortLevel"')
    expect(config).not.toContain('"autoCompactWindow"')
  })

  test('writes every authorized model to the Codex catalog', () => {
    const config = buildCliConfig({
      tool: 'codex-cli',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-sol',
    })

    expect(config).toContain('model = "gpt-5.6-sol"')
    expect(config).toContain('model_catalog_json = "stravia-models.json"')
    expect(config).toContain('env_key = "STRAVIA_API_KEY"')
    expect(config).not.toContain('auth.json')
    expect(config).not.toContain('requires_openai_auth')
    expect(config).not.toContain('"max_context_window"')
    expect(config).not.toContain('"model_messages"')
    expect(config).toContain('"base_instructions":')
    expect(config).toContain('"supports_parallel_tool_calls": false')
    expect(config).toContain('"context_window": 272000')
    expect(config).toContain('"effort": "none"')
    expect(config).toContain('"effort": "xhigh"')
    for (const model of modelNames) expect(config).toContain(`"slug": "${model}"`)
  })

  test('writes every authorized model to the OpenCode provider', () => {
    const config = buildCliConfig({
      tool: 'opencode',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-luna',
    })

    const json = JSON.parse(config.split('\n').slice(1).join('\n'))
    expect(Object.keys(json)).toEqual(['model', 'provider'])
    expect(Object.keys(json.provider.stravia)).toEqual(['npm', 'models', 'options'])
    expect(json.provider.stravia.npm).toBe('@ai-sdk/open-responses')
    expect(json.model).toBe('stravia/gpt-5.6-luna')
    expect(Object.keys(json.provider.stravia.models)).toEqual(modelNames)
    expect(Object.keys(json.provider.stravia.models['gpt-5.6-sol'])).toEqual([
      'reasoning',
      'variants',
      'limit',
    ])
    expect(json.provider.stravia.models['gpt-5.6-sol']).toMatchObject({
      reasoning: true,
      limit: { context: 272_000, output: 128_000 },
      variants: {
        none: { reasoningEffort: 'none' },
        medium: { reasoningEffort: 'medium' },
        xhigh: { reasoningEffort: 'xhigh' },
        max: { reasoningEffort: 'max' },
      },
    })
    expect(json.provider.stravia.options).toEqual({
      name: 'stravia',
      url: 'http://localhost:5174/v1/responses',
      apiKey: 'sk-client',
    })
  })

  test('writes every authorized model to the OMP Responses provider', () => {
    const config = buildCliConfig({
      tool: 'omp',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-sol',
    })

    expect(config).toStartWith('# ~/.omp/agent/models.yml')
    expect(config).toContain('baseUrl: "http://localhost:5174/v1"')
    expect(config).toContain('api: openai-responses')
    expect(config).toContain('authHeader: true')
    expect(config).toContain('efforts: ["minimal","low","medium","high","xhigh","max"]')
    expect(config).toContain('default: "stravia/gpt-5.6-sol"')
    for (const model of modelNames) expect(config).toContain(`- id: "${model}"`)
  })

  test('writes every authorized model to the Pi Responses provider', () => {
    const config = buildCliConfig({
      tool: 'pi',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-luna',
    })
    const [modelsJson, settingsJson] = config.split('\n\n# Merge into ~/.pi/agent/settings.json\n')
    const providerConfig = JSON.parse(modelsJson.split('\n').slice(1).join('\n'))
    const settings = JSON.parse(settingsJson)

    expect(providerConfig.providers.stravia).toMatchObject({
      baseUrl: 'http://localhost:5174/v1',
      apiKey: 'sk-client',
      api: 'openai-responses',
      authHeader: true,
    })
    expect(providerConfig.providers.stravia.models.map((model: { id: string }) => model.id)).toEqual(modelNames)
    expect(providerConfig.providers.stravia.models[1].thinkingLevelMap).toMatchObject({
      off: 'none',
      minimal: 'minimal',
      medium: 'medium',
      xhigh: 'xhigh',
      max: 'max',
    })
    expect(settings).toEqual({ defaultProvider: 'stravia', defaultModel: 'gpt-5.6-luna' })
  })

  test('rejects defaults and mappings outside the API Key scope', () => {
    expect(() =>
      buildCliConfig({
        tool: 'codex-cli',
        host: 'http://localhost:5174',
        apiKey: 'sk-client',
        models,
        defaultModel: 'not-authorized',
      }),
    ).toThrow('default model must be available to the API key')

    expect(() =>
      buildCliConfig({
        tool: 'claude-code',
        host: 'http://localhost:5174',
        apiKey: 'sk-client',
        models,
        mappings: {
          defaultModel: 'claude-opus',
          haikuModel: 'not-authorized',
          sonnetModel: 'gpt-5.6-sol',
          opusModel: 'claude-opus',
        },
      }),
    ).toThrow('Claude model mappings must use models available to the API key')
  })

  test('uses the route-level model capabilities returned by the backend', () => {
    const model = {
      id: 'route',
      name: 'shared-model',
      balance: 'weighted',
      target_provider: 'provider-a',
      target_model: 'upstream-a',
      is_enabled: true,
      created_at: '2026-08-05T00:00:00Z',
      supported_thinking_levels: ['off', 'low', 'high'],
      context_window: 128_000,
      output_max_tokens: 32_000,
      targets: [],
    } satisfies Model

    expect(defineClientModel(model)).toEqual({
      name: 'shared-model',
      supportedThinkingLevels: ['off', 'low', 'high'],
      contextWindow: 128_000,
      outputMaxTokens: 32_000,
    })
  })
})

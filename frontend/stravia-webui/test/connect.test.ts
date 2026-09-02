import { describe, expect, test } from 'bun:test'

import {
  apiKeyAllowsModel,
  buildCliConfig,
  buildCode,
  CLI_TOOLS,
  defineClientModel,
  maskApiKey,
  type ClientModelDefinition,
} from '../src/lib/connect'
import type { Route } from '../src/lib/types'

const models: ClientModelDefinition[] = [
  {
    modelId: 'claude-opus',
    displayName: 'Claude Opus',
    supportedThinkingLevels: ['off', 'high', 'max'],
    supportsImageInput: false,
    contextWindow: 200_000,
    outputMaxTokens: 32_000,
  },
  {
    modelId: 'gpt-5.6-sol',
    displayName: 'GPT 5.6 Sol',
    supportedThinkingLevels: ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'],
    supportsImageInput: true,
    contextWindow: 272_000,
    outputMaxTokens: 128_000,
  },
  {
    modelId: 'gpt-5.6-luna',
    displayName: 'GPT 5.6 Luna',
    supportedThinkingLevels: ['off', 'low', 'medium', 'high'],
    supportsImageInput: false,
    contextWindow: 196_000,
    outputMaxTokens: 64_000,
  },
]
const modelIds = models.map((model) => model.modelId)

describe('client configuration generation', () => {
  test('treats an empty API Key model scope as unrestricted', () => {
    expect(apiKeyAllowsModel([], 'model-id')).toBe(true)
    expect(apiKeyAllowsModel(['other-model-id'], 'model-id')).toBe(false)
  })

  test('masks API Keys consistently without exposing most of the prefix', () => {
    expect(maskApiKey('sk-d787f8575abcdef4482')).toBe('sk-d78••••••••4482')
    expect(maskApiKey('sk-short-key')).toBe('••••••••••••')
  })

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
          modelId: 'small-model',
          displayName: 'small-model',
          supportedThinkingLevels: ['off', 'minimal', 'max'],
          supportsImageInput: false,
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
      transparentImageInputEnabled: false,
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
    const codexCatalog = JSON.parse(config.split('# ~/.codex/stravia-models.json\n')[1]) as {
      models: Array<{ slug: string; display_name: string; input_modalities: string[] }>
    }
    expect(codexCatalog.models.map((model) => model.input_modalities)).toEqual([['text'], ['text', 'image'], ['text']])
    for (const model of modelIds) expect(config).toContain(`"slug": "${model}"`)
    expect(codexCatalog.models[1]).toMatchObject({ slug: 'gpt-5.6-sol', display_name: 'GPT 5.6 Sol' })
  })

  test('writes every authorized model to the OpenCode provider', () => {
    const config = buildCliConfig({
      tool: 'opencode',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-luna',
      transparentImageInputEnabled: false,
    })

    const json = JSON.parse(config.split('\n').slice(1).join('\n'))
    expect(Object.keys(json)).toEqual(['model', 'provider'])
    expect(Object.keys(json.provider.stravia)).toEqual(['npm', 'models', 'options'])
    expect(json.provider.stravia.npm).toBe('@ai-sdk/open-responses')
    expect(json.model).toBe('stravia/gpt-5.6-luna')
    expect(Object.keys(json.provider.stravia.models)).toEqual(modelIds)
    expect(Object.keys(json.provider.stravia.models['gpt-5.6-sol'])).toEqual([
      'reasoning',
      'variants',
      'limit',
      'modalities',
    ])
    expect(json.provider.stravia.models['gpt-5.6-sol']).toMatchObject({
      reasoning: true,
      limit: { context: 272_000, output: 128_000 },
      modalities: { input: ['text', 'image'], output: ['text'] },
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
      transparentImageInputEnabled: false,
    })

    expect(config).toStartWith('# ~/.omp/agent/models.yml')
    expect(config).toContain('baseUrl: "http://localhost:5174/v1"')
    expect(config).toContain('api: openai-responses')
    expect(config).toContain('authHeader: true')
    expect(config).toContain('efforts: ["minimal","low","medium","high","xhigh","max"]')
    expect(config).toContain('input: ["text","image"]')
    expect(config).toContain('input: ["text"]')
    expect(config).toContain('default: "stravia/gpt-5.6-sol"')
    for (const model of modelIds) expect(config).toContain(`- id: "${model}"`)
    expect(config).toContain('- id: "gpt-5.6-sol"\n        name: "GPT 5.6 Sol"')
  })

  test('writes every authorized model to the Pi Responses provider', () => {
    const config = buildCliConfig({
      tool: 'pi',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-luna',
      transparentImageInputEnabled: false,
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
    expect(providerConfig.providers.stravia.models.map((model: { id: string }) => model.id)).toEqual(modelIds)
    expect(providerConfig.providers.stravia.models[1].name).toBe('GPT 5.6 Sol')
    expect(providerConfig.providers.stravia.models[1].thinkingLevelMap).toMatchObject({
      off: 'none',
      minimal: 'minimal',
      medium: 'medium',
      xhigh: 'xhigh',
      max: 'max',
    })
    expect(providerConfig.providers.stravia.models.map((model: { input: string[] }) => model.input)).toEqual([
      ['text'],
      ['text', 'image'],
      ['text'],
    ])
    expect(settings).toEqual({ defaultProvider: 'stravia', defaultModel: 'gpt-5.6-luna' })
  })

  test('lists the requested clients in product order', () => {
    expect(CLI_TOOLS.slice(0, 10).map((tool) => tool.name)).toEqual([
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
    ])
  })

  test('writes an OpenClaw provider and primary model', () => {
    const config = buildCliConfig({
      tool: 'openclaw',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-sol',
      transparentImageInputEnabled: false,
    })
    const json = JSON.parse(config.split('\n').slice(1).join('\n'))

    expect(json.models.providers.stravia).toMatchObject({
      baseUrl: 'http://localhost:5174/v1',
      apiKey: 'sk-client',
      api: 'openai-completions',
    })
    expect(json.models.providers.stravia.models.map((model: { id: string }) => model.id)).toEqual(modelIds)
    expect(json.models.providers.stravia.models[1].name).toBe('GPT 5.6 Sol')
    expect(json.models.providers.stravia.models.map((model: { input: string[] }) => model.input)).toEqual([
      ['text'],
      ['text', 'image'],
      ['text'],
    ])
    expect(json.agents.defaults.model.primary).toBe('stravia/gpt-5.6-sol')
  })

  test('writes modern Hermes named-provider configuration', () => {
    const config = buildCliConfig({
      tool: 'hermes-agent',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-luna',
      transparentImageInputEnabled: false,
    })

    expect(config).toContain('STRAVIA_API_KEY=sk-client')
    expect(config).toContain('key_env: STRAVIA_API_KEY')
    expect(config).toContain('transport: chat_completions')
    expect(config).toContain('default: "gpt-5.6-luna"')
    expect(config).toContain('"gpt-5.6-sol":\n        context_length: 272000\n        supports_vision: true')
    expect(config).toContain('"gpt-5.6-luna":\n        context_length: 196000\n        supports_vision: false')
    for (const model of modelIds) expect(config).toContain(`"${model}":`)
  })

  test('writes a TRAE OpenAI-compatible model binding', () => {
    const config = buildCliConfig({
      tool: 'trae',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-sol',
      transparentImageInputEnabled: false,
    })

    expect(config).toContain('provider: openai')
    expect(config).toContain('enable_lakeview: false')
    expect(config).toContain('base_url: "http://localhost:5174/v1"')
    expect(config).toContain('model_provider: stravia')
    expect(config).toContain('model: "gpt-5.6-sol"')
    expect(config).toContain('max_tokens: 128000')
    expect(config).toContain('temperature: 0.5')
    expect(config).toContain('top_p: 1')
    expect(config).toContain('top_k: 0')
    expect(config).toContain('max_retries: 10')
    expect(config).toContain('parallel_tool_calls: true')
  })

  test('writes WorkBuddy models.json and a complete ZCode provider entry', () => {
    const common = { host: 'http://localhost:5174', apiKey: 'sk-client', models, defaultModel: 'gpt-5.6-sol' }
    const workbuddy = buildCliConfig({ tool: 'workbuddy', ...common, transparentImageInputEnabled: true })
    const zcode = buildCliConfig({ tool: 'zcode', ...common, transparentImageInputEnabled: true })

    const workbuddyModels = JSON.parse(workbuddy.split('\n').slice(1).join('\n'))
    expect(workbuddy).toStartWith('# ~/.workbuddy/models.json')
    expect(workbuddyModels.map((model: { id: string }) => model.id)).toEqual(modelIds)
    expect(workbuddyModels[1]).toEqual({
      id: 'gpt-5.6-sol',
      name: 'GPT 5.6 Sol',
      vendor: 'Custom',
      url: 'http://localhost:5174/v1/chat/completions',
      apiKey: 'sk-client',
      maxInputTokens: 272000,
      maxOutputTokens: 128000,
      supportsToolCall: true,
      supportsImages: true,
      supportsReasoning: true,
      useCustomProtocol: false,
      reasoning: { supportedEfforts: ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'] },
    })

    const zcodeJson = zcode.slice(zcode.indexOf('{'), zcode.lastIndexOf('}') + 1)
    const zcodeDocument = JSON.parse(zcodeJson) as {
      provider: Record<
        string,
        {
          name: string
          kind: string
          options: { apiKey: string; baseURL: string; apiKeyRequired: boolean }
          source: string
          models: Record<
            string,
            {
              limit: { context: number; output: number }
              modalities: { input: string[]; output: string[] }
              zcode: { modalitiesConfigured: boolean; modified: boolean }
            }
          >
        }
      >
    }
    const [zcodeProvider] = Object.values(zcodeDocument.provider)
    expect(zcode).toStartWith('# Exit ZCode before editing its configuration file.')
    expect(Object.keys(zcodeDocument.provider)).toEqual(['custom:stravia'])
    expect(zcodeProvider).toBeDefined()
    expect(zcodeProvider?.name).toBe('Stravia')
    expect(zcodeProvider?.kind).toBe('openai-compatible')
    expect(zcodeProvider?.options).toEqual({
      apiKey: 'sk-client',
      baseURL: 'http://localhost:5174/v1',
      apiKeyRequired: true,
    })
    expect(zcodeProvider?.source).toBe('custom')
    expect(Object.keys(zcodeProvider?.models ?? {})).toEqual(modelIds)
    expect(zcodeProvider?.models['gpt-5.6-sol']).toEqual({
      limit: { context: 272000, output: 128000 },
      modalities: { input: ['text', 'image'], output: ['text'] },
      zcode: { modalitiesConfigured: true, modified: true },
    })
    expect(zcode).toContain('ZCode rewrites this file at startup and does not preserve custom per-level request mappings.')
    expect(zcode).toContain('Reasoning controls are available only when ZCode recognizes the model itself.')
    expect(zcode).toContain('# Restart ZCode, then select gpt-5.6-sol as the default model.')
  })

  test('does not emit custom ZCode reasoning mappings that startup removes', () => {
    const zcode = buildCliConfig({
      tool: 'zcode',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models: [
        {
          modelId: 'grok-4.6',
          displayName: 'Grok 4.6',
          supportedThinkingLevels: ['low', 'medium', 'high', 'xhigh'],
          supportsImageInput: true,
        },
      ],
      defaultModel: 'grok-4.6',
      transparentImageInputEnabled: false,
    })
    const document = JSON.parse(zcode.slice(zcode.indexOf('{'), zcode.lastIndexOf('}') + 1)) as {
      provider: Record<string, { models: Record<string, Record<string, unknown>> }>
    }
    const [provider] = Object.values(document.provider)

    expect(provider?.models['grok-4.6']?.reasoning).toBeUndefined()
    expect(provider?.models['grok-4.6']?.variants).toBeUndefined()
    expect(zcode).not.toContain('providerOptionsByLevel')
    expect(zcode).not.toContain('reasoningEffort')
  })

  test('falls back to model image capabilities when WorkBuddy transparent media understanding is disabled', () => {
    const workbuddy = buildCliConfig({
      tool: 'workbuddy',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-sol',
      transparentImageInputEnabled: false,
    })
    const workbuddyModels = JSON.parse(workbuddy.split('\n').slice(1).join('\n')) as Array<{ supportsImages: boolean }>

    expect(workbuddyModels.map((model) => model.supportsImages)).toEqual([false, true, false])
  })

  test('falls back to model image capabilities when ZCode transparent media understanding is disabled', () => {
    const zcode = buildCliConfig({
      tool: 'zcode',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-sol',
      transparentImageInputEnabled: false,
    })
    const zcodeDocument = JSON.parse(zcode.slice(zcode.indexOf('{'), zcode.lastIndexOf('}') + 1)) as {
      provider: Record<string, { models: Record<string, { modalities: { input: string[] } }> }>
    }
    const [zcodeProvider] = Object.values(zcodeDocument.provider)

    expect(Object.values(zcodeProvider?.models ?? {}).map((model) => model.modalities.input)).toEqual([
      ['text'],
      ['text', 'image'],
      ['text'],
    ])
  })

  test('writes a DeepSeek Harness custom provider', () => {
    const config = buildCliConfig({
      tool: 'deepseek-harness',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models,
      defaultModel: 'gpt-5.6-sol',
      transparentImageInputEnabled: false,
    })

    expect(config).toContain('apiKeyEnv: STRAVIA_API_KEY')
    expect(config).toContain('api: openai-completions')
    expect(config).toContain('baseURL: "http://localhost:5174/v1"')
    expect(config).toContain('- id: "gpt-5.6-sol"')
    expect(config).toContain('contextWindow: 272000')
    expect(config).toContain('maxTokens: 128000')
    expect(config).toContain('input: ["text","image"]')
    expect(config).toContain('input: ["text"]')
  })

  test('rejects defaults and mappings outside the API Key scope', () => {
    expect(() =>
      buildCliConfig({
        tool: 'codex-cli',
        host: 'http://localhost:5174',
        apiKey: 'sk-client',
        models,
        defaultModel: 'not-authorized',
        transparentImageInputEnabled: false,
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

    expect(defineClientModel(model)).toEqual({
      modelId: 'shared-model',
      displayName: 'Shared Model',
      supportedThinkingLevels: ['off', 'low', 'high'],
      supportsImageInput: true,
      contextWindow: 128_000,
      outputMaxTokens: 32_000,
    })
  })

  test('falls back to Model ID when a Route has no display name', () => {
    const model = {
      id: 'unnamed-route',
      model_id: 'custom/unnamed-model',
      display_name: null,
      balance: 'weighted',
      target_provider: 'provider-a',
      target_model: 'upstream-a',
      is_enabled: true,
      created_at: '2026-08-05T00:00:00Z',
      supported_thinking_levels: [],
      targets: [],
    } satisfies Route

    const definition = defineClientModel(model)
    expect(definition.displayName).toBe('custom/unnamed-model')
    const config = buildCliConfig({
      tool: 'omp',
      host: 'http://localhost:5174',
      apiKey: 'sk-client',
      models: [definition],
      defaultModel: definition.modelId,
      transparentImageInputEnabled: false,
    })
    expect(config).toContain('- id: "custom/unnamed-model"\n        name: "custom/unnamed-model"')
  })
})

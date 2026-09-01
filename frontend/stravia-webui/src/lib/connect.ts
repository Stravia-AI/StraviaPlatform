import * as m from '$lib/paraglide/messages.js'
import type { Route, ThinkingLevel } from '$lib/types'

export type CodeLanguage = 'python' | 'typescript' | 'curl'
export type GatewayProtocol = 'openai-compatible' | 'open-responses' | 'anthropic-messages' | 'google-gemini'
export type CliToolId =
  | 'codex-cli'
  | 'claude-code'
  | 'opencode'
  | 'openclaw'
  | 'hermes-agent'
  | 'trae'
  | 'workbuddy'
  | 'zcode'
  | 'deepseek-harness'
  | 'pi'
  | 'omp'

export interface ClaudeModelMappings {
  defaultModel: string
  haikuModel: string
  sonnetModel: string
  opusModel: string
}

export interface ClientModelDefinition {
  name: string
  supportedThinkingLevels: readonly ThinkingLevel[]
  contextWindow?: number
  outputMaxTokens?: number
}

type CliConfigParams =
  | {
      tool: 'claude-code'
      host: string
      apiKey: string
      models: readonly ClientModelDefinition[]
      mappings: ClaudeModelMappings
    }
  | {
      tool: 'workbuddy' | 'zcode'
      host: string
      apiKey: string
      models: readonly ClientModelDefinition[]
      defaultModel: string
      imageInputEnabled: boolean
    }
  | {
      tool: Exclude<CliToolId, 'claude-code' | 'workbuddy' | 'zcode'>
      host: string
      apiKey: string
      models: readonly ClientModelDefinition[]
      defaultModel: string
    }

export const CLI_TOOLS: ReadonlyArray<{
  id: CliToolId
  name: string
  protocol: GatewayProtocol
  description: () => string
}> = [
  { id: 'codex-cli', name: 'Codex', protocol: 'open-responses', description: m.connect_tool_codex_description },
  {
    id: 'claude-code',
    name: 'Claude Code',
    protocol: 'anthropic-messages',
    description: m.connect_tool_claude_description,
  },
  { id: 'opencode', name: 'OpenCode', protocol: 'open-responses', description: m.connect_tool_opencode_description },
  { id: 'openclaw', name: 'OpenClaw', protocol: 'openai-compatible', description: m.connect_tool_openclaw_description },
  {
    id: 'hermes-agent',
    name: 'Hermes Agent',
    protocol: 'openai-compatible',
    description: m.connect_tool_hermes_description,
  },
  { id: 'trae', name: 'TRAE', protocol: 'openai-compatible', description: m.connect_tool_trae_description },
  {
    id: 'workbuddy',
    name: 'WorkBuddy',
    protocol: 'openai-compatible',
    description: m.connect_tool_workbuddy_description,
  },
  { id: 'zcode', name: 'ZCode', protocol: 'openai-compatible', description: m.connect_tool_zcode_description },
  {
    id: 'deepseek-harness',
    name: 'DeepSeek Harness',
    protocol: 'openai-compatible',
    description: m.connect_tool_deepseek_harness_description,
  },
  { id: 'pi', name: 'Pi', protocol: 'open-responses', description: m.connect_tool_pi_description },
  { id: 'omp', name: 'OMP', protocol: 'open-responses', description: m.connect_tool_omp_description },
]

const thinkingLevelDescriptions: Record<ThinkingLevel, string> = {
  off: 'No reasoning',
  minimal: 'Minimal reasoning effort',
  low: 'Low reasoning effort',
  medium: 'Medium reasoning effort',
  high: 'High reasoning effort',
  xhigh: 'Extra high reasoning effort',
  max: 'Maximum reasoning effort',
}

const zcodeStraviaProviderId = 'custom:stravia'

function reasoningEffort(level: ThinkingLevel): string {
  return level === 'off' ? 'none' : level
}

function reasoningLevels(model: ClientModelDefinition): ThinkingLevel[] {
  return model.supportedThinkingLevels.filter((level) => level !== 'off')
}

function supportsReasoning(model: ClientModelDefinition): boolean {
  return reasoningLevels(model).length > 0
}

function claudeEffortLevel(model: ClientModelDefinition): 'low' | 'medium' | 'high' | 'xhigh' | undefined {
  return (['medium', 'high', 'low', 'xhigh'] as const).find((level) =>
    model.supportedThinkingLevels.includes(level),
  )
}

function claudeAutoCompactWindow(model: ClientModelDefinition): number | undefined {
  if (model.contextWindow === undefined || model.contextWindow < 100_000) return undefined
  return Math.min(model.contextWindow, 1_000_000)
}

export function defineClientModel(model: Route): ClientModelDefinition {
  return {
    name: model.name,
    supportedThinkingLevels: [...new Set(model.supported_thinking_levels ?? [])],
    ...(model.context_window ? { contextWindow: model.context_window } : {}),
    ...(model.output_max_tokens ? { outputMaxTokens: model.output_max_tokens } : {}),
  }
}

export function apiKeyAllowsModel(modelIds: readonly string[], modelId: string): boolean {
  return modelIds.length === 0 || modelIds.includes(modelId)
}

export function maskApiKey(key: string): string {
  return key.length <= 14 ? '••••••••••••' : `${key.slice(0, 6)}••••••••${key.slice(-4)}`
}

export function protocolLabel(protocol: GatewayProtocol): string {
  if (protocol === 'openai-compatible') return 'OpenAI Compatible'
  if (protocol === 'open-responses') return 'Open Responses'
  if (protocol === 'anthropic-messages') return 'Anthropic Messages'
  return 'Google Gemini'
}

export function buildCode(params: {
  protocol: GatewayProtocol
  model: string
  apiKey?: string
  host: string
  language: CodeLanguage
}): string {
  const { protocol, model, apiKey, host, language } = params
  const clientApiKey = apiKey ?? 'not-required'
  if (language === 'curl') {
    if (protocol === 'open-responses') {
      const authHeader = apiKey ? `  -H "Authorization: Bearer ${apiKey}" \\\n` : ''
      return `curl ${host}/v1/responses \\
${authHeader}  -H "Content-Type: application/json" \\
  -d '{"model":"${model}","input":"Hello"}'`
    }
    if (protocol === 'anthropic-messages') {
      const authHeader = apiKey ? `  -H "x-api-key: ${apiKey}" \\\n` : ''
      return `curl ${host}/v1/messages \\
${authHeader}  -H "anthropic-version: 2023-06-01" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"${model}","max_tokens":1024,"messages":[{"role":"user","content":"Hello"}]}'`
    }
    if (protocol === 'google-gemini') {
      const authHeader = apiKey ? `  -H "x-goog-api-key: ${apiKey}" \\\n` : ''
      return `curl ${host}/v1beta/models/${encodeURIComponent(model).replace(/%3A/gi, ':')}:generateContent \\
${authHeader}  -H "Content-Type: application/json" \\
  -d '{"contents":[{"role":"user","parts":[{"text":"Hello"}]}]}'`
    }
    const authHeader = apiKey ? `  -H "Authorization: Bearer ${apiKey}" \\\n` : ''
    return `curl ${host}/v1/chat/completions \\
${authHeader}  -H "Content-Type: application/json" \\
  -d '{"model":"${model}","messages":[{"role":"user","content":"Hello"}]}'`
  }
  if (language === 'python') {
    if (protocol === 'open-responses')
      return `# pip install openai
from openai import OpenAI

client = OpenAI(api_key="${clientApiKey}", base_url="${host}/v1")
response = client.responses.create(model="${model}", input="Hello")
print(response.output_text)`
    if (protocol === 'anthropic-messages')
      return `# pip install anthropic
from anthropic import Anthropic

client = Anthropic(api_key="${clientApiKey}", base_url="${host}")
response = client.messages.create(model="${model}", max_tokens=1024, messages=[{"role": "user", "content": "Hello"}])
print(response.content[0].text)`
    if (protocol === 'google-gemini')
      return `# pip install google-genai
from google import genai

client = genai.Client(api_key="${clientApiKey}", http_options={"base_url": "${host}"})
response = client.models.generate_content(model="${model}", contents="Hello")
print(response.text)`
    return `# pip install openai
from openai import OpenAI

client = OpenAI(api_key="${clientApiKey}", base_url="${host}/v1")
response = client.chat.completions.create(model="${model}", messages=[{"role": "user", "content": "Hello"}])
print(response.choices[0].message.content)`
  }
  if (protocol === 'open-responses')
    return `// npm install openai
import OpenAI from "openai";

const client = new OpenAI({ apiKey: "${clientApiKey}", baseURL: "${host}/v1" });
const response = await client.responses.create({ model: "${model}", input: "Hello" });
return response.output_text;`
  if (protocol === 'anthropic-messages')
    return `// npm install @anthropic-ai/sdk
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic({ apiKey: "${clientApiKey}", baseURL: "${host}" });
const response = await client.messages.create({ model: "${model}", max_tokens: 1024, messages: [{ role: "user", content: "Hello" }] });
return response.content[0];`
  if (protocol === 'google-gemini')
    return `// npm install @google/genai
import { GoogleGenAI } from "@google/genai";

const client = new GoogleGenAI({ apiKey: "${clientApiKey}", baseUrl: "${host}" });
const response = await client.models.generateContent({ model: "${model}", contents: "Hello" });
return response.text;`
  return `// npm install openai
import OpenAI from "openai";

const client = new OpenAI({ apiKey: "${clientApiKey}", baseURL: "${host}/v1" });
const response = await client.chat.completions.create({ model: "${model}", messages: [{ role: "user", content: "Hello" }] });
return response.choices[0]?.message?.content;`
}

export function buildCliConfig(params: CliConfigParams): string {
  const models = [...new Map(params.models.map((model) => [model.name, model])).values()]
  if (models.length === 0) throw new Error('client configuration requires at least one model')
  const modelNames = models.map((model) => model.name)

  if (params.tool === 'claude-code') {
    const mappedModels = Object.values(params.mappings)
    if (mappedModels.some((model) => !modelNames.includes(model)))
      throw new Error('Claude model mappings must use models available to the API key')
    const defaultModel = models.find((model) => model.name === params.mappings.defaultModel)!
    const effortLevel = claudeEffortLevel(defaultModel)
    const autoCompactWindow = claudeAutoCompactWindow(defaultModel)

    return `# ~/.claude/settings.json
${JSON.stringify(
  {
    env: {
      ANTHROPIC_AUTH_TOKEN: params.apiKey,
      ANTHROPIC_BASE_URL: params.host,
      ANTHROPIC_MODEL: params.mappings.defaultModel,
      ANTHROPIC_DEFAULT_HAIKU_MODEL: params.mappings.haikuModel,
      ANTHROPIC_DEFAULT_SONNET_MODEL: params.mappings.sonnetModel,
      ANTHROPIC_DEFAULT_OPUS_MODEL: params.mappings.opusModel,
    },
    ...(effortLevel === undefined ? {} : { effortLevel }),
    ...(autoCompactWindow === undefined ? {} : { autoCompactWindow }),
  },
  null,
  2,
)}`
  }

  if (!modelNames.includes(params.defaultModel))
    throw new Error('default model must be available to the API key')

  if (params.tool === 'codex-cli') {
    const modelCatalog = {
      models: models.map((model, index) => ({
        slug: model.name,
        display_name: model.name,
        description: null,
        default_reasoning_level:
          model.supportedThinkingLevels.includes('medium')
            ? 'medium'
            : model.supportedThinkingLevels[0] === undefined
              ? undefined
              : reasoningEffort(model.supportedThinkingLevels[0]),
        supported_reasoning_levels: model.supportedThinkingLevels.map((level) => ({
          effort: reasoningEffort(level),
          description: thinkingLevelDescriptions[level],
        })),
        shell_type: 'unified_exec',
        visibility: 'list',
        supported_in_api: true,
        priority: index,
        availability_nux: null,
        upgrade: null,
        base_instructions:
          "You are a coding agent running in Codex. Follow the user's instructions and use the available tools to complete the task.",
        support_verbosity: false,
        default_verbosity: null,
        apply_patch_tool_type: null,
        truncation_policy: { mode: 'bytes', limit: 10_000 },
        supports_parallel_tool_calls: false,
        experimental_supported_tools: [],
        context_window: model.contextWindow,
      })),
    }

    return `# Set STRAVIA_API_KEY to the selected API Key before starting Codex.
# Value: ${params.apiKey}

# ~/.codex/config.toml
model_provider = "stravia"
model = ${JSON.stringify(params.defaultModel)}
model_catalog_json = "stravia-models.json"

[model_providers.stravia]
name = "Stravia Gateway"
base_url = ${JSON.stringify(`${params.host}/v1`)}
wire_api = "responses"
env_key = "STRAVIA_API_KEY"

# ~/.codex/stravia-models.json
${JSON.stringify(modelCatalog, null, 2)}`
  }

  if (params.tool === 'omp') {
    const modelEntries = models.flatMap((model) => {
      const levels = reasoningLevels(model)
      return [
        `      - id: ${JSON.stringify(model.name)}`,
        `        name: ${JSON.stringify(model.name)}`,
        `        reasoning: ${supportsReasoning(model)}`,
        ...(levels.length === 0
          ? []
          : [
              '        thinking:',
              '          mode: effort',
              `          efforts: ${JSON.stringify(levels)}`,
              ...(levels.includes('medium') ? ['          defaultLevel: medium'] : []),
            ]),
        ...(model.contextWindow === undefined ? [] : [`        contextWindow: ${model.contextWindow}`]),
        ...(model.outputMaxTokens === undefined ? [] : [`        maxTokens: ${model.outputMaxTokens}`]),
      ]
    })

    return `# ~/.omp/agent/models.yml
providers:
  stravia:
    baseUrl: ${JSON.stringify(`${params.host}/v1`)}
    apiKey: ${JSON.stringify(params.apiKey)}
    api: openai-responses
    authHeader: true
    models:
${modelEntries.join('\n')}

# Merge into ~/.omp/agent/config.yml
modelRoles:
  default: ${JSON.stringify(`stravia/${params.defaultModel}`)}`
  }

  if (params.tool === 'pi') {
    const piModels = models.map((model) => {
      const levels = reasoningLevels(model)
      const thinkingLevelMap =
        levels.length === 0
          ? undefined
          : Object.fromEntries(
              (['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const).map((level) => [
                level,
                model.supportedThinkingLevels.includes(level) ? reasoningEffort(level) : null,
              ]),
            )
      return {
        id: model.name,
        name: model.name,
        reasoning: supportsReasoning(model),
        ...(thinkingLevelMap === undefined ? {} : { thinkingLevelMap }),
        ...(model.contextWindow === undefined ? {} : { contextWindow: model.contextWindow }),
        ...(model.outputMaxTokens === undefined ? {} : { maxTokens: model.outputMaxTokens }),
      }
    })

    return `# ~/.pi/agent/models.json
${JSON.stringify(
  {
    providers: {
      stravia: {
        baseUrl: `${params.host}/v1`,
        apiKey: params.apiKey,
        api: 'openai-responses',
        authHeader: true,
        models: piModels,
      },
    },
  },
  null,
  2,
)}

# Merge into ~/.pi/agent/settings.json
${JSON.stringify({ defaultProvider: 'stravia', defaultModel: params.defaultModel }, null, 2)}`
  }

  if (params.tool === 'openclaw') {
    return `# ~/.openclaw/openclaw.json
${JSON.stringify(
  {
    models: {
      providers: {
        stravia: {
          baseUrl: `${params.host}/v1`,
          apiKey: params.apiKey,
          api: 'openai-completions',
          models: models.map((model) => ({
            id: model.name,
            name: model.name,
            ...(model.contextWindow === undefined ? {} : { contextWindow: model.contextWindow }),
            ...(model.outputMaxTokens === undefined ? {} : { maxTokens: model.outputMaxTokens }),
          })),
        },
      },
    },
    agents: { defaults: { model: { primary: `stravia/${params.defaultModel}` } } },
  },
  null,
  2,
)}`
  }

  if (params.tool === 'hermes-agent') {
    const modelEntries = models.flatMap((model) => [
      `      ${JSON.stringify(model.name)}:`,
      ...(model.contextWindow === undefined ? [] : [`        context_length: ${model.contextWindow}`]),
    ])
    return `# ~/.hermes/.env
STRAVIA_API_KEY=${params.apiKey}

# ~/.hermes/config.yaml
providers:
  stravia:
    api: ${JSON.stringify(`${params.host}/v1`)}
    key_env: STRAVIA_API_KEY
    transport: chat_completions
    default_model: ${JSON.stringify(params.defaultModel)}
    discover_models: false
    models:
${modelEntries.join('\n')}
model:
  provider: stravia
  default: ${JSON.stringify(params.defaultModel)}`
  }

  if (params.tool === 'trae') {
    return `# trae_config.yaml
agents:
  trae_agent:
    enable_lakeview: false
    model: stravia_default
    max_steps: 200
    tools:
      - bash
      - str_replace_based_edit_tool
      - sequentialthinking
      - task_done
model_providers:
  stravia:
    provider: openai
    api_key: ${JSON.stringify(params.apiKey)}
    base_url: ${JSON.stringify(`${params.host}/v1`)}
models:
  stravia_default:
    model_provider: stravia
    model: ${JSON.stringify(params.defaultModel)}
    max_tokens: ${models.find((model) => model.name === params.defaultModel)?.outputMaxTokens ?? 4096}
    temperature: 0.5
    top_p: 1
    top_k: 0
    max_retries: 10
    parallel_tool_calls: true`
  }

  if (params.tool === 'workbuddy') {
    return `# ~/.workbuddy/models.json
${JSON.stringify(
  models.map((model) => {
    const supportedEfforts = reasoningLevels(model)
    return {
      id: model.name,
      name: model.name,
      vendor: 'Custom',
      url: `${params.host}/v1/chat/completions`,
      apiKey: params.apiKey,
      ...(model.contextWindow === undefined ? {} : { maxInputTokens: model.contextWindow }),
      ...(model.outputMaxTokens === undefined ? {} : { maxOutputTokens: model.outputMaxTokens }),
      supportsToolCall: true,
      supportsImages: params.imageInputEnabled,
      supportsReasoning: supportedEfforts.length > 0,
      useCustomProtocol: false,
      ...(supportedEfforts.length === 0 ? {} : { reasoning: { supportedEfforts } }),
    }
  }),
  null,
  2,
)}`
  }

  if (params.tool === 'zcode') {
    const zcodeModels = Object.fromEntries(
      models.map((model) => {
        const variants = [...new Set(model.supportedThinkingLevels)]
        const defaultVariant = variants.includes('medium')
          ? 'medium'
          : variants.find((variant) => variant !== 'off')
        const limit = {
          ...(model.contextWindow === undefined ? {} : { context: model.contextWindow }),
          ...(model.outputMaxTokens === undefined ? {} : { output: model.outputMaxTokens }),
        }

        return [
          model.name,
          {
            ...(defaultVariant === undefined
              ? {}
              : {
                  reasoning: {
                    enabled: true,
                    variants,
                    defaultVariant,
                  },
                }),
            ...(Object.keys(limit).length === 0 ? {} : { limit }),
            modalities: {
              input: params.imageInputEnabled ? ['text', 'image'] : ['text'],
              output: ['text'],
            },
            zcode: {
              modalitiesConfigured: true,
              modified: true,
            },
          },
        ]
      }),
    )

    return `# Exit ZCode before editing its configuration file.
# Windows: %USERPROFILE%\\.zcode\\v2\\config.json
# macOS/Linux: ~/.zcode/v2/config.json
# Merge the provider entry below into the top-level "provider" object; keep every existing provider.
${JSON.stringify(
  {
    provider: {
      [zcodeStraviaProviderId]: {
        name: 'Stravia',
        kind: 'openai-compatible',
        options: {
          apiKey: params.apiKey,
          baseURL: `${params.host}/v1`,
          apiKeyRequired: true,
        },
        source: 'custom',
        models: zcodeModels,
      },
    },
  },
  null,
  2,
)}

# Restart ZCode, then select ${params.defaultModel} as the default model.`
  }

  if (params.tool === 'deepseek-harness') {
    const modelEntries = models.flatMap((model) => [
      `        - id: ${JSON.stringify(model.name)}`,
      ...(model.contextWindow === undefined ? [] : [`          contextWindow: ${model.contextWindow}`]),
      ...(model.outputMaxTokens === undefined ? [] : [`          maxTokens: ${model.outputMaxTokens}`]),
    ])
    return `# Set STRAVIA_API_KEY before starting dsh.
# Value: ${params.apiKey}

# $DSH_HOME/settings.yaml
llm-pi-ai:
  providers:
    stravia:
      displayName: Stravia Gateway
      apiKeyEnv: STRAVIA_API_KEY
      api: openai-completions
      baseURL: ${JSON.stringify(`${params.host}/v1`)}
      models:
${modelEntries.join('\n')}

# In Settings > Models, select stravia/${params.defaultModel}.
# DeepSeek Harness persists that selection as the default for new sessions.`
  }

  const opencodeModels = Object.fromEntries(
    models.map((model) => {
      const variants = Object.fromEntries(
        model.supportedThinkingLevels.map((level) => {
          const effort = reasoningEffort(level)
          return [effort, { reasoningEffort: effort }]
        }),
      )
      const limit =
        model.contextWindow !== undefined && model.outputMaxTokens !== undefined
          ? { context: model.contextWindow, output: model.outputMaxTokens }
          : undefined
      return [
        model.name,
        {
          reasoning: supportsReasoning(model),
          variants,
          limit,
        },
      ]
    }),
  )
  return `# ~/.config/opencode/opencode.json
${JSON.stringify(
  {
    model: `stravia/${params.defaultModel}`,
    provider: {
      stravia: {
        npm: '@ai-sdk/open-responses',
        models: opencodeModels,
        options: {
          name: 'stravia',
          url: `${params.host}/v1/responses`,
          apiKey: params.apiKey,
        },
      },
    },
  },
  null,
  2,
)}`
}

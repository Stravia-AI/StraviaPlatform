import * as m from '$lib/paraglide/messages.js'
import type { Model, ThinkingLevel } from '$lib/types'

export type CodeLanguage = 'python' | 'typescript' | 'curl'
export type GatewayProtocol = 'openai-compatible' | 'open-responses' | 'anthropic-messages' | 'google-gemini'
export type CliToolId = 'claude-code' | 'codex-cli' | 'opencode' | 'omp' | 'pi'

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
      tool: Exclude<CliToolId, 'claude-code'>
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
  {
    id: 'claude-code',
    name: 'Claude Code',
    protocol: 'anthropic-messages',
    description: m.connect_tool_claude_description,
  },
  { id: 'codex-cli', name: 'Codex', protocol: 'openai-compatible', description: m.connect_tool_codex_description },
  { id: 'opencode', name: 'OpenCode', protocol: 'open-responses', description: m.connect_tool_opencode_description },
  { id: 'omp', name: 'OMP', protocol: 'open-responses', description: m.connect_tool_omp_description },
  { id: 'pi', name: 'Pi', protocol: 'open-responses', description: m.connect_tool_pi_description },
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

export function defineClientModel(model: Model): ClientModelDefinition {
  return {
    name: model.name,
    supportedThinkingLevels: [...new Set(model.supported_thinking_levels ?? [])],
    ...(model.context_window ? { contextWindow: model.context_window } : {}),
    ...(model.output_max_tokens ? { outputMaxTokens: model.output_max_tokens } : {}),
  }
}

export function maskApiKey(key: string): string {
  return key.length <= 14 ? key : `${key.slice(0, 12)}••`
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

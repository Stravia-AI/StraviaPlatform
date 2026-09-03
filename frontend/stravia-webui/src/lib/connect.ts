import * as m from '$lib/paraglide/messages.js'
import { effectiveModelDisplayName } from '$lib/logical-model'
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
  modelId: string
  displayName: string
  supportedThinkingLevels: readonly ThinkingLevel[]
  supportsImageInput: boolean
  contextWindow?: number
  outputMaxTokens?: number
}

export type ConnectClientApplyRequest =
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
      transparentImageInputEnabled: boolean
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

export function defineClientModel(model: Route): ClientModelDefinition {
  return {
    modelId: model.model_id,
    displayName: effectiveModelDisplayName(model),
    supportedThinkingLevels: [...new Set(model.supported_thinking_levels ?? [])],
    supportsImageInput: model.supports_image_input ?? false,
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

import type { ApiKey, Provider, Route, ThinkingLevel } from '$lib/types'

export type ConsoleThinkingSelection = 'default' | ThinkingLevel
export type ConsoleApiKey = Omit<ApiKey, 'key'>
export type ConsoleReadOnlyReason = 'deleted' | 'disabled' | 'expired'
export type ConsoleChatBlocker =
  | 'no-services'
  | 'disabled-services'
  | 'no-models'
  | 'disabled-models'
  | 'no-keys'
  | 'unavailable-keys'

export interface ConsoleChatError {
  message: string
  status?: number
  code?: string
  params?: Record<string, unknown>
}

export interface ConsoleUserMessage {
  id: string
  role: 'user'
  text: string
  createdAt: string
}

export interface ConsoleTokenUsage {
  inputTokens?: number
  outputTokens?: number
}

export interface ConsoleAssistantMessage {
  id: string
  role: 'assistant'
  status: 'completed' | 'incomplete' | 'stopped' | 'failed'
  routeId: string
  thinkingLevel: ConsoleThinkingSelection
  outputItems: unknown[]
  partialText?: string
  partialThinking?: string
  requestReasoning?: ConsoleResponsesRequest['reasoning']
  usage?: ConsoleTokenUsage
  error?: ConsoleChatError
  createdAt: string
}

export type ConsoleMessage = ConsoleUserMessage | ConsoleAssistantMessage

export interface ConsoleConversation {
  id: string
  title: string
  apiKeyId: string
  apiKeyName: string
  selectedModelId: string
  thinkingSelection: ConsoleThinkingSelection
  createdAt: string
  updatedAt: string
  messages: ConsoleMessage[]
}

export interface ConsoleChatPreferences {
  apiKeyId?: string
  modelId?: string
}

export interface ConversationStore {
  load(): Promise<{ conversations: ConsoleConversation[]; preferences: ConsoleChatPreferences }>
  saveConversation(conversation: ConsoleConversation): Promise<void>
  savePreferences(preferences: ConsoleChatPreferences): Promise<void>
  deleteConversation(id: string): Promise<void>
  clearConversations(): Promise<void>
}

export interface ConsoleAdminCatalog {
  read(): Promise<{
    apiKeys: ConsoleApiKey[]
    models: Route[]
    providers: Pick<Provider, 'id' | 'is_enabled'>[]
  }>
  revealKey(id: string): Promise<string>
}

export interface ConsoleResponsesRequest {
  model: string
  stream: true
  input: unknown[]
  reasoning: { summary: 'auto'; effort?: string }
}

export interface ConsoleResponse {
  output?: unknown[]
  status?: string
  model?: string
  usage?: { input_tokens?: number; output_tokens?: number }
  error?: ConsoleChatError | null
  incomplete_details?: { reason?: string } | null
}

export interface ConsoleResponsesEvent {
  type: string
  delta?: string
  response?: ConsoleResponse
  error?: ConsoleChatError
  message?: string
  code?: string
  output_index?: number
  content_index?: number
  summary_index?: number
  item?: unknown
  part?: unknown
  text?: string
}

export interface ResponsesTransport {
  stream(input: {
    apiKey: string
    request: ConsoleResponsesRequest
    signal: AbortSignal
  }): AsyncIterable<ConsoleResponsesEvent>
}

export interface ConsoleGeneration {
  text: string
  summary: string
  reasoning: string
}

export interface ConsoleChatSnapshot {
  conversations: ConsoleConversation[]
  currentConversation: ConsoleConversation | null
  currentConversationId: string | null
  missingConversation: boolean
  loading: boolean
  loadError: unknown
  catalogError: unknown
  storageError: unknown
  keyCandidates: ConsoleApiKey[]
  modelCandidates: Route[]
  selectedKeyId: string | null
  selectedModelId: string | null
  thinkingSelection: ConsoleThinkingSelection
  thinkingLevels: ThinkingLevel[]
  readOnlyReason: ConsoleReadOnlyReason | null
  retryModelAvailable: boolean
  blocker: ConsoleChatBlocker | null
  generations: Record<string, ConsoleGeneration>
}

export interface ConsoleChatControllerOptions {
  store: ConversationStore
  transport: ResponsesTransport
  catalog: ConsoleAdminCatalog
  onSnapshot?: (snapshot: ConsoleChatSnapshot) => void
  now?: () => number
  createId?: () => string
}

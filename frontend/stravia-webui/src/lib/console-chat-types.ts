import type { ApiKey, Provider, Route, ThinkingLevel } from '$lib/types'
import type { ThinkingActivity } from '$lib/observation-activities'

export type ConsoleThinkingSelection = 'default' | ThinkingLevel
export type ConsoleApiKey = Omit<ApiKey, 'key'>
export type ConsoleReadOnlyReason = 'deleted' | 'disabled' | 'expired'
export type ConsoleChatBlocker =
  'no-services' | 'disabled-services' | 'no-models' | 'disabled-models' | 'no-keys' | 'unavailable-keys'

export interface ConsoleChatError {
  message: string
  status?: number
  code?: string
  params?: Record<string, unknown>
}

export interface ConsoleImageAttachment {
  id: string
  name: string
  mediaType: 'image/png' | 'image/jpeg' | 'image/webp'
  dataUrl: string
}

export interface ConsoleUserMessage {
  id: string
  role: 'user'
  text: string
  images?: ConsoleImageAttachment[]
  createdAt: string
}

export interface ConsoleTokenUsage {
  /** 管理面净输入；无法确定总输入或缓存读取时不保存数值。 */
  inputTokens?: number
  outputTokens?: number
  cacheReadTokens?: number
  cacheWriteTokens?: number
}

export interface ConsoleAssistantMessage {
  id: string
  role: 'assistant'
  status: 'completed' | 'incomplete' | 'stopped' | 'failed'
  routeId: string
  thinkingLevel: ConsoleThinkingSelection
  /** 签名项用于后续上游回放，不能按展示文本是否为空过滤。 */
  outputItems: unknown[]
  partialText?: string
  partialThinking?: string
  partialActivities?: ThinkingActivity[]
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
  read(): Promise<{ apiKeys: ConsoleApiKey[]; models: Route[]; providers: Pick<Provider, 'id' | 'is_enabled'>[] }>
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
  usage?: {
    input_tokens?: number | null
    /** `cache_write_tokens` 是 Stravia 扩展字段，上游未报告缓存写入时省略。 */
    input_tokens_details?: { cached_tokens?: number | null; cache_write_tokens?: number | null } | null
    output_tokens?: number | null
  } | null
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
  item_id?: string
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
  /** 与原始推理文本分离，避免签名项或空白文本生成空折叠块。 */
  activities: ThinkingActivity[]
  /** 其后已开始正文或工具输出块的思考 item；聊天只在此时收起自动展开的思考，完成、停止或失败本身不收起。 */
  followedThinkingIds: string[]
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
  inputError: ConsoleChatError | null
  historyHasImages: boolean
  modelSupportsImages: boolean
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

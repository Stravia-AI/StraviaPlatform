import { apiKeyAllowsModel, apiKeyReadOnlyReason } from '$lib/connect'
import type {
  ConsoleApiKey,
  ConsoleAssistantMessage,
  ConsoleChatBlocker,
  ConsoleChatControllerOptions,
  ConsoleChatError,
  ConsoleChatPreferences,
  ConsoleChatSnapshot,
  ConsoleConversation,
  ConsoleGeneration,
  ConsoleMessage,
  ConsoleReadOnlyReason,
  ConsoleResponsesRequest,
  ConsoleThinkingSelection,
} from '$lib/console-chat-types'
import type { Route } from '$lib/types'

/** 隐藏普通 HTML 注释及尚未收完的流式注释；不解释 Core 私有历史语法。 */
export function consoleVisibleText(text: string): string {
  return text.replace(/<!--[\s\S]*?(?:-->|$)/g, '')
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' ? value as Record<string, unknown> : null
}

function readableParts(value: unknown): string {
  if (typeof value === 'string') return value
  if (!Array.isArray(value)) return ''
  return value.map((part) => {
    const item = record(part)
    return typeof item?.text === 'string' ? item.text : typeof item?.refusal === 'string' ? item.refusal : ''
  }).join('')
}

export function consoleAssistantContent(message: ConsoleAssistantMessage): { text: string; thinking: string } {
  if (message.status === 'stopped') {
    return { text: consoleVisibleText(message.partialText ?? ''), thinking: consoleVisibleText(message.partialThinking ?? '') }
  }
  let text = ''
  let summary = ''
  let reasoning = ''
  for (const output of message.outputItems) {
    const item = record(output)
    if (item?.type === 'message') text += readableParts(item.content)
    if (item?.type === 'reasoning') {
      summary += readableParts(item.summary)
      reasoning += readableParts(item.content) || readableParts(item.text)
    }
  }
  return { text: consoleVisibleText(text), thinking: consoleVisibleText(summary) || consoleVisibleText(reasoning) }
}

function replay(messages: ConsoleMessage[]): unknown[] {
  return messages.flatMap((message): unknown[] => {
    if (message.role === 'user') return [{ role: 'user', content: message.text }]
    if (message.status === 'failed') return []
    if (message.status === 'stopped') {
      const text = consoleVisibleText(message.partialText ?? '')
      return text ? [{ role: 'assistant', content: text }] : []
    }
    return message.outputItems
  })
}

function chatError(error: unknown): ConsoleChatError {
  const object = record(error)
  return {
    message: typeof object?.message === 'string' ? object.message : String(error),
    ...(typeof object?.status === 'number' ? { status: object.status } : {}),
    ...(typeof object?.code === 'string' ? { code: object.code } : {}),
    ...(record(object?.params) ? { params: object?.params as Record<string, unknown> } : {}),
  }
}

interface ActiveRequest {
  abort: AbortController
  message: ConsoleAssistantMessage
  generation: ConsoleGeneration
}

export class ConsoleChatController {
  private conversations: ConsoleConversation[] = []
  private preferences: ConsoleChatPreferences = {}
  private apiKeys: ConsoleApiKey[] = []
  private models: Route[] = []
  private providers: { id: string; is_enabled: boolean }[] = []
  private currentId: string | null = null
  private draftKey: string | null = null
  private draftModel: string | null = null
  private draftThinking: ConsoleThinkingSelection = 'default'
  private active = new Map<string, ActiveRequest>()
  private loading = true
  private loadError: unknown = null
  private catalogError: unknown = null
  private storageError: unknown = null
  private writes: Promise<void> = Promise.resolve()
  private starting?: Promise<void>
  private catalogEpoch = 0
  private catalogLoaded = false
  // 持久化删除期间禁止新请求；失败时保留可重试的停止历史。
  private deleting = new Set<string>()
  private clearing = false

  constructor(private readonly options: ConsoleChatControllerOptions) {}

  private now(): number { return (this.options.now ?? Date.now)() }
  private time(): string { return new Date(this.now()).toISOString() }
  private id(): string { return (this.options.createId ?? (() => crypto.randomUUID()))() }
  private current(): ConsoleConversation | null {
    return this.conversations.find((conversation) => conversation.id === this.currentId) ?? null
  }
  private keys(): ConsoleApiKey[] {
    return this.apiKeys.filter((key) => !apiKeyReadOnlyReason(key, this.now()))
  }
  private allowedModels(keyId: string | null): Route[] {
    const key = this.apiKeys.find((candidate) => candidate.id === keyId)
    return key ? this.models.filter((model) => model.is_enabled && apiKeyAllowsModel(key.model_ids, model.id)) : []
  }
  readOnlyReasonFor(id: string): ConsoleReadOnlyReason | null {
    if (!this.catalogLoaded) return null
    const conversation = this.conversations.find((candidate) => candidate.id === id)
    return conversation ? apiKeyReadOnlyReason(this.apiKeys.find((key) => key.id === conversation.apiKeyId), this.now()) : null
  }
  private blocker(): ConsoleChatBlocker | null {
    if (!this.providers.length) return 'no-services'
    if (!this.providers.some((provider) => provider.is_enabled)) return 'disabled-services'
    if (!this.models.length) return 'no-models'
    const enabledServices = new Set(this.providers.filter((provider) => provider.is_enabled).map((provider) => provider.id))
    const connectableModels = this.models.filter((model) => model.is_enabled &&
      model.targets.some((target) => target.enabled && enabledServices.has(target.provider_id)))
    if (!connectableModels.length) return 'disabled-models'
    if (!this.apiKeys.length) return 'no-keys'
    if (!this.keys().some((key) => connectableModels.some((model) => apiKeyAllowsModel(key.model_ids, model.id)))) return 'unavailable-keys'
    return null
  }
  get snapshot(): ConsoleChatSnapshot {
    // 响应式壳依赖值身份变化；仅复制可变的对话与消息壳，不复制原样回放的输出项。
    const conversations = this.conversations.map((conversation) => ({
      ...conversation,
      messages: conversation.messages.map((message) => ({ ...message })),
    })).sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))
    const current = conversations.find((conversation) => conversation.id === this.currentId) ?? null
    const selectedKeyId = current?.apiKeyId ?? this.draftKey
    const candidates = this.allowedModels(selectedKeyId)
    const selectedModelId = current ? current.selectedModelId : this.draftModel
    const model = candidates.find((candidate) => candidate.id === selectedModelId)
    const lastMessage = current?.messages.at(-1)
    return {
      conversations,
      currentConversation: current,
      currentConversationId: this.currentId,
      missingConversation: this.currentId !== null && current === null,
      loading: this.loading,
      loadError: this.loadError,
      catalogError: this.catalogError,
      storageError: this.storageError,
      keyCandidates: this.keys(),
      modelCandidates: candidates,
      selectedKeyId,
      selectedModelId: model?.id ?? null,
      thinkingSelection: current?.thinkingSelection ?? this.draftThinking,
      thinkingLevels: [...new Set(model?.supported_thinking_levels ?? [])],
      readOnlyReason: current ? this.readOnlyReasonFor(current.id) : null,
      retryModelAvailable: lastMessage?.role === 'assistant' &&
        candidates.some((candidate) => candidate.model_id === lastMessage.routeId),
      blocker: this.loading || this.loadError || this.catalogError ? null : this.blocker(),
      generations: Object.fromEntries([...this.active].map(([id, request]) => [id, { ...request.generation }])),
    }
  }
  private publish(): void { this.options.onSnapshot?.(this.snapshot) }
  private write(operation: () => Promise<void>): Promise<boolean> {
    const pending = this.writes.then(operation).then(() => {
      this.storageError = null
      return true
    }, (error: unknown) => {
      this.storageError = error
      return false
    }).then((saved) => { this.publish(); return saved })
    this.writes = pending.then(() => {})
    return pending
  }
  private save(conversation: ConsoleConversation): Promise<boolean> {
    // 入队时捕获快照，避免前一个 IndexedDB 事务结束后写入更新的内容。
    const checkpoint = structuredClone(conversation)
    return this.write(() => {
      const current = this.conversations.find((item) => item.id === checkpoint.id)
      if (!current) return Promise.resolve()
      // 标题只取已提交值，流式检查点不能覆盖并发重命名或复活已删除记录。
      checkpoint.title = current.title
      return this.options.store.saveConversation(checkpoint)
    })
  }
  private remember(): void {
    const state = this.snapshot
    this.preferences = {
      ...(state.selectedKeyId ? { apiKeyId: state.selectedKeyId } : {}),
      ...(state.selectedModelId ? { modelId: state.selectedModelId } : {}),
    }
    const preferences = { ...this.preferences }
    void this.write(() => this.options.store.savePreferences(preferences))
  }
  private reconcile(): void {
    const keys = this.keys()
    if (!keys.some((key) => key.id === this.draftKey)) this.draftKey = keys.length === 1 ? keys[0].id : null
    const models = this.allowedModels(this.draftKey)
    if (!models.some((model) => model.id === this.draftModel)) this.draftModel = models.length === 1 ? models[0].id : null
    if (this.draftThinking !== 'default' &&
      !models.find((model) => model.id === this.draftModel)?.supported_thinking_levels.includes(this.draftThinking)) {
      this.draftThinking = 'default'
    }
    for (const conversation of this.conversations) {
      const available = this.allowedModels(conversation.apiKeyId)
      const model = available.find((candidate) => candidate.id === conversation.selectedModelId)
      if (!model) conversation.selectedModelId = available.length === 1 ? available[0].id : ''
      const selected = available.find((candidate) => candidate.id === conversation.selectedModelId)
      if (conversation.thinkingSelection !== 'default' && !selected?.supported_thinking_levels.includes(conversation.thinkingSelection)) {
        conversation.thinkingSelection = 'default'
      }
    }
  }
  start(): Promise<void> {
    this.starting ??= this.initialize().finally(() => {
      if (this.loadError) this.starting = undefined
    })
    return this.starting
  }
  private async initialize(): Promise<void> {
    this.loading = true
    this.loadError = null
    this.publish()
    await Promise.all([
      this.options.store.load().then((loaded) => {
        this.conversations = loaded.conversations
        this.preferences = loaded.preferences
        this.draftKey = loaded.preferences.apiKeyId ?? null
        this.draftModel = loaded.preferences.modelId ?? null
        this.storageError = null
      }, (error: unknown) => { this.loadError = error; this.storageError = error }),
      this.refreshCatalog(),
    ])
    this.reconcile()
    this.loading = false
    this.publish()
  }
  async refreshCatalog(): Promise<void> {
    const epoch = ++this.catalogEpoch
    try {
      const catalog = await this.options.catalog.read()
      if (epoch !== this.catalogEpoch) return
      this.apiKeys = catalog.apiKeys
      this.models = catalog.models
      this.providers = catalog.providers
      this.catalogLoaded = true
      this.catalogError = null
      this.reconcile()
    } catch (error) {
      if (epoch === this.catalogEpoch) this.catalogError = error
    }
    this.publish()
  }
  refreshEligibility(): void {
    this.reconcile()
    this.publish()
  }
  newConversation(): void {
    this.currentId = null
    this.draftKey = this.preferences.apiKeyId ?? null
    this.draftModel = this.preferences.modelId ?? null
    this.draftThinking = 'default'
    this.reconcile()
    this.publish()
  }
  openConversation(id: string | null): void {
    if (id === null) { this.newConversation(); return }
    this.currentId = id
    this.publish()
  }
  selectKey(id: string | null): void {
    if (this.currentId !== null) return
    this.draftKey = this.keys().some((key) => key.id === id) ? id : null
    this.draftModel = null
    this.reconcile()
    this.remember()
    this.publish()
  }
  selectModel(id: string | null): void {
    const current = this.current()
    if (this.currentId !== null && !current) return
    if (this.clearing || (current && this.deleting.has(current.id))) return
    const available = this.allowedModels(current?.apiKeyId ?? this.draftKey)
    const selected = available.find((model) => model.id === id)
    if (current) {
      current.selectedModelId = selected?.id ?? ''
      if (current.thinkingSelection !== 'default' && !selected?.supported_thinking_levels.includes(current.thinkingSelection)) {
        current.thinkingSelection = 'default'
      }
      void this.save(current)
    } else {
      this.draftModel = selected?.id ?? null
      if (this.draftThinking !== 'default' && !selected?.supported_thinking_levels.includes(this.draftThinking)) {
        this.draftThinking = 'default'
      }
    }
    this.remember()
    this.publish()
  }
  selectThinking(selection: ConsoleThinkingSelection): void {
    if (selection !== 'default' && !this.snapshot.thinkingLevels.includes(selection)) return
    const current = this.current()
    if (this.clearing || (current && this.deleting.has(current.id))) return
    if (current) { current.thinkingSelection = selection; void this.save(current) }
    else if (this.currentId === null) this.draftThinking = selection
    this.publish()
  }
  private reasoning(model: Route, selection: ConsoleThinkingSelection): ConsoleResponsesRequest['reasoning'] {
    const effort = selection === 'default' ? model.default_thinking_level : selection
    return { summary: 'auto', ...(effort ? { effort: effort === 'off' ? 'none' : effort } : {}) }
  }
  private canSend(conversation?: ConsoleConversation): boolean {
    return !this.loading && !this.loadError && !this.catalogError && !this.blocker() && !this.clearing &&
      (!conversation || (!this.deleting.has(conversation.id) && !this.active.has(conversation.id) && !this.readOnlyReasonFor(conversation.id)))
  }
  async send(text: string): Promise<void> {
    if (!text.trim() || this.snapshot.missingConversation) return
    let conversation = this.current()
    if (!this.canSend(conversation ?? undefined)) { this.refreshEligibility(); return }
    const state = this.snapshot
    const key = this.keys().find((candidate) => candidate.id === state.selectedKeyId)
    const model = state.modelCandidates.find((candidate) => candidate.id === state.selectedModelId)
    if (!key || !model) return
    if (!conversation) {
      const firstLine = text.trim().split(/\r?\n/, 1)[0]
      const title = [...new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(firstLine)]
        .slice(0, 40).map((segment) => segment.segment).join('')
      conversation = {
        id: this.id(), title, apiKeyId: key.id, apiKeyName: key.name, selectedModelId: model.id,
        thinkingSelection: state.thinkingSelection, createdAt: this.time(), updatedAt: this.time(), messages: [],
      }
      this.conversations.push(conversation)
      this.currentId = conversation.id
    }
    conversation.messages.push({ id: this.id(), role: 'user', text, createdAt: this.time() })
    this.remember()
    await this.generate(conversation, {
      model: model.model_id, stream: true, input: replay(conversation.messages),
      reasoning: this.reasoning(model, state.thinkingSelection),
    }, state.thinkingSelection)
  }
  async retry(): Promise<void> {
    const conversation = this.current()
    if (!conversation || !this.canSend(conversation)) { this.refreshEligibility(); return }
    const last = conversation.messages.at(-1)
    if (last?.role !== 'assistant' || last.status !== 'failed') return
    const model = this.allowedModels(conversation.apiKeyId).find((candidate) => candidate.model_id === last.routeId)
    if (!model) return
    const request: ConsoleResponsesRequest = {
      model: last.routeId, stream: true, input: replay(conversation.messages.slice(0, -1)),
      reasoning: last.requestReasoning ?? this.reasoning(model, last.thinkingLevel),
    }
    conversation.messages.pop()
    await this.generate(conversation, request, last.thinkingLevel)
  }
  async regenerate(): Promise<void> {
    const conversation = this.current()
    if (!conversation || !this.canSend(conversation)) { this.refreshEligibility(); return }
    const last = conversation.messages.at(-1)
    const model = this.snapshot.modelCandidates.find((candidate) => candidate.id === conversation.selectedModelId)
    if (last?.role !== 'assistant' || !model) return
    conversation.messages.pop()
    await this.generate(conversation, {
      model: model.model_id, stream: true, input: replay(conversation.messages),
      reasoning: this.reasoning(model, conversation.thinkingSelection),
    }, conversation.thinkingSelection)
  }
  private async generate(conversation: ConsoleConversation, request: ConsoleResponsesRequest, selection: ConsoleThinkingSelection): Promise<void> {
    const message: ConsoleAssistantMessage = {
      id: this.id(), role: 'assistant', status: 'stopped', routeId: request.model,
      thinkingLevel: selection, requestReasoning: { ...request.reasoning }, outputItems: [],
      partialText: '', createdAt: this.time(),
    }
    const active: ActiveRequest = {
      abort: new AbortController(), message, generation: { text: '', summary: '', reasoning: '' },
    }
    conversation.messages.push(message)
    conversation.updatedAt = this.time()
    this.active.set(conversation.id, active)
    this.publish()
    await this.save(conversation)
    const alive = () => this.active.get(conversation.id) === active && this.conversations.includes(conversation)
    try {
      if (!alive()) return
      const apiKey = await this.options.catalog.revealKey(conversation.apiKeyId)
      if (!alive()) return
      let terminal = false
      for await (const event of this.options.transport.stream({ apiKey, request, signal: active.abort.signal })) {
        if (!alive()) return
        const generation = active.generation
        if (event.type === 'response.output_text.delta' || event.type === 'response.refusal.delta') generation.text += event.delta ?? ''
        else if (event.type === 'response.reasoning_summary_text.delta') generation.summary += event.delta ?? ''
        else if (event.type === 'response.reasoning_text.delta') generation.reasoning += event.delta ?? ''
        else if (event.type === 'error' || event.type === 'response.failed') {
          const failure = event.response?.error ?? event.error ?? { message: event.message ?? 'Response failed', code: event.code }
          throw Object.assign(new Error(failure.message), failure)
        } else if (event.type === 'response.completed' || event.type === 'response.incomplete') {
          const response = event.response
          if (!response) throw new Error('Response ended without a result')
          if (response.error || response.status === 'failed') {
            throw Object.assign(new Error(response.error?.message ?? 'Response failed'), response.error)
          }
          message.status = response.status === 'incomplete' || event.type === 'response.incomplete' ? 'incomplete' : 'completed'
          message.outputItems = response.output ?? []
          if (response.usage) {
            message.usage = {
              ...(typeof response.usage.input_tokens === 'number' ? { inputTokens: response.usage.input_tokens } : {}),
              ...(typeof response.usage.output_tokens === 'number' ? { outputTokens: response.usage.output_tokens } : {}),
            }
          }
          delete message.partialText
          delete message.partialThinking
          terminal = true
          break
        } else continue
        // 流式检查点按 stopped 持久化，刷新时不依赖卸载阶段的异步写入。
        message.partialText = consoleVisibleText(generation.text)
        message.partialThinking = consoleVisibleText(generation.summary) || consoleVisibleText(generation.reasoning)
        conversation.updatedAt = this.time()
        this.publish()
        await this.save(conversation)
      }
      if (!alive()) return
      if (!terminal) throw new Error('Response stream ended before completion')
    } catch (error) {
      if (!alive()) return
      if (active.abort.signal.aborted) message.status = 'stopped'
      else {
        message.status = 'failed'
        message.outputItems = []
        delete message.partialText
        delete message.partialThinking
        message.error = chatError(error)
        if (message.error.status === 401 || message.error.status === 403 ||
          ['invalid_api_key', 'authentication_error', 'unauthorized', 'STRAVIA_AUTH_ERROR',
            'STRAVIA_FORBIDDEN', 'STRAVIA_NOT_FOUND', 'model_not_found'].includes(message.error.code ?? '')) {
          await this.refreshCatalog()
        }
      }
    }
    if (!alive()) return
    this.active.delete(conversation.id)
    conversation.updatedAt = this.time()
    this.publish()
    await this.save(conversation)
  }
  async stop(id = this.currentId ?? ''): Promise<void> {
    const active = this.active.get(id)
    const conversation = this.conversations.find((candidate) => candidate.id === id)
    if (!active || !conversation) return
    this.active.delete(id)
    active.abort.abort()
    active.message.status = 'stopped'
    active.message.partialText = consoleVisibleText(active.generation.text)
    active.message.partialThinking = consoleVisibleText(active.generation.summary) || consoleVisibleText(active.generation.reasoning)
    conversation.updatedAt = this.time()
    this.publish()
    await this.save(conversation)
  }
  async stopAll(): Promise<void> {
    await Promise.all([...this.active.keys()].map((id) => this.stop(id)))
  }
  async rename(id: string, title: string): Promise<void> {
    const conversation = this.conversations.find((candidate) => candidate.id === id)
    if (!conversation || !title.trim() || this.clearing || this.deleting.has(id)) return
    const nextTitle = title.trim()
    if (!await this.write(async () => {
      const checkpoint = structuredClone(conversation)
      checkpoint.title = nextTitle
      await this.options.store.saveConversation(checkpoint)
      conversation.title = nextTitle
    })) throw this.storageError
    this.publish()
  }
  async delete(id: string): Promise<void> {
    this.deleting.add(id)
    try {
      await this.stop(id)
      if (!await this.write(() => this.options.store.deleteConversation(id))) throw this.storageError
      this.conversations = this.conversations.filter((conversation) => conversation.id !== id)
      if (this.currentId === id) this.newConversation()
    } finally {
      this.deleting.delete(id)
      this.publish()
    }
  }
  async clearAll(): Promise<void> {
    this.clearing = true
    try {
      await this.stopAll()
      if (!await this.write(() => this.options.store.clearConversations())) throw this.storageError
      this.conversations = []
      this.newConversation()
    } finally {
      this.clearing = false
      this.publish()
    }
  }
  findConversations(query: string): ConsoleConversation[] {
    const normalized = query.trim().toLocaleLowerCase()
    return this.snapshot.conversations.filter((conversation) => !normalized ||
      conversation.title.toLocaleLowerCase().includes(normalized) ||
      conversation.messages.some((message) => {
        const assistant = message.role === 'assistant' ? consoleAssistantContent(message) : null
        const content = message.role === 'user' ? message.text : `${assistant?.text ?? ''}\n${assistant?.thinking ?? ''}`
        return content.toLocaleLowerCase().includes(normalized)
      }))
  }
}

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
  ConsoleImageAttachment,
  ConsoleMessage,
  ConsoleReadOnlyReason,
  ConsoleResponse,
  ConsoleResponsesRequest,
  ConsoleResponsesEvent,
  ConsoleThinkingSelection,
  ConsoleTokenUsage,
} from '$lib/console-chat-types'
import type { Route } from '$lib/types'
import type { ThinkingActivity } from '$lib/observation-activities'

/** 隐藏普通 HTML 注释及尚未收完的流式注释；不解释 Core 私有历史语法。 */
export function consoleVisibleText(text: string): string {
  return text.replace(/<!--[\s\S]*?(?:-->|$)/g, '')
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : null
}

function readableParts(value: unknown): string {
  if (typeof value === 'string') return value
  if (!Array.isArray(value)) return ''
  return value
    .map((part) => {
      const item = record(part)
      return typeof item?.text === 'string' ? item.text : typeof item?.refusal === 'string' ? item.refusal : ''
    })
    .join('')
}

export function consoleAssistantContent(message: ConsoleAssistantMessage): { text: string; thinking: string } {
  if (message.status === 'stopped' || message.status === 'failed') {
    return {
      text: consoleVisibleText(message.partialText ?? ''),
      thinking: consoleReasoningActivities(message)
        .map((activity) => activity.text)
        .join('\n\n'),
    }
  }
  let text = ''
  for (const output of message.outputItems) {
    const item = record(output)
    if (item?.type === 'message') text += readableParts(item.content)
  }
  return {
    text: consoleVisibleText(text),
    thinking: consoleReasoningActivities(message)
      .map((activity) => activity.text)
      .join('\n\n'),
  }
}

function hasVisibleThinkingText(activity: ThinkingActivity): boolean {
  return activity.text.trim().length > 0
}

export function consoleReasoningActivities(message: ConsoleAssistantMessage): ThinkingActivity[] {
  if (message.status === 'stopped' || message.status === 'failed') {
    if (message.partialActivities)
      return message.partialActivities
        .map((activity) => ({ ...activity, text: consoleVisibleText(activity.text), live: false }))
        .filter(hasVisibleThinkingText)
    const text = consoleVisibleText(message.partialThinking ?? '')
    const activities: ThinkingActivity[] = [
      { kind: 'thinking', id: `${message.id}:reasoning`, at: Date.parse(message.createdAt), text, live: false },
    ]
    return activities.filter(hasVisibleThinkingText)
  }
  return message.outputItems.flatMap((output, index): ThinkingActivity[] => {
    const item = record(output)
    if (item?.type !== 'reasoning') return []
    const text =
      consoleVisibleText(readableParts(item.summary)) ||
      consoleVisibleText(readableParts(item.content) || readableParts(item.text))
    return [
      {
        kind: 'thinking',
        id: typeof item.id === 'string' ? item.id : `${message.id}:reasoning:${index}`,
        at: Date.parse(message.createdAt),
        text,
        live: false,
      },
    ]
  }).filter(hasVisibleThinkingText)
}

function replay(messages: ConsoleMessage[]): unknown[] {
  return messages.flatMap((message): unknown[] => {
    if (message.role === 'user')
      return [
        {
          role: 'user',
          content: message.images?.length
            ? [
                { type: 'input_text', text: message.text },
                ...message.images.map((image) => ({ type: 'input_image', image_url: image.dataUrl })),
              ]
            : message.text,
        },
      ]
    if (message.status === 'failed') return []
    if (message.status === 'stopped') {
      const text = consoleVisibleText(message.partialText ?? '')
      return text ? [{ role: 'assistant', content: text }] : []
    }
    return message.outputItems
  })
}

/**
 * 终态 usage 已由 Stravia 合计本轮全部内部模型请求（平台工具轮次），客户端不再累加。
 * 只保存已报告的分项；净输入需要总输入与缓存读取同时已知，缺失不补零。
 */
function tokenUsage(usage: ConsoleResponse['usage']): ConsoleTokenUsage | undefined {
  if (!usage) return undefined
  const count = (value: number | null | undefined) => (typeof value === 'number' ? value : undefined)
  const input = count(usage.input_tokens)
  const cacheRead = count(usage.input_tokens_details?.cached_tokens)
  const result: ConsoleTokenUsage = {
    inputTokens: input !== undefined && cacheRead !== undefined ? Math.max(input - cacheRead, 0) : undefined,
    outputTokens: count(usage.output_tokens),
    cacheReadTokens: cacheRead,
    cacheWriteTokens: count(usage.input_tokens_details?.cache_write_tokens),
  }
  return Object.fromEntries(Object.entries(result).filter(([, value]) => value !== undefined))
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
  reasoningItems: Map<number, ReasoningItem>
}

interface ReasoningItem {
  id: string
  at: number
  live: boolean
  summary: Map<number, string>
  content: Map<number, string>
}

function orderedParts(parts: Map<number, string>): string {
  return [...parts]
    .sort(([a], [b]) => a - b)
    .map(([, text]) => text)
    .join('')
}

/** Only public Responses item/part events supply readable reasoning. */
function updateReasoning(active: ActiveRequest, event: ConsoleResponsesEvent, at: number): boolean {
  const outputItem = record(event.item)
  const part = record(event.part)
  const itemEvent = event.type === 'response.output_item.added' || event.type === 'response.output_item.done'
  const summaryEvent = event.type.startsWith('response.reasoning_summary_')
  const contentEvent =
    event.type.startsWith('response.reasoning_text.') ||
    (event.type.startsWith('response.content_part.') && part?.type === 'reasoning_text')
  if ((!itemEvent || outputItem?.type !== 'reasoning') && !summaryEvent && !contentEvent) return false
  const publicId = typeof outputItem?.id === 'string' ? outputItem.id : event.item_id
  const existingIndex = publicId ? [...active.reasoningItems].find(([, item]) => item.id === publicId)?.[0] : undefined
  const index = event.output_index ?? existingIndex ?? 0
  let item = active.reasoningItems.get(index)
  if (!item) {
    item = {
      id: publicId ?? `${active.message.id}:reasoning:${index}`,
      at,
      live: true,
      summary: new Map(),
      content: new Map(),
    }
    active.reasoningItems.set(index, item)
  } else if (publicId) item.id = publicId
  if (itemEvent) {
    if (Array.isArray(outputItem?.summary)) {
      item.summary = new Map(outputItem.summary.map((value, partIndex) => [partIndex, readableParts([value])]))
    }
    if (Array.isArray(outputItem?.content)) {
      item.content = new Map(outputItem.content.map((value, partIndex) => [partIndex, readableParts([value])]))
    } else if (typeof outputItem?.text === 'string') item.content = new Map([[0, outputItem.text]])
    item.live = event.type !== 'response.output_item.done' && outputItem?.status !== 'completed'
  } else {
    const parts = summaryEvent ? item.summary : item.content
    const partIndex = (summaryEvent ? event.summary_index : event.content_index) ?? 0
    if (event.type.endsWith('.delta')) {
      parts.set(partIndex, (parts.get(partIndex) ?? '') + (event.delta ?? ''))
    } else if (typeof event.text === 'string') {
      parts.set(partIndex, event.text)
    } else if (typeof part?.text === 'string') {
      parts.set(partIndex, part.text)
    }
  }
  const ordered = [...active.reasoningItems].sort(([a], [b]) => a - b).map(([, value]) => value)
  active.generation.summary = ordered.map((value) => orderedParts(value.summary)).join('')
  active.generation.reasoning = ordered.map((value) => orderedParts(value.content)).join('')
  active.generation.activities = ordered
    .map((value): ThinkingActivity => ({
      kind: 'thinking',
      id: value.id,
      at: value.at,
      live: value.live,
      text: consoleVisibleText(orderedParts(value.summary)) || consoleVisibleText(orderedParts(value.content)),
    }))
    .filter(hasVisibleThinkingText)
  return true
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
  private inputError: ConsoleChatError | null = null
  private writes: Promise<void> = Promise.resolve()
  private starting?: Promise<void>
  private catalogEpoch = 0
  private catalogLoaded = false
  // 持久化删除期间禁止新请求；失败时保留可重试的停止历史。
  private deleting = new Set<string>()
  private clearing = false

  constructor(private readonly options: ConsoleChatControllerOptions) {}

  private now(): number {
    return (this.options.now ?? Date.now)()
  }
  private time(): string {
    return new Date(this.now()).toISOString()
  }
  private id(): string {
    return (this.options.createId ?? (() => crypto.randomUUID()))()
  }
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
    return conversation
      ? apiKeyReadOnlyReason(
          this.apiKeys.find((key) => key.id === conversation.apiKeyId),
          this.now(),
        )
      : null
  }
  private blocker(): ConsoleChatBlocker | null {
    if (!this.providers.length) return 'no-services'
    if (!this.providers.some((provider) => provider.is_enabled)) return 'disabled-services'
    if (!this.models.length) return 'no-models'
    const enabledServices = new Set(
      this.providers.filter((provider) => provider.is_enabled).map((provider) => provider.id),
    )
    const connectableModels = this.models.filter(
      (model) =>
        model.is_enabled && model.targets.some((target) => target.enabled && enabledServices.has(target.provider_id)),
    )
    if (!connectableModels.length) return 'disabled-models'
    if (!this.apiKeys.length) return 'no-keys'
    if (!this.keys().some((key) => connectableModels.some((model) => apiKeyAllowsModel(key.model_ids, model.id))))
      return 'unavailable-keys'
    return null
  }
  get snapshot(): ConsoleChatSnapshot {
    // 响应式壳依赖值身份变化；仅复制可变的对话与消息壳，不复制原样回放的输出项。
    const conversations = this.conversations
      .map((conversation) => ({ ...conversation, messages: conversation.messages.map((message) => ({ ...message })) }))
      .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))
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
      inputError: this.inputError,
      historyHasImages: !!current?.messages.some((message) => message.role === 'user' && message.images?.length),
      modelSupportsImages: model?.supports_image_input === true,
      keyCandidates: this.keys(),
      modelCandidates: candidates,
      selectedKeyId,
      selectedModelId: model?.id ?? null,
      thinkingSelection: current?.thinkingSelection ?? this.draftThinking,
      thinkingLevels: [...new Set(model?.supported_thinking_levels ?? [])],
      readOnlyReason: current ? this.readOnlyReasonFor(current.id) : null,
      retryModelAvailable:
        lastMessage?.role === 'assistant' && candidates.some((candidate) => candidate.model_id === lastMessage.routeId),
      blocker: this.loading || this.loadError || this.catalogError ? null : this.blocker(),
      generations: Object.fromEntries(
        [...this.active].map(([id, request]) => [
          id,
          { ...request.generation, activities: request.generation.activities.map((activity) => ({ ...activity })) },
        ]),
      ),
    }
  }
  private publish(): void {
    this.options.onSnapshot?.(this.snapshot)
  }
  private write(operation: () => Promise<void>): Promise<boolean> {
    const pending = this.writes
      .then(operation)
      .then(
        () => {
          this.storageError = null
          return true
        },
        (error: unknown) => {
          this.storageError = error
          return false
        },
      )
      .then((saved) => {
        this.publish()
        return saved
      })
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
    if (!models.some((model) => model.id === this.draftModel))
      this.draftModel = models.length === 1 ? models[0].id : null
    if (
      this.draftThinking !== 'default' &&
      !models.find((model) => model.id === this.draftModel)?.supported_thinking_levels.includes(this.draftThinking)
    ) {
      this.draftThinking = 'default'
    }
    for (const conversation of this.conversations) {
      const available = this.allowedModels(conversation.apiKeyId)
      const model = available.find((candidate) => candidate.id === conversation.selectedModelId)
      if (!model) conversation.selectedModelId = available.length === 1 ? available[0].id : ''
      const selected = available.find((candidate) => candidate.id === conversation.selectedModelId)
      if (
        conversation.thinkingSelection !== 'default' &&
        !selected?.supported_thinking_levels.includes(conversation.thinkingSelection)
      ) {
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
      this.options.store.load().then(
        (loaded) => {
          this.conversations = loaded.conversations
          this.preferences = loaded.preferences
          this.draftKey = loaded.preferences.apiKeyId ?? null
          this.draftModel = loaded.preferences.modelId ?? null
          this.storageError = null
        },
        (error: unknown) => {
          this.loadError = error
          this.storageError = error
        },
      ),
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
    this.inputError = null
    this.currentId = null
    this.draftKey = this.preferences.apiKeyId ?? null
    this.draftModel = this.preferences.modelId ?? null
    this.draftThinking = 'default'
    this.reconcile()
    this.publish()
  }
  openConversation(id: string | null): void {
    this.inputError = null
    if (id === null) {
      this.newConversation()
      return
    }
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
    this.inputError = null
    const available = this.allowedModels(current?.apiKeyId ?? this.draftKey)
    const selected = available.find((model) => model.id === id)
    if (current) {
      current.selectedModelId = selected?.id ?? ''
      if (
        current.thinkingSelection !== 'default' &&
        !selected?.supported_thinking_levels.includes(current.thinkingSelection)
      ) {
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
    if (current) {
      current.thinkingSelection = selection
      void this.save(current)
    } else if (this.currentId === null) this.draftThinking = selection
    this.publish()
  }
  private reasoning(model: Route, selection: ConsoleThinkingSelection): ConsoleResponsesRequest['reasoning'] {
    const effort = selection === 'default' ? model.default_thinking_level : selection
    return { summary: 'auto', ...(effort ? { effort: effort === 'off' ? 'none' : effort } : {}) }
  }
  private canSend(conversation?: ConsoleConversation): boolean {
    return (
      !this.loading &&
      !this.loadError &&
      !this.catalogError &&
      !this.blocker() &&
      !this.clearing &&
      (!conversation ||
        (!this.deleting.has(conversation.id) &&
          !this.active.has(conversation.id) &&
          !this.readOnlyReasonFor(conversation.id)))
    )
  }
  private imagesCompatible(model: Route, messages: ConsoleMessage[], images: ConsoleImageAttachment[] = []): boolean {
    this.inputError = null
    if (
      model.supports_image_input === true ||
      (!images.length && !messages.some((message) => message.role === 'user' && message.images?.length))
    )
      return true
    this.inputError = {
      code: 'CONSOLE_IMAGE_INPUT_UNSUPPORTED',
      message: 'This model does not support images in this message or conversation. Choose an image-capable model.',
    }
    this.publish()
    return false
  }
  async send(text: string, images: ConsoleImageAttachment[] = []): Promise<boolean> {
    this.inputError = null
    if (!text.trim() || this.snapshot.missingConversation) return false
    if (
      images.some(
        (image) =>
          !['image/png', 'image/jpeg', 'image/webp'].includes(image.mediaType) ||
          !image.dataUrl.startsWith(`data:${image.mediaType};base64,`) ||
          !/^[A-Za-z0-9+/]+={0,2}$/.test(image.dataUrl.slice(image.dataUrl.indexOf(',') + 1)),
      )
    ) {
      this.inputError = {
        code: 'CONSOLE_IMAGE_ATTACHMENT_INVALID',
        message: 'Only original PNG, JPEG and WebP images are supported.',
      }
      this.publish()
      return false
    }
    let conversation = this.current()
    if (!this.canSend(conversation ?? undefined)) {
      this.refreshEligibility()
      return false
    }
    const state = this.snapshot
    const key = this.keys().find((candidate) => candidate.id === state.selectedKeyId)
    const model = state.modelCandidates.find((candidate) => candidate.id === state.selectedModelId)
    if (!key || !model || !this.imagesCompatible(model, conversation?.messages ?? [], images)) return false
    const isNew = !conversation
    if (!conversation) {
      const firstLine = text.trim().split(/\r?\n/, 1)[0]
      const title = [...new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(firstLine)]
        .slice(0, 40)
        .map((segment) => segment.segment)
        .join('')
      conversation = {
        id: this.id(),
        title,
        apiKeyId: key.id,
        apiKeyName: key.name,
        selectedModelId: model.id,
        thinkingSelection: state.thinkingSelection,
        createdAt: this.time(),
        updatedAt: this.time(),
        messages: [],
      }
      this.conversations.push(conversation)
      this.currentId = conversation.id
    }
    const previousMessages = [...conversation.messages]
    conversation.messages.push({
      id: this.id(),
      role: 'user',
      text,
      ...(images.length ? { images: images.map((image) => ({ ...image })) } : {}),
      createdAt: this.time(),
    })
    this.remember()
    const result = await this.generate(
      conversation,
      {
        model: model.model_id,
        stream: true,
        input: replay(conversation.messages),
        reasoning: this.reasoning(model, state.thinkingSelection),
      },
      state.thinkingSelection,
      previousMessages,
    )
    if (
      !result &&
      conversation.messages.length === previousMessages.length &&
      isNew &&
      this.conversations.includes(conversation)
    ) {
      this.conversations = this.conversations.filter((item) => item !== conversation)
      if (this.currentId === conversation.id) this.currentId = null
      this.publish()
    }
    return result
  }
  async retry(): Promise<void> {
    const conversation = this.current()
    if (!conversation || !this.canSend(conversation)) {
      this.refreshEligibility()
      return
    }
    const last = conversation.messages.at(-1)
    if (last?.role !== 'assistant' || last.status !== 'failed') return
    const model = this.allowedModels(conversation.apiKeyId).find((candidate) => candidate.model_id === last.routeId)
    if (!model || !this.imagesCompatible(model, conversation.messages)) return
    const request: ConsoleResponsesRequest = {
      model: last.routeId,
      stream: true,
      input: replay(conversation.messages.slice(0, -1)),
      reasoning: last.requestReasoning ?? this.reasoning(model, last.thinkingLevel),
    }
    const previousMessages = [...conversation.messages]
    conversation.messages.pop()
    await this.generate(conversation, request, last.thinkingLevel, previousMessages)
  }
  async regenerate(): Promise<void> {
    const conversation = this.current()
    if (!conversation || !this.canSend(conversation)) {
      this.refreshEligibility()
      return
    }
    const last = conversation.messages.at(-1)
    const model = this.snapshot.modelCandidates.find((candidate) => candidate.id === conversation.selectedModelId)
    if (last?.role !== 'assistant' || !model || !this.imagesCompatible(model, conversation.messages)) return
    const previousMessages = [...conversation.messages]
    conversation.messages.pop()
    await this.generate(
      conversation,
      {
        model: model.model_id,
        stream: true,
        input: replay(conversation.messages),
        reasoning: this.reasoning(model, conversation.thinkingSelection),
      },
      conversation.thinkingSelection,
      previousMessages,
    )
  }
  private async generate(
    conversation: ConsoleConversation,
    request: ConsoleResponsesRequest,
    selection: ConsoleThinkingSelection,
    previousMessages: ConsoleMessage[],
  ): Promise<boolean> {
    const previousUpdatedAt = conversation.updatedAt
    const message: ConsoleAssistantMessage = {
      id: this.id(),
      role: 'assistant',
      status: 'stopped',
      routeId: request.model,
      thinkingLevel: selection,
      requestReasoning: { ...request.reasoning },
      outputItems: [],
      partialText: '',
      createdAt: this.time(),
    }
    const active: ActiveRequest = {
      abort: new AbortController(),
      message,
      generation: { text: '', summary: '', reasoning: '', activities: [] },
      reasoningItems: new Map(),
    }
    conversation.messages.push(message)
    conversation.updatedAt = this.time()
    this.active.set(conversation.id, active)
    this.publish()
    if (!(await this.save(conversation))) {
      if (this.active.get(conversation.id) === active) {
        this.active.delete(conversation.id)
        conversation.messages = previousMessages
        conversation.updatedAt = previousUpdatedAt
        this.publish()
      }
      return false
    }
    const alive = () => this.active.get(conversation.id) === active && this.conversations.includes(conversation)
    try {
      if (!alive()) return false
      const apiKey = await this.options.catalog.revealKey(conversation.apiKeyId)
      if (!alive()) return false
      let terminal = false
      for await (const event of this.options.transport.stream({ apiKey, request, signal: active.abort.signal })) {
        if (!alive()) return false
        const generation = active.generation
        if (event.type === 'response.output_text.delta' || event.type === 'response.refusal.delta')
          generation.text += event.delta ?? ''
        else if (updateReasoning(active, event, this.now())) {
          // Per-item lifecycle is independent of the surrounding response.
        } else if (event.type === 'error' || event.type === 'response.failed') {
          // 失败终态可能带有已消耗的真实用量；没有报告时保持未知。
          const usage = tokenUsage(event.response?.usage)
          if (usage) message.usage = usage
          const failure = event.response?.error ??
            event.error ?? { message: event.message ?? 'Response failed', code: event.code }
          throw Object.assign(new Error(failure.message), failure)
        } else if (event.type === 'response.completed' || event.type === 'response.incomplete') {
          const response = event.response
          if (!response) throw new Error('Response ended without a result')
          const usage = tokenUsage(response.usage)
          if (usage) message.usage = usage
          if (response.error || response.status === 'failed') {
            throw Object.assign(new Error(response.error?.message ?? 'Response failed'), response.error)
          }
          message.status =
            response.status === 'incomplete' || event.type === 'response.incomplete' ? 'incomplete' : 'completed'
          message.outputItems = response.output ?? []
          delete message.partialText
          delete message.partialThinking
          delete message.partialActivities
          generation.activities = consoleReasoningActivities(message)
          this.publish()
          terminal = true
          break
        } else continue
        // 流式检查点按 stopped 持久化，刷新时不依赖卸载阶段的异步写入。
        message.partialText = consoleVisibleText(generation.text)
        message.partialActivities = generation.activities.map((activity) => ({ ...activity, live: false }))
        message.partialThinking = message.partialActivities.map((activity) => activity.text).join('\n\n')
        conversation.updatedAt = this.time()
        this.publish()
        if (!(await this.save(conversation))) {
          active.abort.abort()
          throw this.storageError
        }
      }
      if (!alive()) return false
      if (!terminal) throw new Error('Response stream ended before completion')
    } catch (error) {
      if (!alive()) return false
      if (active.abort.signal.aborted && !this.storageError) message.status = 'stopped'
      else {
        message.status = 'failed'
        message.outputItems = []
        message.error = chatError(error)
        if (
          message.error.status === 401 ||
          message.error.status === 403 ||
          [
            'invalid_api_key',
            'authentication_error',
            'unauthorized',
            'STRAVIA_AUTH_ERROR',
            'STRAVIA_FORBIDDEN',
            'STRAVIA_NOT_FOUND',
            'model_not_found',
          ].includes(message.error.code ?? '')
        ) {
          await this.refreshCatalog()
        }
      }
    }
    if (!alive()) return false
    this.active.delete(conversation.id)
    conversation.updatedAt = this.time()
    this.publish()
    const saved = await this.save(conversation)
    return saved && (message.status === 'completed' || message.status === 'incomplete')
  }
  async stop(id = this.currentId ?? ''): Promise<void> {
    const active = this.active.get(id)
    const conversation = this.conversations.find((candidate) => candidate.id === id)
    if (!active || !conversation) return
    this.active.delete(id)
    active.abort.abort()
    active.message.status = 'stopped'
    active.message.partialText = consoleVisibleText(active.generation.text)
    active.message.partialThinking =
      consoleVisibleText(active.generation.summary) || consoleVisibleText(active.generation.reasoning)
    active.message.partialActivities = active.generation.activities.map((activity) => ({ ...activity, live: false }))
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
    if (
      !(await this.write(async () => {
        const checkpoint = structuredClone(conversation)
        checkpoint.title = nextTitle
        await this.options.store.saveConversation(checkpoint)
        conversation.title = nextTitle
      }))
    )
      throw this.storageError
    this.publish()
  }
  async delete(id: string): Promise<void> {
    this.deleting.add(id)
    try {
      await this.stop(id)
      if (!(await this.write(() => this.options.store.deleteConversation(id)))) throw this.storageError
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
      if (!(await this.write(() => this.options.store.clearConversations()))) throw this.storageError
      this.conversations = []
      this.newConversation()
    } finally {
      this.clearing = false
      this.publish()
    }
  }
  findConversations(query: string): ConsoleConversation[] {
    const normalized = query.trim().toLocaleLowerCase()
    return this.snapshot.conversations.filter(
      (conversation) =>
        !normalized ||
        conversation.title.toLocaleLowerCase().includes(normalized) ||
        conversation.messages.some((message) => {
          const assistant = message.role === 'assistant' ? consoleAssistantContent(message) : null
          const content =
            message.role === 'user' ? message.text : `${assistant?.text ?? ''}\n${assistant?.thinking ?? ''}`
          return content.toLocaleLowerCase().includes(normalized)
        }),
    )
  }
}

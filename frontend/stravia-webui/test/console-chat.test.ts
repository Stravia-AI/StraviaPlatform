import { describe, expect, test } from 'bun:test'
import { rejects } from 'node:assert/strict'
import {
  ConsoleChatController,
  consoleAssistantContent,
  consoleReasoningActivities,
  consoleVisibleText,
} from '../src/lib/console-chat'
import type {
  ConsoleAdminCatalog,
  ConsoleApiKey,
  ConsoleChatBlocker,
  ConsoleChatPreferences,
  ConsoleChatSnapshot,
  ConsoleConversation,
  ConsoleResponsesEvent,
  ConsoleResponsesRequest,
  ConversationStore,
  ResponsesTransport,
} from '../src/lib/console-chat-types'
import type { Route } from '../src/lib/types'

function key(id = 'key-a', overrides: Partial<ConsoleApiKey> = {}): ConsoleApiKey {
  return {
    id,
    name: id,
    is_enabled: true,
    expires_at: null,
    model_ids: [],
    rpm_limit: null,
    mcp_access_enabled: false,
    transparent_injection_enabled: false,
    inject_web_search: false,
    inject_media_understanding: false,
    inject_media_generation: false,
    created_at: '',
    updated_at: '',
    ...overrides,
  }
}
function model(id = 'route-a', overrides: Partial<Route> = {}): Route {
  return {
    id,
    model_id: id === 'route-a' ? 'model-a' : 'model-b',
    balance: 'traffic_equalization',
    target_provider: 'provider',
    target_model: null,
    is_enabled: true,
    created_at: '',
    supported_thinking_levels: ['off', 'low', 'high'],
    targets: [
      {
        id: 'target',
        model_id: id,
        provider_id: 'provider',
        model: 'upstream',
        enabled: true,
        priority: 0,
        first_token_timeout_ms: 30_000,
        target_retry_budget: 0,
        target_cooldown_ms: 0,
        created_at: '',
        thinking_level_map: [],
      },
    ],
    ...overrides,
  }
}
function complete(text = 'Answer', extra = {}): ConsoleResponsesEvent {
  return {
    type: 'response.completed',
    response: {
      status: 'completed',
      output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text }] }],
      ...extra,
    },
  }
}
function deferred<T>() {
  return Promise.withResolvers<T>()
}
class MemoryStore implements ConversationStore {
  conversations: ConsoleConversation[] = []
  preferences: ConsoleChatPreferences = {}
  failure: Error | null = null
  gate?: Promise<void>
  load() {
    return Promise.resolve(structuredClone({ conversations: this.conversations, preferences: this.preferences }))
  }
  async saveConversation(conversation: ConsoleConversation) {
    if (this.gate) await this.gate
    if (this.failure) throw this.failure
    this.conversations = [
      ...this.conversations.filter((item) => item.id !== conversation.id),
      structuredClone(conversation),
    ]
  }
  savePreferences(preferences: ConsoleChatPreferences) {
    this.preferences = structuredClone(preferences)
    return Promise.resolve()
  }
  deleteConversation(id: string) {
    this.conversations = this.conversations.filter((item) => item.id !== id)
    return Promise.resolve()
  }
  clearConversations() {
    this.conversations = []
    return Promise.resolve()
  }
}
function harness(
  options: {
    keys?: ConsoleApiKey[]
    models?: Route[]
    store?: MemoryStore
    providers?: { id: string; is_enabled: boolean }[]
    onSnapshot?: (snapshot: ConsoleChatSnapshot) => void
    script?: (
      signal: AbortSignal,
      index: number,
    ) => AsyncIterable<ConsoleResponsesEvent> | Iterable<ConsoleResponsesEvent>
  } = {},
) {
  const store = options.store ?? new MemoryStore()
  const state = {
    apiKeys: options.keys ?? [key()],
    models: options.models ?? [model()],
    providers: options.providers ?? [{ id: 'provider', is_enabled: true }],
  }
  const requests: { apiKey: string; request: ConsoleResponsesRequest; signal: AbortSignal }[] = []
  let clock = Date.parse('2026-01-01T00:00:00Z')
  let id = 0
  let catalogFailure: Error | null = null
  const catalog: ConsoleAdminCatalog = {
    read() {
      return catalogFailure ? Promise.reject(catalogFailure) : Promise.resolve(structuredClone(state))
    },
    revealKey() {
      return Promise.resolve('ephemeral-secret')
    },
  }
  const transport: ResponsesTransport = {
    async *stream(input) {
      const index = requests.length
      requests.push(input)
      if (options.script) yield* options.script(input.signal, index)
      else yield complete()
    },
  }
  const controller = new ConsoleChatController({
    store,
    catalog,
    transport,
    now: () => clock,
    createId: () => `id-${++id}`,
    onSnapshot: options.onSnapshot,
  })
  return {
    controller,
    store,
    state,
    requests,
    advance: () => {
      clock += 1000
    },
    failCatalog: (error: Error) => {
      catalogFailure = error
    },
  }
}

describe('selection and onboarding', () => {
  test('enabled models without an enabled target on an enabled service require configuration recovery', async () => {
    const h = harness({ models: [model('route-a', { targets: [] })] })
    await h.controller.start()
    expect(h.controller.snapshot.blocker).toBe('disabled-models')
    await h.controller.send('Cannot reach an upstream')
    expect(h.requests).toEqual([])
  })
  test('valid identities are independent of model availability and bindings constrain routes', async () => {
    const h = harness({
      keys: [
        key('valid', { model_ids: ['route-b'] }),
        key('disabled', { is_enabled: false }),
        key('expired', { expires_at: '2025-12-31 23:59:59' }),
      ],
      models: [model(), model('route-b', { is_enabled: false })],
    })
    await h.controller.start()
    expect(h.controller.snapshot.keyCandidates.map((item) => item.id)).toEqual(['valid'])
    expect(h.controller.snapshot.selectedKeyId).toBe('valid')
    expect(h.controller.snapshot.modelCandidates).toEqual([])
    expect(h.controller.snapshot.blocker).toBe('unavailable-keys')
    await h.controller.send('Cannot send yet')
    expect(h.requests).toEqual([])
  })
  test('remembered IDs must remain valid, unique candidates select, ambiguous candidates wait', async () => {
    const store = new MemoryStore()
    store.preferences = { apiKeyId: 'gone', modelId: 'gone' }
    const h = harness({ store, keys: [key(), key('key-b')], models: [model(), model('route-b')] })
    await h.controller.start()
    expect(h.controller.snapshot.selectedKeyId).toBeNull()
    h.controller.selectKey('key-a')
    expect(h.controller.snapshot.selectedModelId).toBeNull()
    h.controller.selectModel('route-b')
    await h.controller.send('hello')
    const reloaded = harness({ store, keys: h.state.apiKeys, models: h.state.models })
    await reloaded.controller.start()
    expect(reloaded.controller.snapshot.selectedKeyId).toBe('key-a')
    expect(reloaded.controller.snapshot.selectedModelId).toBe('route-b')
  })
  const blockers: [Parameters<typeof harness>[0], ConsoleChatBlocker][] = [
    [{ providers: [] }, 'no-services'],
    [{ providers: [{ id: 'p', is_enabled: false }], models: [] }, 'disabled-services'],
    [{ models: [], keys: [] }, 'no-models'],
    [{ models: [model('route-a', { is_enabled: false })], keys: [] }, 'disabled-models'],
    [{ keys: [] }, 'no-keys'],
    [{ keys: [key('disabled', { is_enabled: false })] }, 'unavailable-keys'],
  ]
  test.each(blockers)('configuration checks are ordered %#', async (options, blocker) => {
    const h = harness(options)
    await h.controller.start()
    expect(h.controller.snapshot.blocker).toBe(blocker)
    await h.controller.send('blocked')
    expect(h.requests.length).toBe(0)
  })
})

describe('request and presentation', () => {
  test('original image inputs survive refresh, full replay and regeneration under the same Key', async () => {
    const images = [
      { id: 'png', name: 'screen.png', mediaType: 'image/png' as const, dataUrl: 'data:image/png;base64,aGVsbG8=' },
      {
        id: 'webp',
        name: 'diagram.webp',
        mediaType: 'image/webp' as const,
        dataUrl: 'data:image/webp;base64,d29ybGQ=',
      },
    ]
    const h = harness({ models: [model('route-a', { supports_image_input: true })] })
    await h.controller.start()
    expect(await h.controller.send('Look at these', images)).toBe(true)
    const userInput = {
      role: 'user',
      content: [
        { type: 'input_text', text: 'Look at these' },
        { type: 'input_image', image_url: images[0].dataUrl },
        { type: 'input_image', image_url: images[1].dataUrl },
      ],
    }
    expect(h.requests[0].request.input).toEqual([userInput])
    const id = h.controller.snapshot.currentConversationId!
    const refreshed = harness({ store: h.store, models: h.state.models })
    await refreshed.controller.start()
    refreshed.controller.openConversation(id)
    expect(refreshed.controller.snapshot.historyHasImages).toBe(true)
    expect(refreshed.controller.snapshot.currentConversation!.messages[0]).toMatchObject({ images })
    await refreshed.controller.send('Continue')
    expect(refreshed.requests[0].request.input[0]).toEqual(userInput)
    await refreshed.controller.regenerate()
    expect(refreshed.requests[1].request.input[0]).toEqual(userInput)
    expect(refreshed.controller.snapshot.currentConversation!.apiKeyId).toBe('key-a')
    await refreshed.controller.delete(id)
    expect(refreshed.store.conversations).toEqual([])
  })
  test('current and historical images block incompatible routes without losing saved input', async () => {
    const image = {
      id: 'jpeg',
      name: 'photo.jpg',
      mediaType: 'image/jpeg' as const,
      dataUrl: 'data:image/jpeg;base64,aGVsbG8=',
    }
    const h = harness({ models: [model('route-a', { supports_image_input: true }), model('route-b')] })
    await h.controller.start()
    h.controller.selectModel('route-b')
    expect(await h.controller.send('Explain', [image])).toBe(false)
    expect(h.requests).toEqual([])
    expect(h.controller.snapshot.inputError?.code).toBe('CONSOLE_IMAGE_INPUT_UNSUPPORTED')
    h.controller.selectModel('route-a')
    expect(await h.controller.send('   ', [image])).toBe(false)
    await h.controller.send('Explain', [image])
    h.controller.selectModel('route-b')
    await h.controller.send('Continue')
    await h.controller.regenerate()
    expect(h.requests).toHaveLength(1)
    expect(h.controller.snapshot.currentConversation!.messages[0]).toMatchObject({ images: [image] })
  })
  test('attachment rejection and retry preserve original image bytes and the failed request effort', async () => {
    const image = {
      id: 'jpeg',
      name: 'photo.jpg',
      mediaType: 'image/jpeg' as const,
      dataUrl: 'data:image/jpeg;base64,aGVsbG8=',
    }
    const h = harness({
      models: [model('route-a', { supports_image_input: true })],
      script: function* (_, index) {
        if (index === 0)
          yield {
            type: 'response.failed',
            response: { error: { code: 'attachment_error', message: 'Attachment storage unavailable' } },
          }
        else yield complete('Accepted')
      },
    })
    await h.controller.start()
    h.controller.selectThinking('high')
    expect(await h.controller.send('Read the photo', [image])).toBe(false)
    expect(h.controller.snapshot.currentConversation!.messages[0]).toMatchObject({ images: [image] })
    h.controller.selectThinking('low')
    await h.controller.retry()
    expect(h.requests[1].request).toEqual(h.requests[0].request)
    expect(h.controller.snapshot.currentConversation!.messages.map((message) => message.role)).toEqual([
      'user',
      'assistant',
    ])
    expect(h.requests.every((request) => request.apiKey === 'ephemeral-secret')).toBe(true)
  })
  test('unsupported and temporary attachment references cannot become saved user images', async () => {
    const h = harness({ models: [model('route-a', { supports_image_input: true })] })
    await h.controller.start()
    expect(
      await h.controller.send('Read', [
        { id: 'bad', name: 'bad.png', mediaType: 'image/png', dataUrl: 'blob:temporary' },
      ]),
    ).toBe(false)
    expect(h.controller.snapshot.inputError?.code).toBe('CONSOLE_IMAGE_ATTACHMENT_INVALID')
    expect(h.controller.snapshot.conversations).toEqual([])
    expect(h.requests).toEqual([])
  })
  test.each([
    [null, 'default', { summary: 'auto' }],
    ['high', 'default', { summary: 'auto', effort: 'high' }],
    ['off', 'default', { summary: 'auto', effort: 'none' }],
    ['high', 'off', { summary: 'auto', effort: 'none' }],
    [null, 'low', { summary: 'auto', effort: 'low' }],
  ] as const)(
    'resolves selected reasoning without changing default label %#',
    async (defaultLevel, selection, reasoning) => {
      const h = harness({ models: [model('route-a', { default_thinking_level: defaultLevel })] })
      await h.controller.start()
      h.controller.selectThinking(selection)
      await h.controller.send('hello')
      expect(h.requests[0].request).toEqual({
        model: 'model-a',
        stream: true,
        input: [{ role: 'user', content: 'hello' }],
        reasoning,
      })
      expect(h.controller.snapshot.currentConversation?.messages[1]).toMatchObject({ thinkingLevel: selection })
      expect(JSON.stringify(h.store.conversations)).not.toContain('ephemeral-secret')
    },
  )
  test('completed output is authoritative, raw reasoning and markers replay without interpretation', async () => {
    const raw = [
      {
        type: 'reasoning',
        id: 'r',
        encrypted_content: 'opaque',
        summary: [{ type: 'summary_text', text: 'Summary<!--hidden-->' }],
        content: [{ text: 'Raw' }],
      },
      { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Final<!--machine-marker-->' }] },
      { type: 'custom_projection', payload: 'untouched' },
    ]
    const h = harness({
      script: function* () {
        yield { type: 'response.output_text.delta', delta: 'Incorrect draft' }
        yield complete('', { output: raw, usage: { input_tokens: 12 } })
      },
    })
    await h.controller.start()
    await h.controller.send('first')
    const answer = h.controller.snapshot.currentConversation!.messages[1]
    if (answer.role !== 'assistant') throw new Error('Expected assistant')
    expect(consoleAssistantContent(answer)).toEqual({ text: 'Final', thinking: 'Summary' })
    await h.controller.send('second')
    expect(h.requests[1].request.input).toEqual([
      { role: 'user', content: 'first' },
      ...raw,
      { role: 'user', content: 'second' },
    ])
  })
  test.each([
    [
      { input_tokens: 12, input_tokens_details: { cached_tokens: 5 } },
      { inputTokens: 7, outputTokens: 3 },
    ],
    [
      { input_tokens: 12, input_tokens_details: { cached_tokens: 0 } },
      { inputTokens: 12, outputTokens: 3 },
    ],
    [
      { input_tokens: 3, input_tokens_details: { cached_tokens: 9 } },
      { inputTokens: 0, outputTokens: 3 },
    ],
    [{ input_tokens: 12 }, { outputTokens: 3 }],
    [{ input_tokens_details: { cached_tokens: 5 } }, { outputTokens: 3 }],
    [{ input_tokens: 12, input_tokens_details: { cached_tokens: null } }, { outputTokens: 3 }],
  ])('persists net input only when both reported operands are known %#', async (usage, expected) => {
    const h = harness({
      script: function* () {
        yield complete('Metered answer', { usage: { ...usage, output_tokens: 3 } })
      },
    })
    await h.controller.start()
    await h.controller.send('hello')
    expect(
      h.controller.snapshot.currentConversation!.messages.find((message) => message.role === 'assistant')?.usage,
    ).toEqual(expected)
    const refreshed = harness({ store: h.store })
    await refreshed.controller.start()
    refreshed.controller.openConversation(h.controller.snapshot.currentConversationId)
    expect(
      refreshed.controller.snapshot.currentConversation!.messages.find((message) => message.role === 'assistant')
        ?.usage,
    ).toEqual(expected)
  })
  test('reasoning falls back to readable raw content and missing usage remains absent', async () => {
    const h = harness({
      script: function* () {
        yield complete('Raw result', {
          output: [{ type: 'reasoning', content: [{ type: 'reasoning_text', text: 'Readable<!--private-->' }] }],
        })
      },
    })
    await h.controller.start()
    await h.controller.send('hello')
    const answer = h.controller.snapshot.currentConversation!.messages[1]
    if (answer.role !== 'assistant') throw new Error('Expected assistant')
    expect(consoleAssistantContent(answer)).toEqual({ text: '', thinking: 'Readable' })
    expect(answer.usage).toBeUndefined()
    expect(consoleVisibleText('Visible<!--streaming hidden')).toBe('Visible')
    expect(consoleVisibleText('Core-projection-delimiter')).toBe('Core-projection-delimiter')
  })
  test('incomplete output remains replayable', async () => {
    const h = harness({
      script: function* () {
        yield complete('truncated', { status: 'incomplete' })
      },
    })
    await h.controller.start()
    await h.controller.send('first')
    expect(h.controller.snapshot.currentConversation!.messages[1]).toMatchObject({ status: 'incomplete' })
    await h.controller.send('next')
    expect(h.requests[1].request.input).toContainEqual({
      type: 'message',
      role: 'assistant',
      content: [{ type: 'output_text', text: 'truncated' }],
    })
  })
  test('answer metadata retains the requested client route rather than an upstream model selector', async () => {
    const h = harness({
      script: function* () {
        yield complete('answer', { model: 'upstream-provider-model' })
      },
    })
    await h.controller.start()
    await h.controller.send('hello')
    expect(h.requests[0].request.model).toBe('model-a')
    expect(h.controller.snapshot.currentConversation!.messages[1]).toMatchObject({ routeId: 'model-a' })
    expect(h.controller.snapshot.selectedModelId).toBe('route-a')
  })
})

describe('stream lifecycle and races', () => {
  test('reasoning items retain public identity, output order, per-item completion and authoritative fallback', async () => {
    const snapshots: ConsoleChatSnapshot[] = []
    const h = harness({
      onSnapshot: (snapshot) => snapshots.push(snapshot),
      script: function* () {
        yield {
          type: 'response.output_item.added',
          output_index: 0,
          item: { type: 'reasoning', id: 'first', status: 'in_progress' },
        }
        yield {
          type: 'response.reasoning_text.delta',
          output_index: 0,
          item_id: 'first',
          content_index: 0,
          delta: 'Readable original',
        }
        yield {
          type: 'response.reasoning_summary_text.delta',
          output_index: 0,
          item_id: 'first',
          summary_index: 0,
          delta: 'Draft summary',
        }
        yield {
          type: 'response.reasoning_summary_text.done',
          output_index: 0,
          item_id: 'first',
          summary_index: 0,
          text: 'Final summary',
        }
        yield {
          type: 'response.output_item.done',
          output_index: 0,
          item: { type: 'reasoning', id: 'first', summary: [{ type: 'summary_text', text: 'Final summary' }] },
        }
        yield {
          type: 'response.output_item.added',
          output_index: 2,
          item: { type: 'reasoning', id: 'second', status: 'in_progress' },
        }
        yield {
          type: 'response.reasoning_text.delta',
          output_index: 2,
          item_id: 'second',
          content_index: 0,
          delta: 'Second original<!--opaque-->',
        }
        yield complete('', {
          output: [
            { type: 'reasoning', id: 'first', summary: [{ type: 'summary_text', text: 'Authoritative summary' }] },
            {
              type: 'reasoning',
              id: 'second',
              content: [{ type: 'reasoning_text', text: 'Authoritative original' }],
              encrypted_content: 'never display',
            },
          ],
        })
      },
    })
    await h.controller.start()
    await h.controller.send('Reason')
    const generations = snapshots.flatMap((snapshot) => Object.values(snapshot.generations))
    expect(
      generations.some((generation) =>
        generation.activities.some(
          (activity) => activity.id === 'first' && activity.live && activity.text === 'Draft summary',
        ),
      ),
    ).toBe(true)
    expect(
      generations.some(
        (generation) =>
          generation.activities.length === 2 &&
          generation.activities[0].id === 'first' &&
          !generation.activities[0].live &&
          generation.activities[1].id === 'second' &&
          generation.activities[1].live,
      ),
    ).toBe(true)
    const answer = h.controller.snapshot.currentConversation!.messages[1]
    if (answer.role !== 'assistant') throw new Error('Expected assistant')
    expect(consoleReasoningActivities(answer).map(({ id, text, live }) => ({ id, text, live }))).toEqual([
      { id: 'first', text: 'Authoritative summary', live: false },
      { id: 'second', text: 'Authoritative original', live: false },
    ])
  })
  test.each(['stop', 'fail'] as const)(
    'interrupted reasoning preserves all readable items and partial answer after %s and refresh',
    async (ending) => {
      const entered = deferred<void>()
      const resume = deferred<void>()
      const h = harness({
        script: async function* () {
          yield { type: 'response.output_item.added', output_index: 0, item: { type: 'reasoning', id: 'r1' } }
          yield { type: 'response.reasoning_summary_text.delta', output_index: 0, item_id: 'r1', delta: 'First' }
          yield {
            type: 'response.output_item.done',
            output_index: 0,
            item: { type: 'reasoning', id: 'r1', summary: [{ text: 'First' }] },
          }
          yield { type: 'response.output_item.added', output_index: 1, item: { type: 'reasoning', id: 'r2' } }
          yield { type: 'response.reasoning_text.delta', output_index: 1, item_id: 'r2', delta: 'Second<!--private-->' }
          yield { type: 'response.output_text.delta', delta: 'Partial answer' }
          entered.resolve()
          await resume.promise
          yield {
            type: 'response.failed',
            response: { error: { message: 'Upstream rejected attachment', code: 'attachment_error' } },
          }
        },
      })
      await h.controller.start()
      const pending = h.controller.send('Explain')
      await entered.promise
      const id = h.controller.snapshot.currentConversationId!
      if (ending === 'stop') await h.controller.stop()
      resume.resolve()
      expect(await pending).toBe(false)
      const refreshed = harness({ store: h.store })
      await refreshed.controller.start()
      refreshed.controller.openConversation(id)
      const answer = refreshed.controller.snapshot.currentConversation!.messages[1]
      if (answer.role !== 'assistant') throw new Error('Expected assistant')
      expect(answer.status).toBe(ending === 'stop' ? 'stopped' : 'failed')
      expect(consoleAssistantContent(answer)).toEqual({ text: 'Partial answer', thinking: 'First\n\nSecond' })
      expect(consoleReasoningActivities(answer).map(({ id, live }) => ({ id, live }))).toEqual([
        { id: 'r1', live: false },
        { id: 'r2', live: false },
      ])
    },
  )
  test('published conversation snapshots remain stable when later messages and titles change', async () => {
    const h = harness()
    await h.controller.start()
    await h.controller.send('First question')
    const published = h.controller.snapshot
    await h.controller.send('Second question')
    await h.controller.rename(published.currentConversationId!, 'Renamed')
    expect(published.currentConversation?.title).toBe('First question')
    expect(published.currentConversation?.messages.map((message) => message.role)).toEqual(['user', 'assistant'])
    expect(h.controller.snapshot.currentConversation?.title).toBe('Renamed')
  })
  test('live checkpoints recover on refresh, stopping replays only visible text and ignores late completion', async () => {
    const delivered = deferred<void>()
    const resume = deferred<void>()
    const h = harness({
      script: async function* () {
        yield { type: 'response.reasoning_text.delta', delta: 'Raw reasoning' }
        yield { type: 'response.reasoning_summary_text.delta', delta: 'Summary' }
        yield { type: 'response.output_text.delta', delta: 'Partial<!--private-->' }
        delivered.resolve()
        await resume.promise
        yield complete('Late result')
      },
    })
    await h.controller.start()
    const pending = h.controller.send('first')
    const id = h.controller.snapshot.currentConversationId!
    expect(id).toBeTruthy()
    await delivered.promise
    expect(h.controller.snapshot.generations[id]).toMatchObject({
      text: 'Partial<!--private-->',
      summary: 'Summary',
      reasoning: 'Raw reasoning',
    })
    const refreshed = harness({ store: h.store })
    await refreshed.controller.start()
    refreshed.controller.openConversation(id)
    expect(refreshed.controller.snapshot.currentConversation!.messages[1]).toMatchObject({
      status: 'stopped',
      partialText: 'Partial',
      partialThinking: 'Summary',
    })
    await h.controller.send('forbidden concurrent')
    expect(h.requests.length).toBe(1)
    await h.controller.stop()
    expect(h.requests[0].signal.aborted).toBe(true)
    resume.resolve()
    await pending
    expect(h.controller.snapshot.currentConversation!.messages[1]).toMatchObject({
      status: 'stopped',
      partialText: 'Partial',
    })
    await refreshed.controller.send('next')
    expect(refreshed.requests[0].request.input).toEqual([
      { role: 'user', content: 'first' },
      { role: 'assistant', content: 'Partial' },
      { role: 'user', content: 'next' },
    ])
  })
  test('different conversations generate concurrently and retain independent model and effort selections', async () => {
    const resume = deferred<void>()
    const entered = [deferred<void>(), deferred<void>()]
    const h = harness({
      models: [model(), model('route-b')],
      script: async function* (_, index) {
        entered[index].resolve()
        await resume.promise
        yield complete(`answer ${index}`)
      },
    })
    await h.controller.start()
    h.controller.selectModel('route-a')
    h.controller.selectThinking('high')
    const first = h.controller.send('first')
    const firstId = h.controller.snapshot.currentConversationId!
    await entered[0].promise
    h.controller.newConversation()
    h.controller.selectModel('route-b')
    h.controller.selectThinking('low')
    const second = h.controller.send('second')
    await entered[1].promise
    expect(Object.keys(h.controller.snapshot.generations)).toHaveLength(2)
    h.controller.openConversation(firstId)
    expect(h.controller.snapshot.selectedModelId).toBe('route-a')
    expect(h.controller.snapshot.thinkingSelection).toBe('high')
    resume.resolve()
    await Promise.all([first, second])
    expect(h.controller.snapshot.generations).toEqual({})
  })
  test.each(['delete', 'clear'] as const)('deleting live data prevents resurrection after %s', async (operation) => {
    const entered = deferred<void>()
    const late = deferred<void>()
    const h = harness({
      script: async function* () {
        entered.resolve()
        await late.promise
        yield complete('must not resurrect')
      },
    })
    await h.controller.start()
    const pending = h.controller.send('hello')
    const id = h.controller.snapshot.currentConversationId!
    await entered.promise
    if (operation === 'delete') await h.controller.delete(id)
    else await h.controller.clearAll()
    expect(h.requests[0].signal.aborted).toBe(true)
    late.resolve()
    await pending
    expect(h.controller.snapshot.conversations).toEqual([])
    expect(h.store.conversations).toEqual([])
  })
  test('serialized checkpoints cannot overwrite terminal output or deletion', async () => {
    const gate = deferred<void>()
    const h = harness()
    await h.controller.start()
    h.store.gate = gate.promise
    const pending = h.controller.send('hello')
    const id = h.controller.snapshot.currentConversationId!
    const deleting = h.controller.delete(id)
    gate.resolve()
    await Promise.all([pending, deleting])
    expect(h.store.conversations).toEqual([])
    expect(h.requests).toEqual([])
  })
  test('streaming saves finish before the authoritative terminal save', async () => {
    const h = harness({
      script: function* () {
        yield { type: 'response.output_text.delta', delta: 'streamed draft' }
        yield complete('authoritative result')
      },
    })
    await h.controller.start()
    await h.controller.send('hello')
    const reloaded = harness({ store: h.store })
    await reloaded.controller.start()
    reloaded.controller.openConversation(h.controller.snapshot.currentConversationId)
    const answer = reloaded.controller.snapshot.currentConversation!.messages[1]
    if (answer.role !== 'assistant') throw new Error('Expected assistant')
    expect(consoleAssistantContent(answer).text).toBe('authoritative result')
  })
  test('an initial store failure preserves the draft boundary and never starts upstream or claims saved history', async () => {
    const h = harness()
    await h.controller.start()
    const failure = new Error('disk full')
    h.store.failure = failure
    expect(await h.controller.send('hello')).toBe(false)
    expect(h.controller.snapshot.storageError).toBe(failure)
    expect(h.requests).toEqual([])
    expect(h.controller.snapshot.currentConversation).toBeNull()
    expect(h.store.conversations).toEqual([])
    h.store.failure = null
    expect(await h.controller.send('hello')).toBe(true)
    expect(h.controller.snapshot.storageError).toBeNull()
  })
  test('unsaved replacement generation cannot discard the previous authoritative answer', async () => {
    const h = harness({ models: [model('route-a', { supports_image_input: true })] })
    await h.controller.start()
    await h.controller.send('Saved question', [
      { id: 'original', name: 'original.png', mediaType: 'image/png', dataUrl: 'data:image/png;base64,aGVsbG8=' },
    ])
    const saved = structuredClone(h.controller.snapshot.currentConversation!.messages)
    h.store.failure = new Error('Storage quota exhausted')
    await h.controller.regenerate()
    expect(h.requests).toHaveLength(1)
    expect(h.controller.snapshot.currentConversation!.messages).toEqual(saved)
    expect(h.store.conversations[0].messages).toEqual(saved)
    expect(await h.controller.send('Unsaved next question')).toBe(false)
    expect(h.controller.snapshot.currentConversation!.messages).toEqual(saved)
    expect(h.requests).toHaveLength(1)
  })
  test('stopAll interrupts every conversation without depending on iterator cooperation', async () => {
    const resume = deferred<void>()
    const entered = [deferred<void>(), deferred<void>()]
    const h = harness({
      script: async function* (_, index) {
        yield { type: 'response.output_text.delta', delta: `partial ${index}` }
        entered[index].resolve()
        await resume.promise
        yield complete('late')
      },
    })
    await h.controller.start()
    const first = h.controller.send('first')
    await entered[0].promise
    h.controller.newConversation()
    const second = h.controller.send('second')
    await entered[1].promise
    await h.controller.stopAll()
    expect(h.requests.every((request) => request.signal.aborted)).toBe(true)
    expect(h.controller.snapshot.generations).toEqual({})
    expect(h.store.conversations.map((conversation) => conversation.messages[1])).toMatchObject([
      { status: 'stopped', partialText: 'partial 0' },
      { status: 'stopped', partialText: 'partial 1' },
    ])
    resume.resolve()
    await Promise.all([first, second])
  })
  test('load and catalog failures remain explicit and do not create conversations', async () => {
    class UnreadableStore extends MemoryStore {
      override load(): Promise<Awaited<ReturnType<ConversationStore['load']>>> {
        return Promise.reject(new Error('History is unavailable'))
      }
    }
    const h = harness({ store: new UnreadableStore() })
    await h.controller.start()
    expect(h.controller.snapshot.loadError).toBeInstanceOf(Error)
    expect(h.controller.snapshot.storageError).toBeInstanceOf(Error)
    await h.controller.send('blocked')
    expect(h.requests).toEqual([])
    const catalog = harness()
    catalog.failCatalog(new Error('Catalog unavailable'))
    await catalog.controller.start()
    expect(catalog.controller.snapshot.catalogError).toBeInstanceOf(Error)
    expect(catalog.controller.snapshot.loading).toBe(false)
    expect(catalog.controller.snapshot.blocker).toBeNull()
  })
  test('manual history retry recovers a temporary local storage failure', async () => {
    class RecoverableStore extends MemoryStore {
      available = false
      override load() {
        if (!this.available) return Promise.reject(new Error('History is temporarily unavailable'))
        return super.load()
      }
    }
    const store = new RecoverableStore()
    const h = harness({ store })
    await h.controller.start()
    store.available = true
    await h.controller.start()
    await h.controller.send('After storage recovery')
    expect(h.controller.snapshot.loadError).toBeNull()
    expect(h.requests[0]?.request.input).toEqual([{ role: 'user', content: 'After storage recovery' }])
  })
})

describe('failures, authorization and local history', () => {
  test('refusal text is visible, searchable and preserved for raw replay', async () => {
    const output = [
      {
        type: 'message',
        role: 'assistant',
        content: [{ type: 'refusal', refusal: 'Cannot assist with that request.' }],
      },
    ]
    const h = harness({
      script: function* () {
        yield complete('', { output })
      },
    })
    await h.controller.start()
    await h.controller.send('question')
    const answer = h.controller.snapshot.currentConversation!.messages[1]
    if (answer.role !== 'assistant') throw new Error('Expected assistant answer')
    expect(consoleAssistantContent(answer).text).toBe('Cannot assist with that request.')
    expect(h.controller.findConversations('cannot assist')).toHaveLength(1)
    await h.controller.send('next')
    expect(h.requests[1].request.input).toContainEqual(output[0])
  })
  test('stopping a refusal delta keeps its visible text for the next turn', async () => {
    const ready = deferred<void>()
    const h = harness({
      script: async function* (signal, index) {
        if (index) {
          yield complete()
          return
        }
        yield { type: 'response.refusal.delta', delta: 'Cannot assist.' }
        ready.resolve()
        await new Promise<void>((resolve) => signal.addEventListener('abort', () => resolve(), { once: true }))
      },
    })
    await h.controller.start()
    const sending = h.controller.send('question')
    await ready.promise
    await h.controller.stop()
    await sending
    await h.controller.send('next')
    expect(h.requests[1].request.input).toContainEqual({ role: 'assistant', content: 'Cannot assist.' })
  })
  test('an expired Key publishes the read-only state when Send is rejected', async () => {
    const published: ConsoleChatSnapshot[] = []
    const h = harness({
      keys: [key('key-a', { expires_at: '2026-01-01T00:00:02Z' })],
      onSnapshot: (snapshot) => published.push(snapshot),
    })
    await h.controller.start()
    await h.controller.send('before expiry')
    h.advance()
    h.advance()
    await h.controller.send('after expiry')
    expect(h.requests).toHaveLength(1)
    expect(published.at(-1)!.readOnlyReason).toBe('expired')
  })
  test('a removed failed model disables retry while regeneration uses its replacement', async () => {
    const h = harness({
      models: [model(), model('route-b')],
      script: function* (_, index) {
        if (!index) throw new Error('Original model unavailable')
        yield complete('replacement answer')
      },
    })
    await h.controller.start()
    h.controller.selectModel('route-a')
    await h.controller.send('question')
    h.state.models = [model('route-b')]
    await h.controller.refreshCatalog()
    expect(h.controller.snapshot.selectedModelId).toBe('route-b')
    expect(h.controller.snapshot.retryModelAvailable).toBe(false)
    await h.controller.regenerate()
    expect(h.requests[1].request.model).toBe('model-b')
  })
  test.each(['delete', 'clearAll'] as const)('failed %s preserves history and can be retried', async (command) => {
    class FailingDeletionStore extends MemoryStore {
      unavailable = true
      override deleteConversation(id: string) {
        return this.unavailable ? Promise.reject(new Error('Deletion unavailable')) : super.deleteConversation(id)
      }
      override clearConversations() {
        return this.unavailable ? Promise.reject(new Error('Deletion unavailable')) : super.clearConversations()
      }
    }
    const store = new FailingDeletionStore()
    const h = harness({ store })
    await h.controller.start()
    await h.controller.send('Keep me until deletion succeeds')
    const id = h.controller.snapshot.currentConversationId!
    const remove = () => (command === 'delete' ? h.controller.delete(id) : h.controller.clearAll())
    await rejects(remove, /Deletion unavailable/)
    expect(h.controller.snapshot.currentConversationId).toBe(id)
    expect(h.controller.snapshot.conversations[0].title).toBe('Keep me until deletion succeeds')
    expect((await store.load()).conversations[0].id).toBe(id)
    store.unavailable = false
    await remove()
    expect(h.controller.snapshot.conversations).toEqual([])
    expect((await store.load()).conversations).toEqual([])
  })
  test('a failed rename retains the saved title and reports failure', async () => {
    const h = harness()
    await h.controller.start()
    await h.controller.send('Original title')
    h.store.failure = new Error('Storage unavailable')
    await rejects(
      () => h.controller.rename(h.controller.snapshot.currentConversationId!, 'New title'),
      /Storage unavailable/,
    )
    expect(h.controller.snapshot.currentConversation!.title).toBe('Original title')
    expect((await h.store.load()).conversations[0].title).toBe('Original title')
  })
  test.each(['delete', 'clearAll'] as const)('edits cannot resurrect history while %s is pending', async (command) => {
    const entered = deferred<void>()
    const release = deferred<void>()
    class PendingDeletionStore extends MemoryStore {
      override async deleteConversation(id: string) {
        entered.resolve()
        await release.promise
        await super.deleteConversation(id)
      }
      override async clearConversations() {
        entered.resolve()
        await release.promise
        await super.clearConversations()
      }
    }
    const store = new PendingDeletionStore()
    const h = harness({ store, models: [model(), model('route-b')] })
    await h.controller.start()
    h.controller.selectModel('route-a')
    await h.controller.send('Original title')
    const id = h.controller.snapshot.currentConversationId!
    const deleting = command === 'delete' ? h.controller.delete(id) : h.controller.clearAll()
    await entered.promise
    h.controller.selectModel('route-b')
    h.controller.selectThinking('high')
    const renaming = h.controller.rename(id, 'Do not resurrect')
    release.resolve()
    await Promise.all([deleting, renaming])
    expect((await store.load()).conversations).toEqual([])
  })
  test.each([true, false])('stream checkpoints keep committed titles when rename fails=%s', async (fails) => {
    const renameEntered = deferred<void>()
    const renameRelease = deferred<void>()
    const streamReady = deferred<void>()
    const streamMore = deferred<void>()
    const checkpointPublished = deferred<void>()
    const checkpointSaved = deferred<void>()
    const finish = deferred<void>()
    class PendingRenameStore extends MemoryStore {
      override async saveConversation(conversation: ConsoleConversation) {
        const last = conversation.messages.at(-1)
        if (
          conversation.title === 'Proposed title' &&
          last?.role === 'assistant' &&
          last.status === 'stopped' &&
          !conversation.messages.some(
            (message) => message.role === 'assistant' && message.partialText?.includes('while rename'),
          )
        ) {
          renameEntered.resolve()
          await renameRelease.promise
          if (fails) throw new Error('Rename failed')
        }
        await super.saveConversation(conversation)
        if (
          conversation.messages.some(
            (message) => message.role === 'assistant' && message.partialText?.includes('while rename'),
          )
        )
          checkpointSaved.resolve()
      }
    }
    const store = new PendingRenameStore()
    const h = harness({
      store,
      onSnapshot: (snapshot) => {
        if (Object.values(snapshot.generations).some((generation) => generation.text.includes('while rename')))
          checkpointPublished.resolve()
      },
      script: async function* () {
        yield { type: 'response.output_text.delta', delta: 'First ' }
        streamReady.resolve()
        await streamMore.promise
        yield { type: 'response.output_text.delta', delta: 'while rename' }
        await finish.promise
        yield complete('Completed')
      },
    })
    await h.controller.start()
    const sending = h.controller.send('Original title')
    await streamReady.promise
    const renaming = h.controller.rename(h.controller.snapshot.currentConversationId!, 'Proposed title')
    await renameEntered.promise
    streamMore.resolve()
    await checkpointPublished.promise
    renameRelease.resolve()
    if (fails) await rejects(renaming, /Rename failed/)
    else await renaming
    await checkpointSaved.promise
    try {
      expect((await store.load()).conversations[0].title).toBe(fails ? 'Original title' : 'Proposed title')
    } finally {
      finish.resolve()
      await sending
    }
  })
  test('unavailable catalog does not label a stored Key as deleted', async () => {
    const original = harness()
    await original.controller.start()
    await original.controller.send('Persisted history')
    const h = harness({ store: original.store })
    h.failCatalog(new Error('Directory unavailable'))
    await h.controller.start()
    h.controller.openConversation(original.controller.snapshot.currentConversationId)
    expect(h.controller.snapshot.catalogError).toBeInstanceOf(Error)
    expect(h.controller.snapshot.readOnlyReason).toBeNull()
    expect(h.controller.readOnlyReasonFor(original.controller.snapshot.currentConversationId!)).toBeNull()
  })
  test('manual retry retains original history and resolved reasoning after selection changes and refresh', async () => {
    const h = harness({
      models: [model('route-a', { default_thinking_level: 'high' }), model('route-b')],
      script: function* (_, index) {
        if (index === 0) throw Object.assign(new Error('Try later'), { status: 429, code: 'rate_limit' })
        yield complete('retry result')
      },
    })
    await h.controller.start()
    h.controller.selectModel('route-a')
    await h.controller.send('question')
    expect(h.requests.length).toBe(1)
    expect(h.controller.snapshot.currentConversation!.messages[1]).toMatchObject({
      status: 'failed',
      error: { message: 'Try later' },
    })
    h.controller.selectModel('route-b')
    h.controller.selectThinking('low')
    await h.controller.rename(h.controller.snapshot.currentConversationId!, 'Saved question')
    h.state.models[0].default_thinking_level = 'off'
    await h.controller.refreshCatalog()
    const retry = harness({ store: h.store, models: h.state.models })
    await retry.controller.start()
    retry.controller.openConversation(h.controller.snapshot.currentConversationId)
    await retry.controller.retry()
    expect(retry.requests[0].request).toEqual(h.requests[0].request)
    expect(retry.controller.snapshot.currentConversation!.messages).toHaveLength(2)
    await retry.controller.regenerate()
    expect(retry.requests[1].request).toEqual({
      model: 'model-b',
      stream: true,
      input: [{ role: 'user', content: 'question' }],
      reasoning: { summary: 'auto', effort: 'low' },
    })
    expect(retry.controller.snapshot.currentConversation!.messages).toHaveLength(2)
  })
  test('failed errors stay out of later replay and truncated streams fail without automatic retry', async () => {
    const h = harness({
      script: function* (_, index) {
        if (index === 0) {
          yield { type: 'response.output_text.delta', delta: 'discard' }
          return
        }
        yield complete()
      },
    })
    await h.controller.start()
    await h.controller.send('first')
    expect(h.requests.length).toBe(1)
    expect(h.controller.snapshot.currentConversation!.messages[1]).toMatchObject({ status: 'failed' })
    await h.controller.send('second')
    expect(h.requests[1].request.input).toEqual([
      { role: 'user', content: 'first' },
      { role: 'user', content: 'second' },
    ])
  })
  test.each(['deleted', 'disabled', 'expired'] as const)('locked Key becomes read-only when %s', async (reason) => {
    const h = harness({ keys: [key(), key('key-b')] })
    await h.controller.start()
    h.controller.selectKey('key-a')
    await h.controller.send('hello')
    h.controller.selectKey('key-b')
    expect(h.controller.snapshot.selectedKeyId).toBe('key-a')
    if (reason === 'deleted') h.state.apiKeys = []
    if (reason === 'disabled') h.state.apiKeys[0].is_enabled = false
    if (reason === 'expired') h.state.apiKeys[0].expires_at = '2026-01-01 00:00:00'
    await h.controller.refreshCatalog()
    expect(h.controller.snapshot.readOnlyReason).toBe(reason)
    await h.controller.send('blocked')
    await h.controller.retry()
    await h.controller.regenerate()
    expect(h.requests.length).toBe(1)
  })
  test('clock expiry and unavailable models do not confuse identity validity', async () => {
    const h = harness({ keys: [key('key-a', { expires_at: '2026-01-01 00:00:01' })] })
    await h.controller.start()
    await h.controller.send('hello')
    h.state.models = []
    await h.controller.refreshCatalog()
    expect(h.controller.snapshot.readOnlyReason).toBeNull()
    expect(h.controller.snapshot.selectedModelId).toBeNull()
    h.advance()
    expect(h.controller.snapshot.readOnlyReason).toBe('expired')
  })
  test('authentication failures refresh catalog but keep the gateway error if refresh fails', async () => {
    const h = harness({
      script: () => {
        h.state.apiKeys[0].is_enabled = false
        throw Object.assign(new Error('Original gateway failure'), { status: 401, code: 'invalid_api_key' })
      },
    })
    await h.controller.start()
    await h.controller.send('hello')
    expect(h.controller.snapshot.readOnlyReason).toBe('disabled')
    const failed = harness({
      script: () => {
        failed.failCatalog(new Error('Admin session expired'))
        throw Object.assign(new Error('Original failure'), { status: 401 })
      },
    })
    await failed.controller.start()
    await failed.controller.send('hello')
    expect(failed.controller.snapshot.catalogError).toBeInstanceOf(Error)
    expect(failed.controller.snapshot.currentConversation!.messages[1]).toMatchObject({
      error: { message: 'Original failure' },
    })
  })
  test.each(['STRAVIA_AUTH_ERROR', 'STRAVIA_FORBIDDEN', 'STRAVIA_NOT_FOUND'])(
    'gateway stream code %s refreshes identity and model state',
    async (code) => {
      const h = harness({
        models: [model(), model('route-b')],
        script: function* () {
          if (code === 'STRAVIA_AUTH_ERROR') h.state.apiKeys[0].is_enabled = false
          else h.state.models[0].is_enabled = false
          yield {
            type: 'response.failed',
            response: { status: 'failed', error: { code, message: 'Gateway rejected the request' } },
          }
        },
      })
      await h.controller.start()
      h.controller.selectModel('route-a')
      await h.controller.send('Refresh rejected selection')
      expect(h.controller.snapshot.currentConversation?.messages[1]).toMatchObject({
        status: 'failed',
        error: { code },
      })
      if (code === 'STRAVIA_AUTH_ERROR') expect(h.controller.snapshot.readOnlyReason).toBe('disabled')
      else {
        expect(h.controller.snapshot.readOnlyReason).toBeNull()
        expect(h.controller.snapshot.selectedModelId).toBe('route-b')
      }
    },
  )
  test('titles truncate graphemes, activity sorts, body searches, rename and missing addresses work', async () => {
    const h = harness()
    await h.controller.start()
    await h.controller.send(`${'👨‍👩‍👧‍👦'.repeat(41)}\nsecond line`)
    const first = h.controller.snapshot.currentConversationId!
    expect(h.controller.snapshot.currentConversation!.title).toBe('👨‍👩‍👧‍👦'.repeat(40))
    h.advance()
    h.controller.newConversation()
    await h.controller.send('Searchable body')
    const second = h.controller.snapshot.currentConversationId!
    expect(h.controller.snapshot.conversations.map((item) => item.id)).toEqual([second, first])
    await h.controller.rename(first, ' Renamed ')
    expect(h.controller.findConversations('renamed').map((item) => item.id)).toEqual([first])
    expect(h.controller.findConversations('SEARCHABLE').map((item) => item.id)).toEqual([second])
    h.controller.openConversation('missing')
    expect(h.controller.snapshot.missingConversation).toBe(true)
    await h.controller.send('must not create')
    expect(h.controller.snapshot.conversations).toHaveLength(2)
    await h.controller.delete(first)
    await h.controller.clearAll()
    expect(h.store.conversations).toEqual([])
  })
})

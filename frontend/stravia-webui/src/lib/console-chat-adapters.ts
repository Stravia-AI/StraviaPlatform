import { createParser } from 'eventsource-parser'
import { admin } from '$lib/admin-client'
import { apiBase } from '$lib/auth'
import type {
  ConsoleAdminCatalog,
  ConsoleChatError,
  ConsoleChatPreferences,
  ConsoleConversation,
  ConsoleResponsesEvent,
  ConversationStore,
  ResponsesTransport,
} from '$lib/console-chat-types'

const DATABASE_NAME = 'stravia-console-chat'
const DATABASE_VERSION = 2
const CONVERSATIONS = 'conversations'
const PREFERENCES = 'preferences'

export class IndexedDbConversationStore implements ConversationStore {
  #database: Promise<IDBDatabase> | undefined

  #open(): Promise<IDBDatabase> {
    if (!this.#database) {
      const { promise, resolve, reject } = Promise.withResolvers<IDBDatabase>()
      const request = indexedDB.open(DATABASE_NAME, DATABASE_VERSION)
      let blocked = false
      request.onupgradeneeded = (event) => {
        if (event.oldVersion === 0) {
          request.result.createObjectStore(CONVERSATIONS, { keyPath: 'id' })
          request.result.createObjectStore(PREFERENCES)
          return
        }
        // v1 只保存总输入，缺少缓存依据；不能将旧值冒充净输入或补零。
        const cursor = request.transaction!.objectStore(CONVERSATIONS).openCursor()
        cursor.onsuccess = () => {
          const current = cursor.result
          if (!current) return
          const conversation = current.value as ConsoleConversation
          let changed = false
          for (const message of conversation.messages) {
            if (message.role === 'assistant' && message.usage && 'inputTokens' in message.usage) {
              delete message.usage.inputTokens
              changed = true
            }
          }
          if (changed) current.update(conversation)
          current.continue()
        }
      }
      request.onerror = () => reject(request.error)
      request.onblocked = () => {
        blocked = true
        reject(new Error('Console chat storage upgrade is blocked by another window'))
      }
      request.onsuccess = () => {
        const database = request.result
        if (blocked) {
          database.close()
          return
        }
        database.onversionchange = () => {
          database.close()
          this.#database = undefined
        }
        resolve(database)
      }
      this.#database = promise.catch((error: unknown) => {
        this.#database = undefined
        throw error
      })
    }
    return this.#database
  }

  async load(): Promise<{ conversations: ConsoleConversation[]; preferences: ConsoleChatPreferences }> {
    const database = await this.#open()
    const { promise, resolve, reject } = Promise.withResolvers<{
      conversations: ConsoleConversation[]
      preferences: ConsoleChatPreferences
    }>()
    const transaction = database.transaction([CONVERSATIONS, PREFERENCES], 'readonly')
    const conversations = transaction.objectStore(CONVERSATIONS).getAll()
    const preferences = transaction.objectStore(PREFERENCES).get('selection')
    transaction.oncomplete = () =>
      resolve({
        conversations: conversations.result as ConsoleConversation[],
        preferences: (preferences.result as ConsoleChatPreferences | undefined) ?? {},
      })
    transaction.onabort = () => reject(transaction.error ?? new Error('Console chat storage read aborted'))
    transaction.onerror = () => reject(transaction.error)
    return promise
  }

  async #write(store: string, operation: (store: IDBObjectStore) => void): Promise<void> {
    const database = await this.#open()
    const { promise, resolve, reject } = Promise.withResolvers<void>()
    const transaction = database.transaction(store, 'readwrite')
    transaction.oncomplete = () => resolve()
    transaction.onabort = () => reject(transaction.error ?? new Error('Console chat storage write aborted'))
    transaction.onerror = () => reject(transaction.error)
    operation(transaction.objectStore(store))
    return promise
  }

  saveConversation(conversation: ConsoleConversation): Promise<void> {
    const snapshot = structuredClone(conversation)
    return this.#write(CONVERSATIONS, (store) => store.put(snapshot))
  }

  savePreferences(preferences: ConsoleChatPreferences): Promise<void> {
    const snapshot = { ...preferences }
    return this.#write(PREFERENCES, (store) => store.put(snapshot, 'selection'))
  }

  deleteConversation(id: string): Promise<void> {
    return this.#write(CONVERSATIONS, (store) => store.delete(id))
  }

  clearConversations(): Promise<void> {
    return this.#write(CONVERSATIONS, (store) => store.clear())
  }
}

export const consoleAdminCatalog: ConsoleAdminCatalog = {
  async read() {
    const [apiKeys, models, providers] = await Promise.all([
      admin.apiKeys.list(),
      admin.models.list(),
      admin.providers.list(),
    ])
    return {
      apiKeys: apiKeys.map((value) => {
        const key = { ...value }
        Reflect.deleteProperty(key, 'key')
        return key
      }),
      models,
      providers: providers.map(({ id, is_enabled }) => ({ id, is_enabled })),
    }
  },
  async revealKey(id) {
    // 管理 API 已提供明文列表；每次发送才读取，不把凭据放入控制器或本地存储。
    const key = (await admin.apiKeys.list()).find((candidate) => candidate.id === id)
    if (!key) throw Object.assign(new Error('API Key no longer exists'), { status: 401 })
    return key.key
  },
}

function responseError(value: unknown, status: number): Error & ConsoleChatError {
  const payload = value && typeof value === 'object' ? (value as Record<string, unknown>) : {}
  const nested =
    payload.error && typeof payload.error === 'object' ? (payload.error as Record<string, unknown>) : payload
  const message =
    typeof nested.message === 'string'
      ? nested.message
      : typeof payload.error === 'string'
        ? payload.error
        : `HTTP ${status}`
  return Object.assign(new Error(message), {
    status,
    ...(typeof nested.code === 'string' ? { code: nested.code } : {}),
    ...(nested.params && typeof nested.params === 'object' ? { params: nested.params as Record<string, unknown> } : {}),
  })
}

export const fetchResponsesTransport: ResponsesTransport = {
  async *stream({ apiKey, request, signal }) {
    const endpoint = `${(await apiBase()).replace(/\/api\/v1$/, '')}/v1/responses`
    // 推理身份独立于管理会话。禁止附带管理 Cookie 或使用 authenticatedFetch。
    const response = await fetch(endpoint, {
      method: 'POST',
      credentials: 'omit',
      headers: { Authorization: `Bearer ${apiKey}`, 'Content-Type': 'application/json', Accept: 'text/event-stream' },
      body: JSON.stringify(request),
      signal,
    })
    if (!response.ok) {
      const text = await response.text()
      let payload: unknown
      try {
        payload = JSON.parse(text)
      } catch {
        payload = { message: text || `HTTP ${response.status}` }
      }
      throw responseError(payload, response.status)
    }
    if (!response.body) throw new Error('Responses stream has no body')

    const events: ConsoleResponsesEvent[] = []
    const parser = createParser({
      onEvent(event) {
        if (!event.data || event.data === '[DONE]') return
        const payload: unknown = JSON.parse(event.data)
        if (!payload || typeof payload !== 'object') throw new Error('Invalid Responses stream event')
        const parsed = payload as ConsoleResponsesEvent
        const type = parsed.type ?? event.event
        if (typeof type !== 'string') throw new Error('Responses stream event has no type')
        events.push({ ...parsed, type })
      },
    })
    const reader = response.body.pipeThrough(new TextDecoderStream()).getReader()
    try {
      while (!signal.aborted) {
        const { done, value } = await reader.read()
        if (done) {
          parser.reset({ consume: true })
          for (const event of events.splice(0)) yield event
          return
        }
        parser.feed(value)
        for (const event of events.splice(0)) yield event
      }
      signal.throwIfAborted()
    } finally {
      // 终态收口或用户停止都会关闭响应体，确保网关不继续执行已经离开的请求。
      try {
        await reader.cancel()
      } finally {
        reader.releaseLock()
      }
    }
  },
}

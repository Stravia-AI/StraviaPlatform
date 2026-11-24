import { createContext } from 'svelte'
import { admin } from '#lib/admin-client.js'
import { ConsoleChatController } from '#lib/console-chat.js'
import { consoleAdminCatalog, fetchResponsesTransport, IndexedDbConversationStore } from '#lib/console-chat-adapters.js'
import { getLocale } from '#lib/paraglide/runtime.js'
import type {
  ConsoleChatControllerOptions,
  ConsoleChatSnapshot,
  ConsoleImageAttachment,
  ConsoleThinkingSelection,
} from '#lib/console-chat-types.js'

export class ConsoleChat {
  snapshot: ConsoleChatSnapshot
  readonly #controller: ConsoleChatController

  constructor(options: Omit<ConsoleChatControllerOptions, 'onSnapshot'>) {
    this.#controller = new ConsoleChatController({
      ...options,
      onSnapshot: (snapshot) => {
        this.snapshot = snapshot
      },
    })
    this.snapshot = $state.raw(this.#controller.snapshot)
  }

  start = (): Promise<void> => this.#controller.start()
  newConversation = (): void => this.#controller.newConversation()
  openConversation = (id: string | null): void => this.#controller.openConversation(id)
  selectKey = (id: string | null): void => this.#controller.selectKey(id)
  selectModel = (id: string | null): void => this.#controller.selectModel(id)
  selectThinking = (selection: ConsoleThinkingSelection): void => this.#controller.selectThinking(selection)
  send = (text: string, images?: ConsoleImageAttachment[]): Promise<boolean> => this.#controller.send(text, images)
  stop = (id?: string): Promise<void> => this.#controller.stop(id)
  stopAll = (): Promise<void> => this.#controller.stopAll()
  retry = (): Promise<void> => this.#controller.retry()
  regenerate = (): Promise<void> => this.#controller.regenerate()
  edit = (messageId: string, text: string): Promise<boolean> => this.#controller.edit(messageId, text)
  rename = (id: string, title: string): Promise<void> => this.#controller.rename(id, title)
  delete = (id: string): Promise<void> => this.#controller.delete(id)
  clearAll = (): Promise<void> => this.#controller.clearAll()
  refreshCatalog = (): Promise<void> => this.#controller.refreshCatalog()
  refreshEligibility = (): void => this.#controller.refreshEligibility()
  findConversations = (query: string) => this.#controller.findConversations(query)
  readOnlyReasonFor = (id: string) => this.#controller.readOnlyReasonFor(id)
}

export const [getConsoleChat, setConsoleChat] = createContext<ConsoleChat>()

export function createConsoleChat(): ConsoleChat {
  return new ConsoleChat({
    store: new IndexedDbConversationStore(),
    transport: fetchResponsesTransport,
    catalog: consoleAdminCatalog,
    generateTitle: async ({ apiKeyId, userMessage, assistantMessage }) => {
      try {
        const result = await admin.platformLlm.generateConversationTitle({
          api_key_id: apiKeyId,
          user_message: userMessage,
          assistant_message: assistantMessage ?? null,
          locale: getLocale(),
        })
        return result.title
      } catch {
        return null
      }
    },
  })
}

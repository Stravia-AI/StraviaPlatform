import { createContext } from 'svelte'
import { ConsoleChatController } from '$lib/console-chat'
import { consoleAdminCatalog, fetchResponsesTransport, IndexedDbConversationStore } from '$lib/console-chat-adapters'
import type {
  ConsoleChatControllerOptions,
  ConsoleChatSnapshot,
  ConsoleThinkingSelection,
} from '$lib/console-chat-types'

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
  send = (text: string): Promise<void> => this.#controller.send(text)
  stop = (id?: string): Promise<void> => this.#controller.stop(id)
  stopAll = (): Promise<void> => this.#controller.stopAll()
  retry = (): Promise<void> => this.#controller.retry()
  regenerate = (): Promise<void> => this.#controller.regenerate()
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
  })
}

import { onDestroy, onMount } from 'svelte'
import type { QueryClient } from '@tanstack/svelte-query'
import {
  ProviderAllowanceReadController,
  type ProviderAllowanceReadApi,
  type ProviderAllowanceReadSnapshot,
} from './provider-allowance-read'

/** 查询与操作语义在 controller；此壳只连接视图生命周期并镜像只读快照。 */
export class ProviderAllowanceRead {
  #state = $state.raw<ProviderAllowanceReadSnapshot>()
  readonly #controller: ProviderAllowanceReadController

  constructor(client: QueryClient, api: ProviderAllowanceReadApi) {
    this.#controller = new ProviderAllowanceReadController({
      client,
      api,
      onSnapshot: (snapshot) => {
        this.#state = snapshot
      },
    })
    this.#state = this.#controller.snapshot
    onMount(() => this.#controller.start())
    onDestroy(() => this.#controller.dispose())
  }

  get snapshot(): ProviderAllowanceReadSnapshot {
    // 构造期间同步初始化，任何视图读取都发生在此后。
    return this.#state!
  }

  retryTargets = (): Promise<void> => this.#controller.retryTargets()
  retryProvider = (id: string): Promise<void> => this.#controller.retryProvider(id)
  refreshProvider = (id: string) => this.#controller.refreshProvider(id)
  refreshAll = () => this.#controller.refreshAll()
  setGuard = (id: string, key: string, checked: boolean): Promise<void> => this.#controller.setGuard(id, key, checked)
}

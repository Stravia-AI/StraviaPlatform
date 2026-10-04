import { ProviderModelEditingController } from './provider-model-editing'
import type { ProviderModelEditingDeps, ProviderModelEditingSnapshot } from './provider-model-editing'
import type { ProviderModelSelectionPolicy } from './types'

export { ProviderModelEditingController } from './provider-model-editing'
export type {
  ProviderModelEditingApi,
  ProviderModelEditingDeps,
  ProviderModelEditingOperation,
  ProviderModelEditingSnapshot,
} from './provider-model-editing'

/** 单一响应式快照；读取归属、草稿保护与写入收口只由 controller 决定。 */
export class ProviderModelEditing {
  state = $state.raw<ProviderModelEditingSnapshot>() as ProviderModelEditingSnapshot
  readonly #controller: ProviderModelEditingController

  constructor(deps: ProviderModelEditingDeps) {
    this.#controller = new ProviderModelEditingController({
      ...deps,
      onSnapshot: (snapshot) => {
        this.state = snapshot
        deps.onSnapshot?.(snapshot)
      },
    })
    this.state = this.#controller.snapshot
  }

  select = (providerId: string, modelId: string): Promise<void> => this.#controller.select(providerId, modelId)
  retry = (): Promise<void> => this.#controller.retry()
  requestLeave = (action: () => void | Promise<void>): void => this.#controller.requestLeave(action)
  confirmDiscard = (): Promise<void> => this.#controller.confirmDiscard()
  keepEditing = (): void => this.#controller.keepEditing()
  markDirty = (dirty: boolean): void => this.#controller.markDirty(dirty)
  close = (): void => this.#controller.close()
  setDrawerOpen = (open: boolean): void => this.#controller.setDrawerOpen(open)
  prepareManual = (providerId: string, templateId: string, existingModelIds: readonly string[]): Promise<boolean> =>
    this.#controller.prepareManual(providerId, templateId, existingModelIds)
  save = (metadataJson: string): Promise<void> => this.#controller.save(metadataJson)
  changeSelection = (policy: ProviderModelSelectionPolicy): Promise<void> => this.#controller.changeSelection(policy)
  requestReimport = (): void => this.#controller.requestReimport()
  requestDelete = (): void => this.#controller.requestDelete()
  setDeleteOpen = (open: boolean): void => this.#controller.setDeleteOpen(open)
  delete = (): Promise<void> => this.#controller.delete()
  refresh = (): Promise<void> => this.#controller.refresh()
  dispose = (): void => this.#controller.dispose()
}

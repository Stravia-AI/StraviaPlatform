import { modelIdFromCatalogId } from './catalog-model-id'
import type { PreparedProviderModel, ProviderModelDetail, ProviderModelSelectionPolicy } from './types'

export type ProviderModelEditingOperation = 'save' | 'selection' | 'reimport' | 'delete'
export interface ProviderModelEditingApi {
  model(providerId: string, modelId: string): Promise<ProviderModelDetail>
  prepareModel(providerId: string, modelId: string, templateId?: string): Promise<PreparedProviderModel>
  createManualModel(
    providerId: string,
    modelId: string,
    metadataJson: string,
    templateId?: string,
  ): Promise<ProviderModelDetail>
  updateModel(providerId: string, modelId: string, metadataJson: string, revision: number): Promise<ProviderModelDetail>
  updateModelSelection(
    providerId: string,
    modelId: string,
    policy: ProviderModelSelectionPolicy,
    revision: number,
  ): Promise<ProviderModelDetail>
  reimportModel(providerId: string, modelId: string, revision: number): Promise<ProviderModelDetail>
  deleteManualModel(providerId: string, modelId: string): Promise<void>
}
export interface ProviderModelEditingSnapshot {
  providerId: string
  modelId: string
  detail?: ProviderModelDetail
  draft: boolean
  drawerOpen: boolean
  loading: boolean
  preparing: boolean
  busy: boolean
  dirty: boolean
  discardOpen: boolean
  deleteOpen: boolean
  readError: unknown
  refreshError: unknown
}
export interface ProviderModelEditingDeps {
  api: ProviderModelEditingApi
  navigate(modelId?: string): Promise<void>
  refresh(operation: ProviderModelEditingOperation): Promise<void>
  onSuccess(operation: ProviderModelEditingOperation): void
  onError(error: unknown, phase: 'read' | 'write' | 'refresh' | 'navigation'): void
  settleEditor(): Promise<void>
  onSnapshot?(snapshot: ProviderModelEditingSnapshot): void
}

export class ProviderModelEditingController {
  readonly #deps: ProviderModelEditingDeps
  #state: ProviderModelEditingSnapshot = {
    providerId: '',
    modelId: '',
    draft: false,
    drawerOpen: false,
    loading: false,
    preparing: false,
    busy: false,
    dirty: false,
    discardOpen: false,
    deleteOpen: false,
    readError: undefined,
    refreshError: undefined,
  }
  #epoch = 0
  #disposed = false
  #discarding = false
  #pending: (() => void | Promise<void>) | undefined
  #refreshOperation: ProviderModelEditingOperation | undefined
  #templateId: string | undefined

  constructor(deps: ProviderModelEditingDeps) {
    this.#deps = deps
  }
  get snapshot(): ProviderModelEditingSnapshot {
    return this.#state
  }
  #patch(patch: Partial<ProviderModelEditingSnapshot>): void {
    if (this.#disposed) return
    this.#state = { ...this.#state, ...patch }
    this.#deps.onSnapshot?.(this.#state)
  }
  #current(epoch: number): boolean {
    return !this.#disposed && epoch === this.#epoch
  }
  #clear(providerId = ''): void {
    ++this.#epoch
    this.#templateId = undefined
    this.#pending = undefined
    this.#refreshOperation = undefined
    this.#patch({
      providerId,
      modelId: '',
      detail: undefined,
      draft: false,
      drawerOpen: false,
      loading: false,
      preparing: false,
      dirty: false,
      discardOpen: false,
      deleteOpen: false,
      readError: undefined,
      refreshError: undefined,
    })
  }
  async #navigate(modelId?: string): Promise<void> {
    try {
      await this.#deps.navigate(modelId)
    } catch (error) {
      if (!this.#disposed) this.#deps.onError(error, 'navigation')
    }
  }
  async #action(action: () => void | Promise<void>): Promise<void> {
    try {
      await action()
    } catch (error) {
      if (!this.#disposed) this.#deps.onError(error, 'navigation')
    }
  }
  requestLeave(action: () => void | Promise<void>): void {
    if (this.#disposed || this.#state.busy || this.#discarding) return
    if (this.#state.dirty) {
      if (!this.#pending) this.#pending = action
      this.#patch({ discardOpen: true })
    } else void this.#action(action)
  }
  keepEditing(): void {
    if (this.#state.busy || this.#discarding) return
    this.#pending = undefined
    this.#patch({ discardOpen: false })
  }
  async confirmDiscard(): Promise<void> {
    if (this.#disposed || this.#state.busy || this.#discarding || !this.#pending) return
    const action = this.#pending
    this.#pending = undefined
    this.#discarding = true
    const detail = this.#state.detail
    this.#patch({
      dirty: false,
      discardOpen: false,
      detail: detail ? { ...detail, metadata: structuredClone(detail.metadata) } : undefined,
    })
    try {
      await this.#deps.settleEditor()
    } catch (error) {
      this.#deps.onError(error, 'navigation')
      return
    } finally {
      this.#discarding = false
    }
    if (!this.#disposed) await this.#action(action)
  }
  markDirty(dirty: boolean): void {
    if (!this.#state.busy && !this.#state.preparing && !this.#discarding && this.#state.detail) this.#patch({ dirty })
  }
  async select(providerId: string, modelId: string): Promise<void> {
    if (this.#disposed || this.#state.busy || this.#discarding) return
    if (providerId === this.#state.providerId && modelId === this.#state.modelId) return
    if (this.#state.dirty) {
      this.requestLeave(() => this.select(providerId, modelId))
      return
    }
    this.#clear(providerId)
    if (!modelId) return
    const epoch = this.#epoch
    this.#patch({ modelId, drawerOpen: false, loading: true })
    try {
      const detail = await this.#deps.api.model(providerId, modelId)
      if (this.#current(epoch)) this.#patch({ detail })
    } catch (error) {
      if (this.#current(epoch)) {
        this.#patch({ readError: error })
        this.#deps.onError(error, 'read')
      }
    } finally {
      if (this.#current(epoch)) this.#patch({ loading: false })
    }
  }
  async retry(): Promise<void> {
    if (this.#disposed || this.#state.busy || this.#state.loading || this.#state.preparing || !this.#state.modelId)
      return
    if (this.#state.dirty) {
      this.requestLeave(() => this.retry())
      return
    }
    const { providerId, modelId } = this.#state
    this.#clear(providerId)
    await this.select(providerId, modelId)
  }
  close(): void {
    this.requestLeave(async () => {
      this.#clear()
      await this.#navigate()
    })
  }
  setDrawerOpen(open: boolean): void {
    if (!open) this.close()
    else if (!this.#state.busy && !this.#state.preparing) this.#patch({ drawerOpen: true })
  }
  async prepareManual(providerId: string, templateId: string, existingModelIds: readonly string[]): Promise<boolean> {
    if (this.#disposed || this.#state.busy || this.#state.preparing || this.#discarding || !templateId.trim())
      return false
    if (this.#state.dirty) {
      this.requestLeave(async () => {
        await this.prepareManual(providerId, templateId, existingModelIds)
      })
      return false
    }
    this.#clear(providerId)
    const epoch = this.#epoch
    const modelId = modelIdFromCatalogId(templateId)
    this.#patch({ modelId, preparing: true })
    try {
      const prepared = await this.#deps.api.prepareModel(providerId, modelId, templateId)
      if (!this.#current(epoch)) return false
      const draft = !existingModelIds.includes(prepared.id)
      const detail: ProviderModelDetail = draft
        ? {
            ...prepared,
            snapshot_state: {
              type: 'edited',
              source: prepared.snapshot_state.type === 'imported' ? prepared.snapshot_state.source : null,
            },
            available: true,
            source_kind: 'manual',
            can_reimport: false,
            selection_policy: 'auto',
            revision: 0,
            created_at: '',
            updated_at: '',
          }
        : await this.#deps.api.model(providerId, prepared.id)
      if (!this.#current(epoch)) return false
      this.#templateId = templateId
      this.#patch({ modelId: detail.id, detail, draft, drawerOpen: draft })
      await this.#navigate(detail.id)
      return this.#current(epoch)
    } catch (error) {
      if (this.#current(epoch)) {
        this.#patch({ modelId: '', readError: error })
        this.#deps.onError(error, 'read')
      }
      return false
    } finally {
      if (this.#current(epoch)) this.#patch({ preparing: false })
    }
  }
  async #refresh(operation: ProviderModelEditingOperation): Promise<void> {
    this.#refreshOperation = operation
    this.#patch({ refreshError: undefined })
    try {
      await this.#deps.refresh(operation)
      if (!this.#disposed) this.#refreshOperation = undefined
    } catch (error) {
      if (!this.#disposed) {
        this.#patch({ refreshError: error })
        this.#deps.onError(error, 'refresh')
      }
    }
  }
  async #write(
    operation: ProviderModelEditingOperation,
    write: (providerId: string, detail: ProviderModelDetail) => Promise<ProviderModelDetail | void>,
  ): Promise<void> {
    if (
      this.#disposed ||
      this.#state.busy ||
      this.#state.loading ||
      this.#state.preparing ||
      this.#discarding ||
      !this.#state.detail
    )
      return
    const { providerId, detail, draft } = this.#state
    const epoch = this.#epoch
    this.#pending = undefined
    this.#patch({ busy: true, discardOpen: false })
    try {
      let saved: ProviderModelDetail | void
      try {
        saved = await write(providerId, detail)
      } catch (error) {
        if (this.#current(epoch)) this.#deps.onError(error, 'write')
        return
      }
      if (!this.#current(epoch)) return
      if (operation === 'delete') this.#clear(providerId)
      else if (saved)
        this.#patch(
          operation === 'selection'
            ? {
                detail: {
                  ...detail,
                  available: saved.available,
                  selection_policy: saved.selection_policy,
                  revision: saved.revision,
                },
              }
            : { detail: saved, draft: false, dirty: false, drawerOpen: draft ? false : this.#state.drawerOpen },
        )
      this.#deps.onSuccess(operation)
      await this.#refresh(operation)
      if (operation === 'delete' && !this.#disposed) await this.#navigate()
    } finally {
      this.#patch({ busy: false })
    }
  }
  save(metadataJson: string): Promise<void> {
    const draft = this.#state.draft
    const templateId = this.#templateId
    return this.#write('save', (providerId, detail) =>
      draft
        ? this.#deps.api.createManualModel(providerId, detail.id, metadataJson, templateId)
        : this.#deps.api.updateModel(providerId, detail.id, metadataJson, detail.revision),
    )
  }
  changeSelection(policy: ProviderModelSelectionPolicy): Promise<void> {
    if (this.#state.draft || this.#state.detail?.selection_policy === policy) return Promise.resolve()
    return this.#write('selection', (providerId, detail) =>
      this.#deps.api.updateModelSelection(providerId, detail.id, policy, detail.revision),
    )
  }
  requestReimport(): void {
    this.requestLeave(() =>
      this.#write('reimport', (providerId, detail) =>
        this.#deps.api.reimportModel(providerId, detail.id, detail.revision),
      ),
    )
  }
  requestDelete(): void {
    this.requestLeave(() => {
      this.#patch({ deleteOpen: true })
    })
  }
  setDeleteOpen(open: boolean): void {
    if (!this.#state.busy) this.#patch({ deleteOpen: open })
  }
  delete(): Promise<void> {
    if (!this.#state.deleteOpen || this.#state.draft || this.#state.detail?.source_kind !== 'manual')
      return Promise.resolve()
    return this.#write('delete', (providerId, detail) => this.#deps.api.deleteManualModel(providerId, detail.id))
  }
  async refresh(): Promise<void> {
    if (this.#disposed || this.#state.busy || this.#state.preparing || !this.#refreshOperation) return
    const operation = this.#refreshOperation
    this.#patch({ busy: true })
    try {
      await this.#refresh(operation)
    } finally {
      this.#patch({ busy: false })
    }
  }
  dispose(): void {
    this.#clear()
    this.#patch({ busy: false })
    this.#disposed = true
    ++this.#epoch
  }
}

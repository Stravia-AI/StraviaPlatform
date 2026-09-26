import {
  ProviderConnectionDraftController,
  emptyProviderConnectionDraft,
  type ProviderConnectionDraftDeps,
  type ProviderConnectionDraftFields,
  type ProviderConnectionDraftSnapshot,
  type ProviderConnectionSubmitOutcome,
} from '$lib/provider-connection-draft'
import type { OAuthCandidateConfiguration, ProviderConfigurationPreview } from '$lib/types'

export { ProviderConnectionDraftController, emptyProviderConnectionDraft } from '$lib/provider-connection-draft'
export type {
  ProviderConnectionDraftDeps,
  ProviderConnectionDraftFields,
  ProviderConnectionDraftHooks,
  ProviderConnectionDraftSnapshot,
  ProviderConnectionDraftTarget,
  ProviderConnectionOAuthHandle,
  ProviderConnectionSubmission,
  ProviderConnectionSubmitOutcome,
  ProviderConnectionWriter,
} from '$lib/provider-connection-draft'

/**
 * ProviderConnectionDraftController 的 Svelte 壳：快照字段镜像为 $state；
 * fields 的 $state 代理由壳创建并交给 controller 持有 —— 只存在这一个草稿载体，
 * 视图绑定与 controller 读写都经过同一代理。编排语义全部在 controller 里。
 */
export class ProviderConnectionDraft {
  fields = $state<ProviderConnectionDraftFields>(emptyProviderConnectionDraft())
  preview = $state<ProviderConfigurationPreview>()
  submitting = $state(false)
  oauthSessionId = $state<string>()
  oauthReady = $state(false)
  submitError = $state<unknown>()
  partialSaveError = $state<unknown>()

  readonly #controller: ProviderConnectionDraftController
  readonly #deps: ProviderConnectionDraftDeps

  constructor(deps: ProviderConnectionDraftDeps) {
    this.#deps = deps
    this.fields = deps.fields ?? emptyProviderConnectionDraft()
    this.#controller = new ProviderConnectionDraftController({
      ...deps,
      fields: this.fields,
      onSnapshot: (snapshot) => this.#applySnapshot(snapshot),
    })
    this.#applySnapshot(this.#controller.snapshot)
  }

  /** 依赖 target() 与 fields 的派生量走 getter：读取处即响应式依赖收集点。 */
  get submittable(): boolean {
    return this.#controller.submittable(this.fields, this.#deps.target())
  }

  get unknownOptionKeys(): string[] {
    return this.#controller.unknownOptionKeys(this.#deps.target())
  }

  get oauthConfiguration(): OAuthCandidateConfiguration {
    return this.#controller.oauthConfiguration(this.fields, this.#deps.target())
  }

  submit = (): Promise<ProviderConnectionSubmitOutcome> => this.#controller.submit()
  dispose = (): void => this.#controller.dispose()
  nameChanged = (): void => this.#controller.nameChanged()
  configurationChanged = (): void => this.#controller.configurationChanged()
  useProxyChanged = (): void => this.#controller.useProxyChanged()
  oauthStateChanged = (sessionId: string | undefined, ready: boolean): void =>
    this.#controller.oauthStateChanged(sessionId, ready)
  resetConnection = (fields?: ProviderConnectionDraftFields): Promise<void> => this.#controller.resetConnection(fields)

  #applySnapshot(snapshot: ProviderConnectionDraftSnapshot): void {
    this.preview = snapshot.preview
    this.submitting = snapshot.submitting
    this.oauthSessionId = snapshot.oauthSessionId
    this.oauthReady = snapshot.oauthReady
    this.submitError = snapshot.submitError
    this.partialSaveError = snapshot.partialSaveError
  }
}

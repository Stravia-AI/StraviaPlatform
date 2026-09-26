import type {
  OAuthCandidateConfiguration,
  Provider,
  ProviderConfigurationPreview,
  ProviderConfigurationPreviewInput,
  ProviderDescriptor,
  VendorChannelDescriptor,
  VendorConfigField,
} from '$lib/types'

/**
 * Provider 连接草稿的字段。视图通过 Svelte 壳代理直接写入；
 * controller 只在 chooseOption / 完整保存成功时整体替换对象。
 */
export interface ProviderConnectionDraftFields {
  name: string
  baseUrl: string
  protocol: string
  useProxy: boolean
  values: Record<string, unknown>
}

export function emptyProviderConnectionDraft(): ProviderConnectionDraftFields {
  return { name: '', baseUrl: '', protocol: '', useProxy: false, values: {} }
}

/**
 * 连接目标：create = 编辑器中选中的 descriptor/channel；
 * edit = 已保存 provider 及其 descriptor/channel。undefined 表示尚不可提交。
 */
export type ProviderConnectionDraftTarget =
  | { kind: 'create'; descriptor: ProviderDescriptor; channel: VendorChannelDescriptor }
  | { kind: 'edit'; provider: Provider; descriptor: ProviderDescriptor; channel: VendorChannelDescriptor }

/** 交给写入适配器的规范化提交内容；baseUrl 已是 preview 规范化后的值。 */
export interface ProviderConnectionSubmission {
  target: ProviderConnectionDraftTarget
  name: string
  baseUrl: string
  protocol?: string
  useProxy: boolean
  options: Record<string, unknown>
  credentials: Record<string, unknown>
  /** 当前 OAuth session（如有）；create 路径据此选择 createOAuth。 */
  sessionId?: string
}

/**
 * 两条路径唯一的真实差异：create 用 create/createOAuth(sessionId)，
 * edit 用 update + 可选 bind。payload 组装留在各视图 adapter。
 */
export interface ProviderConnectionWriter {
  write(submission: ProviderConnectionSubmission): Promise<Provider>
  /** 仅 edit：把已 ready 的 OAuth session 绑定到已保存 provider；失败即部分成功。 */
  bind?(provider: Provider, sessionId: string): Promise<void>
}

export interface ProviderConnectionDraftApi {
  previewConfiguration(input: ProviderConfigurationPreviewInput): Promise<ProviderConfigurationPreview>
}

/** OAuth 子组件句柄。何时取消/消费/换代理由 controller 决定；交互细节留在子组件。 */
export interface ProviderConnectionOAuthHandle {
  /** 取消当前 session；session 未创建但 init 在途时同样生效。 */
  cancel(): Promise<void>
  /** 已写入成功：丢弃本地 session 状态，不再上报/取消。 */
  consume(): void
  updateProxy(useProxy: boolean): Promise<void>
}

/** UI 结果通知。两个回调被调用时提交锁均已释放，视图可安全 goto / 关闭 Sheet。 */
export interface ProviderConnectionDraftHooks {
  /** 完整成功（edit 含 bind）。 */
  onSaved(provider: Provider): void | Promise<void>
  /** 仅 edit：配置已写入但 OAuth bind 失败；草稿保持用户输入不替换。 */
  onPartialSave?(provider: Provider, error: unknown): void
}

export interface ProviderConnectionDraftSnapshot {
  fields: ProviderConnectionDraftFields
  preview: ProviderConfigurationPreview | undefined
  /** 提交锁：preview → write → bind 全程为 true；字段禁用、Sheet/导航封闭据此实现。 */
  submitting: boolean
  oauthSessionId: string | undefined
  oauthReady: boolean
  /** write/preview 请求失败（视图负责 toast / 内联展示）。 */
  submitError: unknown
  /** edit：配置已保存但 OAuth bind 失败（部分成功，草稿不替换）。 */
  partialSaveError: unknown
}

export type ProviderConnectionSubmitOutcome = 'saved' | 'partial' | 'failed' | 'issues' | 'aborted' | 'ignored'

export interface ProviderConnectionDraftDeps {
  fields?: ProviderConnectionDraftFields
  target(): ProviderConnectionDraftTarget | undefined
  api: ProviderConnectionDraftApi
  writer: ProviderConnectionWriter
  oauth: ProviderConnectionOAuthHandle
  hooks: ProviderConnectionDraftHooks
  onSnapshot?(snapshot: ProviderConnectionDraftSnapshot): void
}

/**
 * 声明字段过滤：secret 端剔除 undefined/null/空串（留空即保留服务端值）；
 * options 端回填 config_fields 未声明的既有键（edit 路径保留未知 vendor_options）。
 */
function providerConfigValues(
  values: Record<string, unknown>,
  configFields: VendorConfigField[],
  secret: boolean,
  preserved: Record<string, unknown> = {},
): Record<string, unknown> {
  const declared = new Set(configFields.filter((field) => field.secret === secret).map((field) => field.key))
  const entries = Object.entries(values).filter(
    ([key, value]) =>
      declared.has(key) &&
      value !== undefined &&
      (!secret || (value !== null && (typeof value !== 'string' || value.length > 0))),
  )
  if (!secret) {
    for (const [key, value] of Object.entries(preserved)) {
      if (!configFields.some((field) => field.key === key) && !entries.some(([entryKey]) => entryKey === key)) {
        entries.push([key, value])
      }
    }
  }
  return Object.fromEntries(entries)
}

function preservedOptions(target: ProviderConnectionDraftTarget): Record<string, unknown> {
  return target.kind === 'edit' ? (target.provider.vendor_options ?? {}) : {}
}

/**
 * Provider 连接草稿的编排状态机：草稿有效性、preview 代际与失效、
 * 提交互斥与 preview → write → bind 时序、OAuth session 协调与 ready 自动提交、
 * 部分成功判定都收敛在这里。视图只负责 DOM、弹窗、toast 与导航封闭。
 * 竞态用内部 epoch 计数；调用方不可见版本号。
 */
export class ProviderConnectionDraftController {
  readonly #deps: ProviderConnectionDraftDeps

  #fields: ProviderConnectionDraftFields
  #preview: ProviderConfigurationPreview | undefined
  #submitting = false
  #oauthSessionId: string | undefined
  #oauthReady = false
  #submitError: unknown
  #partialSaveError: unknown
  #epoch = 0
  #disposed = false

  constructor(deps: ProviderConnectionDraftDeps) {
    this.#deps = deps
    this.#fields = deps.fields ?? emptyProviderConnectionDraft()
  }

  get fields(): ProviderConnectionDraftFields {
    return this.#fields
  }

  get snapshot(): ProviderConnectionDraftSnapshot {
    return {
      fields: this.#fields,
      preview: this.#preview,
      submitting: this.#submitting,
      oauthSessionId: this.#oauthSessionId,
      oauthReady: this.#oauthReady,
      submitError: this.#submitError,
      partialSaveError: this.#partialSaveError,
    }
  }

  /**
   * 组件销毁：进行中的编排全部作废，迟到的 init/ready 上报不再触发提交。
   * session 取消本身由 OAuth 子组件 onDestroy 负责。
   */
  dispose(): void {
    this.#disposed = true
    this.#invalidate()
    this.#oauthSessionId = undefined
    this.#oauthReady = false
  }

  /** 仅 edit 有未知 vendor_options 保留；keys 未在任何非 secret 声明字段中出现即保留项。 */
  unknownOptionKeys(target: ProviderConnectionDraftTarget | undefined): string[] {
    if (target?.kind !== 'edit') return []
    const declared = new Set(target.descriptor.config_fields.filter((field) => !field.secret).map((field) => field.key))
    return Object.keys(target.provider.vendor_options ?? {}).filter((key) => !declared.has(key))
  }

  submittable(fields: ProviderConnectionDraftFields, target: ProviderConnectionDraftTarget | undefined): boolean {
    return Boolean(target) && fields.name.trim().length > 0 && this.unknownOptionKeys(target).length === 0
  }

  /** OAuth 子组件的 configuration prop；随草稿与 target 当前值构建。 */
  oauthConfiguration(
    fields: ProviderConnectionDraftFields,
    target: ProviderConnectionDraftTarget | undefined,
  ): OAuthCandidateConfiguration {
    return {
      provider_id: target?.kind === 'edit' ? target.provider.id : undefined,
      base_url: fields.baseUrl.trim(),
      protocol:
        target?.kind === 'edit'
          ? target.provider.protocol || undefined
          : fields.protocol || target?.channel.protocol || undefined,
      options: target
        ? providerConfigValues(fields.values, target.descriptor.config_fields, false, preservedOptions(target))
        : {},
      credentials: target ? providerConfigValues(fields.values, target.descriptor.config_fields, true) : {},
    }
  }

  /** 名称变化：只使 preview 失效；名称不参与 OAuth configuration。 */
  nameChanged(): void {
    if (this.#disposed) return
    this.#invalidate()
    this.#deps.onSnapshot?.(this.snapshot)
  }

  /**
   * 连接配置变化（base_url / protocol / options / credentials）：preview 作废，
   * OAuth session 一并取消 —— 包括尚未返回的 init（cancel 由子组件代际守卫覆盖在途结果）。
   */
  configurationChanged(): void {
    if (this.#disposed) return
    this.#invalidate()
    this.#oauthSessionId = undefined
    this.#oauthReady = false
    void this.#deps.oauth.cancel()
    this.#deps.onSnapshot?.(this.snapshot)
  }

  /** 代理切换：preview 失效；ready session 不能换代理直接丢弃，其余交给子组件 updateProxy。 */
  useProxyChanged(): void {
    if (this.#disposed) return
    this.#invalidate()
    if (this.#oauthReady) {
      this.#oauthSessionId = undefined
      this.#oauthReady = false
    }
    void this.#deps.oauth.updateProxy(this.#fields.useProxy)
    this.#deps.onSnapshot?.(this.snapshot)
  }

  /**
   * OAuth 子组件上报 session 状态。相同 (sessionId, ready) 上报直接忽略：
   * 一次 ready 只自动提交一次，partial/failed 后的重复上报不得再次写入。
   * 非提交期：更新会话、使 preview 失效，session ready 触发一次自动提交。
   * 提交中：只记录最新 session（write/bind 读取当前值），不触发失效或二次提交。
   */
  oauthStateChanged(sessionId: string | undefined, ready: boolean): void {
    if (this.#disposed) return
    if (sessionId === this.#oauthSessionId && ready === this.#oauthReady) return
    this.#oauthSessionId = sessionId
    this.#oauthReady = ready
    if (this.#submitting) {
      this.#deps.onSnapshot?.(this.snapshot)
      return
    }
    this.#invalidate()
    this.#deps.onSnapshot?.(this.snapshot)
    if (sessionId && ready) void this.submit()
  }

  /** 换服务 / 回退 / 关闭 Sheet：取消 OAuth（含在途 init）并重置连接状态；可整体替换草稿。 */
  async resetConnection(fields?: ProviderConnectionDraftFields): Promise<void> {
    await this.#deps.oauth.cancel()
    if (this.#disposed) return
    this.#invalidate()
    this.#oauthSessionId = undefined
    this.#oauthReady = false
    if (fields) {
      // 成员级写入保持草稿对象 identity 不变：Svelte 代理与视图绑定不重建。
      this.#fields.name = fields.name
      this.#fields.baseUrl = fields.baseUrl
      this.#fields.protocol = fields.protocol
      this.#fields.useProxy = fields.useProxy
      this.#fields.values = fields.values
    }
    this.#deps.onSnapshot?.(this.snapshot)
  }

  /**
   * 提交：preview（可跳过）→ write →（edit 且有 ready session）bind。
   * 互斥 + epoch 守卫：提交期间的 preview 响应若已过期不得写回草稿。
   * 锁在调用 hooks.onSaved / onPartialSave 前释放，保证父级 goto / 关闭不被导航守卫拦截。
   */
  async submit(): Promise<ProviderConnectionSubmitOutcome> {
    const target = this.#deps.target()
    if (this.#submitting || this.#disposed || !target || !this.submittable(this.#fields, target)) return 'ignored'
    this.#submitting = true
    this.#submitError = undefined
    this.#partialSaveError = undefined
    this.#deps.onSnapshot?.(this.snapshot)
    const generation = this.#epoch
    try {
      let baseUrl = this.#fields.baseUrl.trim()
      if (target.channel.capabilities.includes('config_validation') || baseUrl) {
        const preview = await this.#deps.api.previewConfiguration(this.#previewInput(target))
        if (generation !== this.#epoch || this.#disposed) return 'aborted'
        this.#preview = preview
        this.#deps.onSnapshot?.(this.snapshot)
        if (preview.issues.length > 0) return 'issues'
        baseUrl = preview.base_url
      }
      const provider = await this.#deps.writer.write(this.#submission(target, baseUrl))
      // 销毁后到达的写入结果不得再触达 UI 回调（onSaved 可能触发父级 goto/关闭）。
      if (this.#disposed) return 'aborted'
      if (target.kind === 'edit' && this.#oauthSessionId && this.#oauthReady && this.#deps.writer.bind) {
        try {
          await this.#deps.writer.bind(provider, this.#oauthSessionId)
          this.#deps.oauth.consume()
        } catch (error) {
          this.#partialSaveError = error
          this.#finish()
          if (!this.#disposed) this.#deps.hooks.onPartialSave?.(provider, error)
          return 'partial'
        }
      } else if (target.kind === 'create' || !this.#oauthSessionId) {
        this.#deps.oauth.consume()
      }
      this.#preview = undefined
      if (target.kind === 'edit') {
        // 完整成功才用已保存值替换草稿；bind 失败路径保留未规范化的用户输入。
        // fields 是共享草稿对象，成员级替换使 Svelte 代理与测试引用始终读到同一份。
        this.#fields.name = provider.name
        this.#fields.baseUrl = provider.base_url
        this.#fields.values = { ...(provider.vendor_options ?? {}) }
      }
      this.#finish()
      if (!this.#disposed) await this.#deps.hooks.onSaved(provider)
      return 'saved'
    } catch (error) {
      this.#submitError = error
      this.#finish()
      return 'failed'
    } finally {
      if (this.#submitting) this.#finish()
    }
  }

  #previewInput(target: ProviderConnectionDraftTarget): ProviderConfigurationPreviewInput {
    return {
      provider_id: target.kind === 'edit' ? target.provider.id : undefined,
      vendor_id: target.descriptor.provider_id,
      channel: target.channel.id,
      base_url: this.#fields.baseUrl.trim(),
      options: providerConfigValues(
        this.#fields.values,
        target.descriptor.config_fields,
        false,
        preservedOptions(target),
      ),
      credentials: providerConfigValues(this.#fields.values, target.descriptor.config_fields, true),
    }
  }

  #submission(target: ProviderConnectionDraftTarget, baseUrl: string): ProviderConnectionSubmission {
    return {
      target,
      name: this.#fields.name.trim(),
      baseUrl,
      protocol: this.#fields.protocol || target.channel.protocol || undefined,
      useProxy: this.#fields.useProxy,
      options: providerConfigValues(
        this.#fields.values,
        target.descriptor.config_fields,
        false,
        preservedOptions(target),
      ),
      credentials: providerConfigValues(this.#fields.values, target.descriptor.config_fields, true),
      sessionId: this.#oauthSessionId,
    }
  }

  #invalidate(): void {
    this.#epoch += 1
    this.#preview = undefined
    this.#submitError = undefined
    this.#partialSaveError = undefined
  }

  #finish(): void {
    this.#submitting = false
    this.#deps.onSnapshot?.(this.snapshot)
  }
}

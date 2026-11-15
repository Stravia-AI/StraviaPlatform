import { QueryObserver, type QueryClient } from '@tanstack/svelte-query'
import type {
  Provider,
  ProviderAllowanceGuardUpdate,
  ProviderAllowanceSnapshot,
  ProviderAllowanceTarget,
  ProviderAllowanceResetRequest,
  ProviderAllowanceResetResult,
  ProviderAllowanceResetOutcome,
} from './types'
import { createUuid } from './uuid'

const REFRESH_INTERVAL = 180_000
const CREDENTIAL_INVALID_MESSAGE = 'Credential invalid; allowance fetching is paused until the credential is updated.'
const providersKey = ['providers'] as const
const targetsKey = ['provider-allowances'] as const
const snapshotKey = (id: string) => ['provider-allowance', id] as const

type AllowanceProvider = Pick<Provider, 'id' | 'credential_status'>

export interface ProviderAllowanceReadApi {
  providers(): Promise<AllowanceProvider[]>
  list(): Promise<ProviderAllowanceTarget[]>
  get(id: string): Promise<ProviderAllowanceSnapshot>
  refresh(id: string): Promise<ProviderAllowanceSnapshot>
  replaceGuards(id: string, keys: string[]): Promise<ProviderAllowanceGuardUpdate>
  reset(id: string, request: ProviderAllowanceResetRequest): Promise<ProviderAllowanceResetResult>
}

export interface ProviderAllowanceReadEntry {
  target: ProviderAllowanceTarget
  snapshot?: ProviderAllowanceSnapshot
  pending: boolean
  failed: boolean
  refreshing: boolean
  credentialInvalid: boolean
  canRefresh: boolean
  canSaveGuards: boolean
  canRetry: boolean
  guardError?: unknown
  refreshError?: unknown
  savingGuards: boolean
  resetting: boolean
  canReset: boolean
  pendingResetGrantId?: string
}

export interface ProviderAllowanceReadSnapshot {
  entries: ProviderAllowanceReadEntry[]
  loading: boolean
  loadError: unknown
  hasTargets: boolean
  fetching: boolean
  refreshAllDisabled: boolean
  refreshingAll: boolean
}

export type ProviderAllowanceRefreshOutcome =
  { status: 'refreshed' } | { status: 'failed'; error: unknown } | { status: 'ignored' }

export type ProviderAllowanceResetControllerOutcome =
  | { status: 'completed'; outcome: ProviderAllowanceResetOutcome; refreshError?: unknown }
  | { status: 'failed'; error: unknown; refreshError?: unknown }
  | { status: 'ignored' }

interface ProviderAllowanceReadDeps {
  client: QueryClient
  api: ProviderAllowanceReadApi
  onSnapshot?: (snapshot: ProviderAllowanceReadSnapshot) => void
  now?: () => number
  storage?: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>
  randomUUID?: () => string
}

function invalidSnapshot(snapshot: ProviderAllowanceSnapshot | undefined): boolean {
  return snapshot?.error?.category === 'authentication' && snapshot.error.message === CREDENTIAL_INVALID_MESSAGE
}

/**
 * 额度页的读取与写回协调。QueryClient 是唯一查询缓存；observer、读取代际和
 * 操作互斥都留在这里，页面无需知道取消查询、凭据证据或缓存发布顺序。
 */
export class ProviderAllowanceReadController {
  readonly #deps: ProviderAllowanceReadDeps
  readonly #providers: QueryObserver<AllowanceProvider[]>
  readonly #targets: QueryObserver<ProviderAllowanceTarget[]>
  readonly #snapshots = new Map<string, QueryObserver<ProviderAllowanceSnapshot>>()
  readonly #discoveredInvalidAt = new Map<string, number>()
  readonly #readEpochs = new Map<string, number>()
  readonly #guardReadEpochs = new Map<string, number>()
  readonly #savingGuards = new Set<string>()
  readonly #refreshingProviders = new Set<string>()
  readonly #resettingProviders = new Set<string>()
  readonly #guardErrors = new Map<string, unknown>()
  readonly #refreshErrors = new Map<string, unknown>()
  #metadataEpoch = 0
  #refreshingAll = false
  #started = false
  #disposed = false
  #updatingQueries = false

  constructor(deps: ProviderAllowanceReadDeps) {
    this.#deps = deps
    this.#providers = new QueryObserver<AllowanceProvider[]>(deps.client, {
      queryKey: providersKey,
      queryFn: async () => {
        const epoch = this.#metadataEpoch
        const providers = await deps.api.providers()
        return epoch === this.#metadataEpoch
          ? providers
          : (deps.client.getQueryData<AllowanceProvider[]>(providersKey) ?? providers)
      },
      refetchInterval: REFRESH_INTERVAL,
      enabled: false,
    })
    this.#targets = new QueryObserver<ProviderAllowanceTarget[]>(deps.client, {
      queryKey: targetsKey,
      queryFn: async () => {
        const epoch = this.#metadataEpoch
        const targets = await deps.api.list()
        if (epoch !== this.#metadataEpoch)
          return deps.client.getQueryData<ProviderAllowanceTarget[]>(targetsKey) ?? targets
        if (!this.#disposed) {
          for (const target of targets) {
            if (invalidSnapshot(target.snapshot) && !this.#discoveredInvalidAt.has(target.provider_id)) {
              this.#discoveredInvalidAt.set(target.provider_id, this.#now())
            }
          }
          this.#changed()
        }
        return targets
      },
      refetchInterval: REFRESH_INTERVAL,
      enabled: false,
    })
  }

  get snapshot(): ProviderAllowanceReadSnapshot {
    const targets = this.#targets.getCurrentResult()
    const providers = this.#providers.getCurrentResult()
    const entries = (targets.data ?? []).map((target): ProviderAllowanceReadEntry => {
      const id = target.provider_id
      const result = this.#snapshots.get(id)?.getCurrentResult()
      const invalid = this.#credentialInvalid(id)
      const cached = result?.data ?? target.snapshot
      const snapshot = invalid && cached?.status === 'fresh' ? { ...cached, status: 'stale' as const } : cached
      const failed = snapshot == null && Boolean(result?.isError)
      const refreshing =
        !invalid &&
        (this.#refreshingProviders.has(id) ||
          Boolean(result?.isFetching) ||
          (target.refreshing && (!result?.data || result.dataUpdatedAt < targets.dataUpdatedAt)))
      return {
        // 单项读取只更新自己的query；写targets会重置其180秒轮询计时。
        target: snapshot ? { ...target, snapshot, suspension: snapshot.suspension, refreshing } : target,
        snapshot,
        pending: snapshot == null && !failed && !invalid,
        failed,
        refreshing,
        credentialInvalid: invalid,
        canRefresh:
          !this.#disposed && !invalid && !providers.isPending && !refreshing && !this.#refreshingAll && !this.#busy(id),
        canSaveGuards: !this.#disposed && !this.#busy(id),
        canRetry: !this.#disposed && Boolean(result) && !invalid && !this.#busy(id),
        guardError: this.#guardErrors.get(id),
        refreshError: this.#refreshErrors.get(id),
        savingGuards: this.#savingGuards.has(id),
        resetting: this.#resettingProviders.has(id),
        canReset:
          !this.#disposed &&
          !this.#credentialInvalid(id) &&
          !this.#busy(id) &&
          !refreshing &&
          !this.#refreshingAll &&
          !providers.isPending &&
          snapshot?.status === 'fresh' &&
          !snapshot.error &&
          !result?.isError &&
          snapshot.resets != null &&
          !this.#refreshErrors.has(id),
        pendingResetGrantId: this.#pendingResetGrantId(id),
      }
    })
    return {
      entries,
      loading: targets.isPending,
      loadError: targets.error,
      hasTargets: targets.data !== undefined,
      fetching: targets.isFetching,
      refreshingAll: this.#refreshingAll,
      refreshAllDisabled:
        this.#disposed ||
        this.#refreshingAll ||
        targets.isPending ||
        providers.isPending ||
        !entries.some((entry) => !entry.credentialInvalid),
    }
  }

  start(): void {
    if (this.#started || this.#disposed) return
    this.#started = true
    this.#providers.setOptions({ ...this.#providers.options, enabled: true })
    this.#targets.setOptions({ ...this.#targets.options, enabled: true })
    this.#providers.subscribe(() => this.#changed())
    this.#targets.subscribe(() => this.#changed())
    this.#changed()
  }

  dispose(): void {
    if (this.#disposed) return
    this.#disposed = true
    this.#providers.destroy()
    this.#targets.destroy()
    for (const observer of this.#snapshots.values()) observer.destroy()
    this.#snapshots.clear()
  }

  async retryTargets(): Promise<void> {
    if (!this.#disposed) await this.#targets.refetch()
  }

  async retryProvider(id: string): Promise<void> {
    if (this.#disposed || this.#credentialInvalid(id) || this.#busy(id)) return
    await this.#snapshots.get(id)?.refetch()
  }

  async refreshProvider(id: string): Promise<ProviderAllowanceRefreshOutcome> {
    if (
      !this.#started ||
      this.#disposed ||
      this.#providers.getCurrentResult().isPending ||
      this.#credentialInvalid(id) ||
      this.#busy(id) ||
      !this.#target(id)
    ) {
      return { status: 'ignored' }
    }
    this.#refreshingProviders.add(id)
    this.#changed()
    try {
      this.#publish(await this.#deps.api.refresh(id))
      this.#refreshErrors.delete(id)
      await this.#refreshMetadata()
      return this.#disposed ? { status: 'ignored' } : { status: 'refreshed' }
    } catch (error) {
      return this.#disposed ? { status: 'ignored' } : { status: 'failed', error }
    } finally {
      this.#refreshingProviders.delete(id)
      this.#changed()
    }
  }

  async resetProvider(id: string, grantId: string): Promise<ProviderAllowanceResetControllerOutcome> {
    const entry = this.snapshot.entries.find((item) => item.target.provider_id === id)
    const grant = entry?.snapshot?.resets?.grants.find((item) => item.id === grantId)
    const pendingRetry = entry?.pendingResetGrantId === grantId
    if (
      !this.#started ||
      !entry?.canReset ||
      (entry.pendingResetGrantId != null && !pendingRetry) ||
      (!pendingRetry &&
        (!grant?.usable || grant.remaining <= 0 || (grant.expires_at != null && grant.expires_at <= this.#now())))
    )
      return { status: 'ignored' }

    this.#resettingProviders.add(id)
    this.#readEpochs.set(id, (this.#readEpochs.get(id) ?? 0) + 1)
    this.#changed()
    let result: ProviderAllowanceResetControllerOutcome
    let submitted = false
    try {
      const storage = this.#storage()
      const key = this.#resetStorageKey(id)
      const raw = storage.getItem(key)
      const value: unknown = raw
        ? JSON.parse(raw)
        : { grant_id: grantId, request_id: (this.#deps.randomUUID ?? createUuid)() }
      if (
        value == null ||
        typeof value !== 'object' ||
        !('grant_id' in value) ||
        !('request_id' in value) ||
        typeof value.grant_id !== 'string' ||
        typeof value.request_id !== 'string' ||
        !value.request_id
      )
        throw new Error('Invalid pending allowance reset')
      const pending: ProviderAllowanceResetRequest = { grant_id: value.grant_id, request_id: value.request_id }
      if (pending.grant_id !== grantId) throw new Error('Another allowance reset is pending')
      if (!raw) storage.setItem(key, JSON.stringify(pending))
      await this.#deps.client.cancelQueries({ queryKey: snapshotKey(id) })
      if (this.#disposed) throw new Error('Allowance controller disposed before reset')
      submitted = true
      const response = await this.#deps.api.reset(id, { ...pending, retry: Boolean(raw) })
      // A definitive business outcome resolves the logical request, even if the following read fails.
      result = { status: 'completed', outcome: response.outcome }
      try {
        storage.removeItem(key)
      } catch {
        /* Retaining the ID is safer than creating a new one. */
      }
    } catch (error) {
      result = { status: 'failed', error }
    }
    try {
      if (submitted) {
        this.#publish(await this.#deps.api.refresh(id))
        this.#refreshErrors.delete(id)
        await this.#refreshMetadata()
      }
    } catch (refreshError) {
      this.#refreshErrors.set(id, refreshError)
      result = { ...result, refreshError }
    } finally {
      this.#resettingProviders.delete(id)
      this.#changed()
    }
    return result
  }

  async refreshAll(): Promise<ProviderAllowanceRefreshOutcome> {
    if (!this.#started || this.#disposed || this.#refreshingAll || this.#providers.getCurrentResult().isPending)
      return { status: 'ignored' }
    const pending = (this.#targets.getCurrentResult().data ?? []).filter(
      (target) => !this.#credentialInvalid(target.provider_id) && !this.#busy(target.provider_id),
    )
    if (pending.length === 0) return { status: 'ignored' }
    this.#refreshingAll = true
    for (const target of pending) this.#refreshingProviders.add(target.provider_id)
    this.#changed()
    try {
      const results = await Promise.allSettled(
        pending.map(async (target) => {
          try {
            this.#publish(await this.#deps.api.refresh(target.provider_id))
            this.#refreshErrors.delete(target.provider_id)
          } finally {
            this.#refreshingProviders.delete(target.provider_id)
            this.#changed()
          }
        }),
      )
      await this.#refreshMetadata()
      if (this.#disposed) return { status: 'ignored' }
      const failure = results.find((result) => result.status === 'rejected')
      return failure ? { status: 'failed', error: failure.reason } : { status: 'refreshed' }
    } catch (error) {
      return this.#disposed ? { status: 'ignored' } : { status: 'failed', error }
    } finally {
      this.#refreshingAll = false
      this.#changed()
    }
  }

  async setGuard(id: string, key: string, checked: boolean): Promise<void> {
    if (!this.#started || this.#disposed || this.#busy(id)) return
    const current = this.#snapshots.get(id)?.getCurrentResult().data ?? this.#target(id)?.snapshot
    if (!current) return
    const keys = new Set([
      ...current.allowances.filter((item) => item.guarded).map((item) => item.key),
      ...current.missing_guarded_keys,
    ])
    if (checked) keys.add(key)
    else keys.delete(key)
    const interruptedGuardRead = this.#guardReadEpochs.has(id)
    this.#savingGuards.add(id)
    this.#guardErrors.delete(id)
    // cancelQueries 阻止旧 GET 写缓存；代际还阻止旧 GET 的凭据失效副作用。
    this.#readEpochs.set(id, (this.#readEpochs.get(id) ?? 0) + 1)
    this.#changed()
    let confirmed = false
    let epoch = this.#readEpochs.get(id) ?? 0
    try {
      await this.#deps.client.cancelQueries({ queryKey: snapshotKey(id) })
      if (this.#disposed) return
      const update = await this.#deps.api.replaceGuards(id, [...keys])
      if (this.#disposed) return
      this.#metadataEpoch += 1
      // 取消已在飞的元数据，不等待网络；防止旧 providers 更新时间洗掉新凭据证据。
      await Promise.all([
        this.#deps.client.cancelQueries({ queryKey: providersKey, exact: true }),
        this.#deps.client.cancelQueries({ queryKey: targetsKey, exact: true }),
      ])
      if (this.#disposed) return
      // 保存响应只确认配置；旧数值的时间戳与 freshness 原样保留。
      epoch = (this.#readEpochs.get(id) ?? 0) + 1
      this.#readEpochs.set(id, epoch)
      const confirmedKeys = new Set(update.keys)
      const cached = update.target.snapshot ?? current
      const snapshot = update.target.snapshot ?? {
        ...cached,
        suspension: update.target.suspension,
        allowances: cached.allowances.map((item) => ({ ...item, guarded: confirmedKeys.has(item.key) })),
        models: cached.models.map((model) => ({
          ...model,
          allowances: model.allowances.map((item) => ({ ...item, guarded: confirmedKeys.has(item.key) })),
        })),
        missing_guarded_keys: update.keys.filter((key) => !cached.allowances.some((item) => item.key === key)),
      }
      this.#deps.client.setQueryData<ProviderAllowanceTarget[]>(targetsKey, (targets) =>
        targets?.map((target) => (target.provider_id === id ? { ...update.target, snapshot } : target)),
      )
      this.#publish(snapshot)
      this.#refreshErrors.delete(id)
      confirmed = true
    } catch (error) {
      if (!this.#disposed) this.#guardErrors.set(id, error)
    } finally {
      this.#savingGuards.delete(id)
      this.#changed()
    }
    // 失败写入没有改变已确认守护；恢复它打断的那次读取，不对普通保存失败额外读上游。
    if (confirmed || interruptedGuardRead) void this.#finishGuardRefresh(id, epoch)
  }

  async #finishGuardRefresh(id: string, epoch: number): Promise<void> {
    const current = () => !this.#disposed && epoch === this.#readEpochs.get(id)
    this.#guardReadEpochs.set(id, epoch)
    try {
      const result = await this.#snapshots.get(id)?.refetch({ cancelRefetch: false })
      if (this.#guardReadEpochs.get(id) === epoch) this.#guardReadEpochs.delete(id)
      if (!current()) return
      if (result?.isError) throw result.error
      await this.#refreshMetadata()
    } catch (error) {
      if (current()) this.#refreshErrors.set(id, error)
    } finally {
      if (this.#guardReadEpochs.get(id) === epoch) this.#guardReadEpochs.delete(id)
      if (current()) {
        this.#deps.client.setQueryData<ProviderAllowanceTarget[]>(targetsKey, (targets) =>
          targets?.map((target) => (target.provider_id === id ? { ...target, refreshing: false } : target)),
        )
        this.#changed()
      }
    }
  }

  #storage(): Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> {
    return this.#deps.storage ?? sessionStorage
  }

  #resetStorageKey(id: string): string {
    return `provider-allowance-reset:${id}`
  }

  #pendingResetGrantId(id: string): string | undefined {
    try {
      const raw = this.#storage().getItem(this.#resetStorageKey(id))
      if (!raw) return undefined
      const pending: unknown = JSON.parse(raw)
      return pending != null &&
        typeof pending === 'object' &&
        'grant_id' in pending &&
        typeof pending.grant_id === 'string'
        ? pending.grant_id
        : undefined
    } catch {
      return undefined
    }
  }

  #target(id: string): ProviderAllowanceTarget | undefined {
    return this.#targets.getCurrentResult().data?.find((target) => target.provider_id === id)
  }

  #busy(id: string): boolean {
    return this.#savingGuards.has(id) || this.#refreshingProviders.has(id) || this.#resettingProviders.has(id)
  }

  #now(): number {
    return (this.#deps.now ?? Date.now)()
  }

  #credentialInvalid(id: string): boolean {
    const providers = this.#providers.getCurrentResult()
    if (providers.data?.find((provider) => provider.id === id)?.credential_status === 'invalid') return true
    const discoveredAt = this.#discoveredInvalidAt.get(id) ?? 0
    return discoveredAt > 0 && providers.dataUpdatedAt <= discoveredAt
  }

  #remember(snapshot: ProviderAllowanceSnapshot): void {
    if (invalidSnapshot(snapshot)) this.#discoveredInvalidAt.set(snapshot.provider_id, this.#now())
  }

  #publish(snapshot: ProviderAllowanceSnapshot): void {
    if (!this.#disposed) this.#remember(snapshot)
    this.#deps.client.setQueryData(snapshotKey(snapshot.provider_id), snapshot)
    this.#changed()
  }

  async #refreshMetadata(): Promise<void> {
    await Promise.all([
      this.#deps.client.invalidateQueries({ queryKey: providersKey }, { throwOnError: true }),
      this.#deps.client.invalidateQueries({ queryKey: targetsKey, exact: true }, { throwOnError: true }),
    ])
  }

  #changed(): void {
    if (this.#disposed || !this.#started || this.#updatingQueries) return
    this.#updatingQueries = true
    try {
      const targets = this.#targets.getCurrentResult().data ?? []
      const ids = new Set(targets.map((target) => target.provider_id))
      for (const [id, observer] of this.#snapshots) {
        if (!ids.has(id)) {
          observer.destroy()
          this.#snapshots.delete(id)
        }
      }
      for (const target of targets) {
        const id = target.provider_id
        const invalid = this.#credentialInvalid(id)
        const enabled =
          !this.#providers.getCurrentResult().isPending &&
          !invalid &&
          !this.#savingGuards.has(id) &&
          !this.#resettingProviders.has(id)
        const interval = invalid ? false : REFRESH_INTERVAL
        let observer = this.#snapshots.get(id)
        if (!observer) {
          observer = new QueryObserver<ProviderAllowanceSnapshot>(this.#deps.client, {
            queryKey: snapshotKey(id),
            queryFn: async () => {
              const epoch = this.#readEpochs.get(id) ?? 0
              const snapshot = await this.#deps.api.get(id)
              if (this.#disposed || epoch !== (this.#readEpochs.get(id) ?? 0)) {
                return this.#deps.client.getQueryData<ProviderAllowanceSnapshot>(snapshotKey(id)) ?? snapshot
              }
              this.#remember(snapshot)
              return snapshot
            },
            initialData: target.snapshot,
            enabled,
            refetchInterval: interval,
            retry: (failureCount) => !this.#credentialInvalid(id) && failureCount < 3,
          })
          this.#snapshots.set(id, observer)
          observer.subscribe(() => this.#changed())
        } else if (observer.options.enabled !== enabled || observer.options.refetchInterval !== interval) {
          observer.setOptions({ ...observer.options, enabled, refetchInterval: interval })
        }
      }
    } finally {
      this.#updatingQueries = false
    }
    this.#deps.onSnapshot?.(this.snapshot)
  }
}

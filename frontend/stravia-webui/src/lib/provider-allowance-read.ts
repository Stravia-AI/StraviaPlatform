import { QueryObserver, type QueryClient } from '@tanstack/svelte-query'
import type { Provider, ProviderAllowanceSnapshot, ProviderAllowanceTarget } from './types'

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
  replaceGuards(id: string, keys: string[]): Promise<ProviderAllowanceSnapshot>
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

interface ProviderAllowanceReadDeps {
  client: QueryClient
  api: ProviderAllowanceReadApi
  onSnapshot?: (snapshot: ProviderAllowanceReadSnapshot) => void
  now?: () => number
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
  readonly #savingGuards = new Set<string>()
  readonly #refreshingProviders = new Set<string>()
  readonly #guardErrors = new Map<string, unknown>()
  #refreshingAll = false
  #started = false
  #disposed = false
  #updatingQueries = false

  constructor(deps: ProviderAllowanceReadDeps) {
    this.#deps = deps
    this.#providers = new QueryObserver<AllowanceProvider[]>(deps.client, {
      queryKey: providersKey,
      queryFn: () => deps.api.providers(),
      refetchInterval: REFRESH_INTERVAL,
      enabled: false,
    })
    this.#targets = new QueryObserver<ProviderAllowanceTarget[]>(deps.client, {
      queryKey: targetsKey,
      queryFn: async () => {
        const targets = await deps.api.list()
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
      const refreshing = !invalid && (this.#refreshingProviders.has(id) || Boolean(result?.isFetching))
      return {
        target,
        snapshot,
        pending: snapshot == null && !failed && !invalid,
        failed,
        refreshing,
        credentialInvalid: invalid,
        canRefresh:
          !this.#disposed &&
          !invalid &&
          !providers.isPending &&
          !refreshing &&
          !this.#refreshingAll &&
          !this.#savingGuards.has(id),
        canSaveGuards: !this.#disposed && !this.#busy(id),
        canRetry: !this.#disposed && Boolean(result) && !invalid && !this.#savingGuards.has(id),
        guardError: this.#guardErrors.get(id),
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
    if (this.#disposed || this.#credentialInvalid(id) || this.#savingGuards.has(id)) return
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
      await this.#refreshMetadata()
      return this.#disposed ? { status: 'ignored' } : { status: 'refreshed' }
    } catch (error) {
      return this.#disposed ? { status: 'ignored' } : { status: 'failed', error }
    } finally {
      this.#refreshingProviders.delete(id)
      this.#changed()
    }
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
    this.#savingGuards.add(id)
    this.#guardErrors.delete(id)
    // cancelQueries 阻止旧 GET 写缓存；代际还阻止旧 GET 的凭据失效副作用。
    this.#readEpochs.set(id, (this.#readEpochs.get(id) ?? 0) + 1)
    this.#changed()
    try {
      await this.#deps.client.cancelQueries({ queryKey: snapshotKey(id) })
      if (this.#disposed) return
      this.#publish(await this.#deps.api.replaceGuards(id, [...keys]))
      await this.#refreshMetadata()
    } catch (error) {
      if (!this.#disposed) this.#guardErrors.set(id, error)
    } finally {
      this.#savingGuards.delete(id)
      this.#changed()
    }
  }

  #target(id: string): ProviderAllowanceTarget | undefined {
    return this.#targets.getCurrentResult().data?.find((target) => target.provider_id === id)
  }

  #busy(id: string): boolean {
    return this.#savingGuards.has(id) || this.#refreshingProviders.has(id)
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
      this.#deps.client.invalidateQueries({ queryKey: providersKey }),
      this.#deps.client.invalidateQueries({ queryKey: targetsKey, exact: true }),
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
        const enabled = !this.#providers.getCurrentResult().isPending && !invalid && !this.#savingGuards.has(id)
        const interval = invalid ? false : REFRESH_INTERVAL
        let observer = this.#snapshots.get(id)
        if (!observer) {
          observer = new QueryObserver<ProviderAllowanceSnapshot>(this.#deps.client, {
            queryKey: snapshotKey(id),
            queryFn: async () => {
              const epoch = this.#readEpochs.get(id) ?? 0
              const snapshot = await this.#deps.api.get(id)
              if (!this.#disposed && epoch === (this.#readEpochs.get(id) ?? 0)) {
                this.#remember(snapshot)
                this.#changed()
              }
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

import { CanvasLinkIndex } from '$lib/interaction-canvas-links'
import { hiddenFailureNode } from '$lib/observation-chain-visibility'
import { mergeObservationRuns, retainLiveBlocks, withoutCommittedBlocks } from '$lib/observation-state'
import type { ObservationSubscription } from '$lib/observation-stream'
import type {
  BundleResourceKind,
  FailedRequestDetail,
  FailedRequestPage,
  FailedRequestQuery,
  FailedRequestSummary,
  ForestPage,
  ForestQuery,
  ForestRoot,
  InteractionDetail,
  InteractionEventsPage,
  InteractionEventsQuery,
  InteractionSummary,
  LiveContentBlock,
  ObservationStreamUpdate,
  ObservationLiveUpdate,
  RootChangesQuery,
  RootChangesPage,
} from '$lib/types/observation'

export const OBSERVATION_BATCH_SIZE = 12
export const OBSERVATION_MAX_WINDOW_MS = 86_400_000
export const OBSERVATION_DEFAULT_WINDOW_MS = 10 * 60_000
export const OBSERVATION_PRESET_MINUTES = [5, 10, 30, 60, 240, 720, 1440]
export const OBSERVATION_MIN_TOKEN_STOPS = [
  0, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000, 1_000_000,
]
export const OBSERVATION_DEFAULT_MIN_TOKEN_STOP = 4
const FAILURE_BATCH_SIZE = 30
export const OBSERVATION_REFRESH_WINDOW_MS = 100

export function isValidObservationRange(startMs: number, endMs: number): boolean {
  return (
    Number.isFinite(startMs) &&
    Number.isFinite(endMs) &&
    endMs > startMs &&
    endMs - startMs <= OBSERVATION_MAX_WINDOW_MS
  )
}

/** logs 页需要的 admin.observations 窄面；admin.observations 结构满足此接口。 */
export interface ObservationWorkspaceApi {
  forest(query: ForestQuery): Promise<ForestPage>
  changes(query: RootChangesQuery): Promise<RootChangesPage>
  failures(query: FailedRequestQuery): Promise<FailedRequestPage>
  interaction(id: string, query?: ForestQuery): Promise<InteractionDetail>
  interactionEvents(id: string, query: InteractionEventsQuery): Promise<InteractionEventsPage>
  failure(kind: FailedRequestSummary['kind'], id: string): Promise<FailedRequestDetail>
}

export type ObservationWorkspaceSubscribe = (
  snapshotSequence: number,
  onUpdate: (update: ObservationStreamUpdate) => void | Promise<void>,
  onConnectionChange: (connected: boolean) => void,
) => ObservationSubscription

export type ObservationWorkspaceSubscribeLive = (
  interactionId: string,
  onUpdate: (update: ObservationLiveUpdate) => void,
) => ObservationSubscription

/** 控制器不触达 DOM：聚焦/适配由页面以 tick+canvas 实现；错误由页面 toast。 */
export interface ObservationWorkspaceHooks {
  focusLatest(): void | Promise<void>
  focusNode(id: string): void | Promise<void>
  fitAfterAllLoaded(): void | Promise<void>
  onError(error: unknown): void
}

export interface ObservationWorkspaceDeps {
  api: ObservationWorkspaceApi
  subscribe: ObservationWorkspaceSubscribe
  subscribeLive: ObservationWorkspaceSubscribeLive
  hooks: ObservationWorkspaceHooks
  onSnapshot?: (snapshot: ObservationWorkspaceSnapshot) => void
  now?: () => number
}

export interface ObservationWorkspaceSnapshot {
  windowStart: number
  windowEnd: number
  durationMs: number
  customRange: boolean
  liveWindow: boolean
  activeTab: string
  providerFilter: string
  modelFilter: string
  apiKeyFilter: string
  statusFilter: string
  minTokenStop: number
  minTokensFilter: number
  activeFilterCount: number
  canvasRoots: ForestRoot[]
  latestInteraction?: InteractionSummary
  selectedPath: Set<string>
  selectedMigrated?: string
  migratedRoots: ReadonlySet<string>
  selectedInteraction?: InteractionSummary
  selectedFailure?: FailedRequestSummary
  interactionDetail?: InteractionDetail
  failureDetail?: FailedRequestDetail
  failureDetailError: unknown
  detailLoading: boolean
  olderLoading: boolean
  selectedLiveBlocks: LiveContentBlock[]
  liveContentEpoch: number
  selectedLiveActive: boolean
  liveTerminalEpoch: number
  detailError: unknown
  liveGaps: string[]
  liveCapacityGaps: string[]
  streamConnected: boolean
  followPaused: boolean
  hasNewActivity: boolean
  fitProgress?: number
  loading: boolean
  loadingMore: boolean
  loadError: unknown
  nextCursor: string | null | undefined
  rootTotal: number
  failures: FailedRequestSummary[]
  failureTotal: number
  failureCursor: string | null | undefined
  failureLoading: boolean
  failureError: unknown
}

/**
 * logs 页观测工作区的编排状态机：roots/cursor 和解、live 窗口、selection 竞态守卫、
 * live-block 保留、reset 恢复、failures 分页与深链 reveal 都收敛在这里；
 * 页面只持有 DOM、对话框与 toast。竞态用内部 epoch 计数，调用方不再见到版本号。
 */
export class ObservationWorkspaceController {
  readonly #api: ObservationWorkspaceApi
  readonly #subscribe: ObservationWorkspaceSubscribe
  readonly #subscribeLive: ObservationWorkspaceSubscribeLive
  readonly #hooks: ObservationWorkspaceHooks
  readonly #listener: (snapshot: ObservationWorkspaceSnapshot) => void
  readonly #now: () => number

  #rangeVersion = 0
  #selectionVersion = 0
  #failureRequestVersion = 0

  #anchorAt: number
  #windowIndex = 0
  #durationMs = OBSERVATION_DEFAULT_WINDOW_MS
  #customRange = false
  #windowStart: number
  #windowEnd: number

  #activeTab = 'interactions'
  #providerFilter = 'all'
  #modelFilter = 'all'
  #apiKeyFilter = 'all'
  #statusFilter = 'all'
  #minTokenStop = OBSERVATION_DEFAULT_MIN_TOKEN_STOP

  #roots: ForestRoot[] = []
  #rootTotal = 0
  #rootTotalSequence = 0
  #nextCursor: string | null | undefined
  #snapshotSequence = 0
  #appliedSequence = 0
  #pendingRoots = new Map<string, number>()
  #rootErrors = new Map<string, unknown>()
  #rootSequences = new Map<string, number>()
  #inFlightRoots = new Map<string, number>()
  #refreshDeadline: number | undefined
  #refreshTimer: ReturnType<typeof setTimeout> | undefined
  #eventsRequest: Promise<void> | undefined
  #eventsPending = 0
  #failurePending = 0
  #failureSequence = 0
  #failureFlight: Promise<void> | undefined
  #failureQueryRequest: Promise<void> | undefined
  #failureQueryEpoch = -1
  #liveStream: ObservationSubscription | undefined
  #liveContentEpoch = 0
  #selectedLiveActive = false
  #finishedLiveRuns = new Set<string>()
  #liveTerminalEpoch = 0
  #detailError: unknown
  #disposed = false
  #loading = true
  #loadingMore = false
  #loadError: unknown
  #rootBatchRequest: Promise<void> | undefined

  #selectedInteraction: InteractionSummary | undefined
  #selectedFailure: FailedRequestSummary | undefined
  #interactionDetail: InteractionDetail | undefined
  #interactionDetailEpoch = -1
  #resetRecoveryPending = false
  #failureDetail: FailedRequestDetail | undefined
  #failureDetailError: unknown
  #detailLoading = false
  #olderLoading = false

  #liveBlocks: LiveContentBlock[] = []
  #liveGaps: string[] = []
  #liveCapacityGaps: string[] = []
  #stream: ObservationSubscription | undefined
  #streamConnected = false

  #followPaused = false
  #hasNewActivity = false
  #migratedRoots = new Set<string>()
  // 交互链路成员规则：零客户端可见输出且最终失败的交互默认不进画布；「失败的请求」页
  // 跳转、深链与跟随聚焦把它显式 reveal 后才可见。
  #revealedFailures = new Set<string>()
  #fitProgress: number | undefined

  #failures: FailedRequestSummary[] = []
  #failureTotal = 0
  #failureCursor: string | null | undefined
  #failureLoading = false
  #failureError: unknown

  constructor(deps: ObservationWorkspaceDeps) {
    this.#api = deps.api
    this.#subscribe = deps.subscribe
    this.#subscribeLive = deps.subscribeLive
    this.#hooks = deps.hooks
    this.#listener = deps.onSnapshot ?? (() => undefined)
    this.#now = deps.now ?? Date.now
    this.#anchorAt = this.#now()
    this.#windowEnd = this.#anchorAt
    this.#windowStart = this.#windowEnd - this.#durationMs
  }

  async start(): Promise<void> {
    this.#updateLiveBounds()
    await this.#loadForest(true)
  }

  dispose(): void {
    this.#disposed = true
    this.#rangeVersion += 1
    this.#selectionVersion += 1
    this.#stream?.close()
    this.#liveStream?.close()
    if (this.#refreshTimer) clearTimeout(this.#refreshTimer)
  }

  /** 页面秒针：推进 live 窗口边界，窗口内过期内容触发整页重查而非本地过滤。 */
  advanceClock(): void {
    if (this.#resetRecoveryPending) {
      this.#updateLiveBounds()
      this.#publish()
      return
    }
    if (this.#refreshDeadline !== undefined && this.#now() >= this.#refreshDeadline) {
      if (this.#refreshTimer) clearTimeout(this.#refreshTimer)
      this.#refreshTimer = undefined
      this.#refreshDeadline = undefined
      this.#flushRefresh()
    }
    if (!this.#liveWindow) return
    this.#updateLiveBounds()
    if (!this.#loading && !this.#loadingMore && this.#roots.some((root) => root.last_active_at < this.#windowStart)) {
      void this.#loadForest(true)
    }
    if (
      this.#activeTab === 'failures' &&
      !this.#failureLoading &&
      this.#failures.some((item) => item.started_at < this.#windowStart)
    ) {
      void this.loadFailures()
    }
    this.#publish()
  }

  async choosePreset(minutes: number): Promise<void> {
    if (!OBSERVATION_PRESET_MINUTES.includes(minutes)) return
    this.#durationMs = minutes * 60_000
    this.#customRange = false
    this.#anchorAt = this.#now()
    this.#windowIndex = 0
    this.#updateLiveBounds()
    await this.#reloadWindow()
  }

  async applyRange(startMs: number, endMs: number): Promise<void> {
    if (!isValidObservationRange(startMs, endMs)) return
    this.#windowStart = startMs
    this.#windowEnd = endMs
    this.#durationMs = endMs - startMs
    this.#customRange = true
    this.#windowIndex = 0
    await this.#reloadWindow()
  }

  async changeWindow(delta: number): Promise<void> {
    if (this.#customRange) {
      this.#windowStart -= delta * this.#durationMs
      this.#windowEnd -= delta * this.#durationMs
    } else {
      if (this.#liveWindow) this.#anchorAt = this.#windowEnd
      this.#windowIndex = Math.max(0, this.#windowIndex + delta)
      this.#windowEnd = this.#anchorAt - this.#windowIndex * this.#durationMs
      this.#windowStart = this.#windowEnd - this.#durationMs
      this.#updateLiveBounds()
    }
    await this.#reloadWindow()
  }

  async refreshAnchor(focusId?: string): Promise<void> {
    if (focusId) this.#revealedFailures = new Set([...this.#revealedFailures, focusId])
    if (focusId || !this.#customRange) {
      this.#customRange = false
      this.#anchorAt = this.#now()
      this.#windowIndex = 0
      this.#updateLiveBounds()
    }
    await this.#reloadWindow()
    if (focusId) {
      const interaction = this.#canvasInteractions().find((item) => item.id === focusId)
      if (interaction) await this.selectInteraction(interaction)
    }
  }

  async reloadForest(): Promise<void> {
    if (this.#resetRecoveryPending) return this.#retryReset()
    await this.#loadForest(true)
    this.#scheduleRefresh(true)
  }

  /** 外部历史变更使原基线失效：与 SSE reset 共用完整恢复，保留尚可读取的 selection。 */
  refreshData(): Promise<void> {
    return this.#retryReset()
  }

  async applyFilters(): Promise<void> {
    this.#rangeVersion += 1
    this.#resetRefreshState()
    this.closeInspector()
    this.#followPaused = !this.#liveWindow
    this.#hasNewActivity = false
    this.#migratedRoots = new Set()
    // 先推进实时窗口再查询，两次推进之间完成的失败不会被旧 end_at 排除。
    if (this.#activeTab === 'failures') {
      this.#updateLiveBounds()
      await this.loadFailures()
    } else await this.#loadForest(true)
  }

  clearFilters(): void {
    this.#providerFilter = 'all'
    this.#modelFilter = 'all'
    this.#apiKeyFilter = 'all'
    this.#statusFilter = 'all'
    // 滑到 0 才显示被默认 10k 阈值隐藏的小链路。
    this.#minTokenStop = 0
    this.#publish()
  }

  setProviderFilter(value: string): void {
    this.#providerFilter = value
    this.#publish()
  }

  setModelFilter(value: string): void {
    this.#modelFilter = value
    this.#publish()
  }

  setApiKeyFilter(value: string): void {
    this.#apiKeyFilter = value
    this.#publish()
  }

  setStatusFilter(value: string): void {
    this.#statusFilter = value
    this.#publish()
  }

  setMinTokenStop(value: number): void {
    this.#minTokenStop = value
    this.#publish()
  }

  async tabChanged(value: string): Promise<void> {
    this.#rangeVersion += 1
    this.#resetRefreshState()
    this.#activeTab = value
    this.closeInspector()
    this.#publish()
    // 先推进实时窗口再查询，两次推进之间完成的失败不会被旧 end_at 排除。
    if (value === 'failures') {
      this.#updateLiveBounds()
      await this.loadFailures()
    } else await this.#loadForest(true)
  }

  loadNextRootBatch(): Promise<void> {
    if (this.#rootBatchRequest) return this.#rootBatchRequest
    if (!this.#nextCursor) return Promise.resolve()
    this.#rootBatchRequest = this.#loadForest(false).finally(() => {
      this.#rootBatchRequest = undefined
    })
    return this.#rootBatchRequest
  }

  async fitAll(): Promise<void> {
    if (!this.#nextCursor) {
      await this.#hooks.fitAfterAllLoaded()
      return
    }
    this.#fitProgress = Math.round((this.#roots.length / Math.max(this.#rootTotal, 1)) * 100)
    this.#publish()
    while (this.#nextCursor) {
      await this.loadNextRootBatch()
      this.#fitProgress = Math.round((this.#roots.length / Math.max(this.#rootTotal, 1)) * 100)
      this.#publish()
      if (this.#loadError) break
    }
    await this.#hooks.fitAfterAllLoaded()
    this.#fitProgress = undefined
    this.#publish()
  }

  async selectInteraction(interaction: InteractionSummary): Promise<void> {
    this.closeInspector()
    const selection = this.#selectionVersion
    this.#selectedInteraction = interaction
    this.#openLive(interaction.id, selection)
    this.#detailLoading = true
    if (interaction.id !== this.#latestInteraction()?.id) this.#followPaused = true
    this.#publish()
    try {
      this.#applySelectedDetail(await this.#api.interaction(interaction.id, this.#query()), selection)
      await this.#refreshSelectedEvents(interaction.id, selection)
    } catch (error) {
      if (selection === this.#selectionVersion) {
        if (!this.#removeMissingSelection(error, selection)) this.#detailError = error
        this.#hooks.onError(error)
      }
    } finally {
      if (selection === this.#selectionVersion) this.#detailLoading = false
      this.#publish()
    }
  }

  async selectFailure(failure: FailedRequestSummary): Promise<void> {
    this.closeInspector()
    const selection = this.#selectionVersion
    this.#selectedFailure = failure
    this.#detailLoading = true
    this.#publish()
    try {
      const detail = await this.#api.failure(failure.kind, failure.id)
      if (selection === this.#selectionVersion) this.#failureDetail = detail
    } catch (error) {
      if (selection === this.#selectionVersion) this.#failureDetailError = error
    } finally {
      if (selection === this.#selectionVersion) this.#detailLoading = false
      this.#publish()
    }
  }

  /** 深链（page.url ?interaction=）：reveal + 选中 + 补齐事件。 */
  async deepLinkInteraction(id: string): Promise<void> {
    this.closeInspector()
    const selection = this.#selectionVersion
    this.#followPaused = true
    if (!this.#revealedFailures.has(id)) this.#revealedFailures = new Set([...this.#revealedFailures, id])
    this.#detailLoading = true
    this.#publish()
    try {
      this.#applySelectedDetail(await this.#api.interaction(id), selection)
      await this.#refreshSelectedEvents(id, selection)
    } catch (error) {
      if (selection === this.#selectionVersion) this.#hooks.onError(error)
    } finally {
      if (selection === this.#selectionVersion) this.#detailLoading = false
      this.#publish()
    }
  }

  closeInspector(): void {
    this.#closeLiveScope()
    this.#liveBlocks = []
    this.#selectedInteraction = undefined
    this.#selectedFailure = undefined
    this.#interactionDetail = undefined
    this.#failureDetail = undefined
    this.#failureDetailError = undefined
    this.#detailLoading = false
    this.#publish()
  }

  async loadOlderEvents(): Promise<void> {
    const detail = this.#interactionDetail
    if (!detail || detail.older_events_cursor === null || this.#olderLoading) return
    const selection = this.#selectionVersion
    const id = detail.interaction.id
    this.#olderLoading = true
    this.#publish()
    try {
      const page = await this.#api.interactionEvents(id, {
        before_sequence: detail.older_events_cursor,
        through_sequence: detail.snapshot_sequence,
      })
      if (selection !== this.#selectionVersion || !this.#interactionDetail) return
      this.#interactionDetail = {
        ...this.#interactionDetail,
        runs: mergeObservationRuns(this.#interactionDetail.runs, page.runs, true),
        older_events_cursor: page.next_cursor,
      }
      this.#liveBlocks = withoutCommittedBlocks(this.#liveBlocks, this.#interactionDetail)
    } catch (error) {
      if (selection === this.#selectionVersion) this.#hooks.onError(error)
    } finally {
      if (selection === this.#selectionVersion) this.#olderLoading = false
      this.#publish()
    }
  }

  async openFailureInteraction(): Promise<void> {
    const id = this.#failureDetail?.request.interaction_id
    if (!id) return
    const selection = this.#selectionVersion
    try {
      const snapshot = await this.#api.interaction(id)
      if (selection !== this.#selectionVersion) return
      this.closeInspector()
      this.#activeTab = 'interactions'
      this.#followPaused = true
      this.#revealedFailures = new Set([...this.#revealedFailures, id])
      this.#roots = [...this.#roots.filter((root) => root.id !== snapshot.root.id), snapshot.root]
      this.#rootSequences.set(snapshot.root.id, snapshot.snapshot_sequence)
      this.#publish()
      await this.#hooks.focusNode(id)
    } catch (error) {
      this.#hooks.onError(error)
    }
  }

  /** 诊断数据清除后重取当前选中项；无选中时是空操作。 */
  async refreshSelectedDetail(): Promise<void> {
    if (this.#resetRecoveryPending) {
      await this.#retryReset()
      return
    }
    this.#detailError = undefined
    const selection = this.#selectionVersion
    const interactionId = this.#selectedInteraction?.id
    const failure = this.#selectedFailure
    try {
      if (interactionId && selection === this.#selectionVersion) {
        this.#applySelectedDetail(await this.#api.interaction(interactionId, this.#query()), selection)
        await this.#refreshSelectedEvents(interactionId, selection)
      }
      if (failure && selection === this.#selectionVersion) {
        const detail = await this.#api.failure(failure.kind, failure.id)
        if (selection === this.#selectionVersion) this.#failureDetail = detail
      }
    } catch (error) {
      if (selection === this.#selectionVersion) {
        if (interactionId) {
          if (!this.#removeMissingSelection(error, selection)) this.#detailError = error
        } else this.#failureDetailError = error
      }
    } finally {
      this.#publish()
    }
  }

  loadFailures(replace = true): Promise<void> {
    if (this.#failureQueryRequest && this.#failureQueryEpoch === this.#rangeVersion) return this.#failureQueryRequest
    this.#failureQueryEpoch = this.#rangeVersion
    const request = this.#pullFailures(replace).finally(() => {
      if (this.#failureQueryRequest === request) this.#failureQueryRequest = undefined
    })
    this.#failureQueryRequest = request
    return request
  }

  async #pullFailures(replace: boolean): Promise<void> {
    if (!replace && this.#failureLoading) return
    const version = this.#rangeVersion
    const requestVersion = ++this.#failureRequestVersion
    this.#failureLoading = true
    this.#failureError = undefined
    this.#publish()
    try {
      const query = this.#query()
      const page = await this.#api.failures({
        start_at: this.#windowStart,
        end_at: this.#windowEnd,
        limit: FAILURE_BATCH_SIZE,
        cursor: replace ? undefined : (this.#failureCursor ?? undefined),
        provider: query.provider,
        model: query.model,
        api_key: query.api_key,
      })
      if (version !== this.#rangeVersion || requestVersion !== this.#failureRequestVersion) return
      this.#failures = replace
        ? page.items
        : [
            ...this.#failures,
            ...page.items.filter(
              (item) => !this.#failures.some((known) => known.kind === item.kind && known.id === item.id),
            ),
          ]
      this.#failureSequence = page.snapshot_sequence
      this.#failureTotal = page.total
      this.#failureCursor = page.next_cursor
    } catch (error) {
      if (version === this.#rangeVersion && requestVersion === this.#failureRequestVersion) this.#failureError = error
    } finally {
      if (version === this.#rangeVersion && requestVersion === this.#failureRequestVersion) this.#failureLoading = false
      this.#publish()
    }
  }

  pauseFollow(): void {
    this.#followPaused = true
    this.#publish()
  }

  resumeFollow(): void {
    this.#followPaused = false
    this.#hasNewActivity = false
    this.#publish()
  }

  bundleTarget(): { kind: BundleResourceKind; id: string } | null {
    const kind =
      this.#interactionDetail || this.#failureDetail?.request.interaction_id ? 'interaction' : 'rejected_request'
    const id =
      this.#interactionDetail?.interaction.id ??
      this.#failureDetail?.request.interaction_id ??
      this.#failureDetail?.request.id
    return id ? { kind, id } : null
  }

  get #liveWindow(): boolean {
    return !this.#customRange && this.#windowIndex === 0
  }

  #minTokens(): number {
    return OBSERVATION_MIN_TOKEN_STOPS[this.#minTokenStop] ?? 0
  }

  #query(): ForestQuery {
    const minTokens = this.#minTokens()
    return {
      live_window: this.#liveWindow,
      start_at: this.#windowStart,
      end_at: this.#windowEnd,
      limit: OBSERVATION_BATCH_SIZE,
      provider: this.#providerFilter === 'all' ? undefined : this.#providerFilter,
      model: this.#modelFilter === 'all' ? undefined : this.#modelFilter,
      api_key: this.#apiKeyFilter === 'all' ? undefined : this.#apiKeyFilter,
      status: this.#statusFilter === 'all' ? undefined : this.#statusFilter,
      min_tokens: this.#activeTab === 'interactions' && minTokens > 0 ? minTokens : undefined,
    }
  }

  #interactions(): InteractionSummary[] {
    return this.#roots.flatMap((root) => root.interactions)
  }

  #canvasRoots(): ForestRoot[] {
    return this.#roots
      .map((root) => ({
        ...root,
        interactions: root.interactions.filter(
          (item) => this.#revealedFailures.has(item.id) || !hiddenFailureNode(item),
        ),
      }))
      .filter((root) => root.interactions.length > 0)
  }

  #canvasInteractions(): InteractionSummary[] {
    return this.#canvasRoots().flatMap((root) => root.interactions)
  }

  #latestInteraction(): InteractionSummary | undefined {
    return [...this.#canvasInteractions()].sort(
      (a, b) => Number(b.status === 'running') - Number(a.status === 'running') || b.last_active_at - a.last_active_at,
    )[0]
  }

  #selectedPath(): Set<string> {
    const path = new Set<string>()
    let current = this.#selectedInteraction
    if (!current) return path
    // 一次构建视觉父边索引，整条选中路径共用快照内的祖先查询。
    const index = new CanvasLinkIndex(this.#canvasInteractions())
    while (current && !path.has(current.id)) {
      path.add(current.id)
      const parentId: string | undefined = index.visualParent(current)?.id
      current = parentId ? index.byId.get(parentId) : undefined
    }
    return path
  }

  #updateLiveBounds(): void {
    if (!this.#liveWindow) return
    this.#windowEnd = this.#now()
    this.#windowStart = this.#windowEnd - this.#durationMs
  }

  async #reloadWindow(): Promise<void> {
    this.#rangeVersion += 1
    this.#resetRefreshState()
    this.closeInspector()
    this.#migratedRoots = new Set()
    this.#followPaused = !this.#liveWindow
    this.#hasNewActivity = false
    this.#failures = []
    this.#failureCursor = undefined
    await Promise.all([
      this.#loadForest(true),
      this.#activeTab === 'failures' ? this.loadFailures() : Promise.resolve(),
    ])
  }

  #applyPage(page: ForestPage, replace: boolean, advanceStream: boolean): void {
    this.#roots = replace
      ? page.roots
      : [...this.#roots, ...page.roots.filter((root) => !this.#roots.some((known) => known.id === root.id))]
    if (replace || page.snapshot_sequence >= this.#rootTotalSequence) {
      this.#rootTotal = page.root_total
      this.#rootTotalSequence = page.snapshot_sequence
    }
    this.#nextCursor = page.next_cursor
    this.#snapshotSequence = page.snapshot_sequence
    this.#appliedSequence = Math.max(this.#appliedSequence, page.snapshot_sequence)
    if (replace) {
      for (const [id, sequence] of this.#pendingRoots) {
        if (sequence <= page.snapshot_sequence) {
          this.#pendingRoots.delete(id)
          this.#rootErrors.delete(id)
        }
      }
      this.#rootSequences = new Map(page.roots.map((root) => [root.id, page.snapshot_sequence]))
    }
    if (replace && advanceStream) {
      this.#resetRecoveryPending = false
      this.#stream?.setCursor(page.snapshot_sequence)
    }
    if (!this.#stream) {
      this.#stream = this.#subscribe(
        page.snapshot_sequence,
        (update) => {
          if (update.type === 'reset_required') return this.#onStreamUpdate(update)
          void this.#onStreamUpdate(update).catch((error) => {
            if (!this.#disposed) {
              this.#loadError = error
              this.#publish()
            }
          })
        },
        (connected) => {
          this.#streamConnected = connected
          this.#publish()
        },
      )
    }
  }

  async #loadForest(replace: boolean, advanceStream = true): Promise<void> {
    const version = this.#rangeVersion
    if (replace) {
      this.#loading = true
      this.#loadError = undefined
    } else this.#loadingMore = true
    this.#publish()
    try {
      const page = await this.#api.forest({
        ...this.#query(),
        cursor: replace ? undefined : (this.#nextCursor ?? undefined),
      })
      if (version !== this.#rangeVersion) return
      if (replace && page.snapshot_sequence < this.#appliedSequence) return
      this.#applyPage(page, replace, advanceStream)
      this.#loadError = this.#rootErrors.values().next().value
      this.#publish()
      if (replace && this.#liveWindow && !this.#followPaused) await this.#hooks.focusLatest()
    } catch (error) {
      if (version === this.#rangeVersion) this.#loadError = error
    } finally {
      if (version === this.#rangeVersion) {
        this.#loading = false
        this.#loadingMore = false
        this.#publish()
      }
    }
  }

  #applySelectedDetail(detail: InteractionDetail, selection: number): void {
    if (selection !== this.#selectionVersion) return
    if (
      this.#interactionDetailEpoch === selection &&
      this.#interactionDetail &&
      this.#interactionDetail.snapshot_sequence > detail.snapshot_sequence
    )
      return
    this.#selectedInteraction = detail.interaction
    if (!this.#liveStream) this.#openLive(detail.interaction.id, selection)
    this.#interactionDetail = detail
    this.#interactionDetailEpoch = selection
    this.#liveBlocks = withoutCommittedBlocks(this.#liveBlocks, detail)
    this.#detailLoading = false
    this.#publish()
  }

  #refreshSelectedEvents(id: string, selection: number): Promise<void> {
    if (this.#eventsRequest) return this.#eventsRequest
    const request = this.#pullSelectedEvents(id, selection)
      .catch((error) => {
        if (selection === this.#selectionVersion) {
          if (!this.#removeMissingSelection(error, selection)) this.#detailError = error
          this.#hooks.onError(error)
          this.#publish()
        }
      })
      .finally(() => {
        if (this.#eventsRequest !== request) return
        this.#eventsRequest = undefined
        if (!this.#detailError && this.#eventsPending > (this.#interactionDetail?.snapshot_sequence ?? 0))
          this.#scheduleRefresh(false)
      })
    this.#eventsRequest = request
    return request
  }

  async #pullSelectedEvents(id: string, selection: number): Promise<void> {
    if (!this.#interactionDetail || selection !== this.#selectionVersion) return
    let after = this.#interactionDetail.snapshot_sequence
    let through: number | undefined
    do {
      const page = await this.#api.interactionEvents(id, { after_sequence: after, through_sequence: through })
      if (selection !== this.#selectionVersion || !this.#interactionDetail) return
      through ??= page.snapshot_sequence
      this.#interactionDetail = {
        ...this.#interactionDetail,
        runs: mergeObservationRuns(
          this.#interactionDetail.runs,
          page.runs,
          page.snapshot_sequence < this.#interactionDetail.snapshot_sequence,
        ),
        snapshot_sequence:
          page.next_cursor === null
            ? Math.max(through, this.#interactionDetail.snapshot_sequence)
            : this.#interactionDetail.snapshot_sequence,
      }
      this.#liveBlocks = withoutCommittedBlocks(this.#liveBlocks, this.#interactionDetail)
      this.#detailError = undefined
      this.#publish()
      if (page.next_cursor === null) return
      after = page.next_cursor
    } while (selection === this.#selectionVersion)
  }

  #applyLiveBlocks(blocks: LiveContentBlock[]): void {
    const retained = retainLiveBlocks(blocks, this.#selectedInteraction?.id)
    if (retained.length !== blocks.length) {
      const ids = new Set(retained.map((block) => block.block_id))
      this.#liveCapacityGaps = [
        ...new Set([
          ...this.#liveCapacityGaps,
          ...blocks.filter((block) => !ids.has(block.block_id)).map((block) => block.interaction_id),
        ]),
      ].slice(-64)
    }
    this.#liveBlocks = retained
  }

  #openLive(id: string, selection: number): void {
    this.#liveStream?.close()
    this.#liveStream = this.#subscribeLive(id, (update) => {
      if (selection !== this.#selectionVersion || this.#disposed) return
      if (
        update.type !== 'live_snapshot' &&
        (update.type === 'live_content' ? update.block.interaction_id : update.interaction_id) !== id
      )
        return
      this.#onLiveUpdate(update)
    })
  }

  #onLiveUpdate(update: ObservationLiveUpdate): void {
    if (update.type === 'live_finished') {
      this.#finishLiveRun(update.run_id)
      this.#publish()
      return
    }
    if (update.type === 'live_content') {
      const previous = this.#liveBlocks.find((block) => block.block_id === update.block.block_id)
      if (previous && previous.revision >= update.block.revision) return
      let blocks = previous
        ? this.#liveBlocks.map((block) => (block.block_id === update.block.block_id ? update.block : block))
        : [...this.#liveBlocks, update.block]
      if (this.#interactionDetail) blocks = withoutCommittedBlocks(blocks, this.#interactionDetail)
      this.#applyLiveBlocks(blocks)
      this.#selectedLiveActive = !this.#finishedLiveRuns.has(update.block.run_id)
      this.#publish()
      return
    }
    if (update.type === 'live_snapshot') {
      this.#liveContentEpoch += 1
      this.#selectedLiveActive = update.blocks.length > 0 && this.#selectedInteraction?.status === 'running'
      this.#applyLiveBlocks(
        this.#interactionDetail
          ? withoutCommittedBlocks(
              update.blocks.filter((block) => block.interaction_id === this.#selectedInteraction?.id),
              this.#interactionDetail,
            )
          : update.blocks.filter((block) => block.interaction_id === this.#selectedInteraction?.id),
      )
      this.#publish()
      return
    }
    if (update.type === 'live_gap') {
      if (update.reason === 'live_capacity') {
        this.#liveCapacityGaps = [
          ...this.#liveCapacityGaps.filter((id) => id !== update.interaction_id),
          update.interaction_id,
        ].slice(-64)
      } else {
        this.#liveGaps = [...this.#liveGaps.filter((id) => id !== update.interaction_id), update.interaction_id].slice(
          -64,
        )
      }
      this.#publish()
      return
    }
  }

  #finishLiveRun(runId: string): void {
    if (this.#finishedLiveRuns.has(runId)) return
    this.#finishedLiveRuns.add(runId)
    this.#selectedLiveActive = false
    this.#liveTerminalEpoch += 1
  }

  #resetRefreshState(): void {
    this.#appliedSequence = 0
    this.#snapshotSequence = 0
    this.#rootSequences.clear()
    this.#rootTotalSequence = 0
    this.#pendingRoots.clear()
    this.#rootErrors.clear()
    this.#failurePending = 0
    this.#failureFlight = undefined
    if (this.#refreshTimer) clearTimeout(this.#refreshTimer)
    this.#refreshTimer = undefined
    this.#refreshDeadline = undefined
  }

  #closeLiveScope(): void {
    this.#selectionVersion += 1
    this.#olderLoading = false
    this.#liveStream?.close()
    this.#liveStream = undefined
    this.#liveContentEpoch += 1
    this.#eventsRequest = undefined
    this.#eventsPending = 0
    this.#selectedLiveActive = false
    this.#finishedLiveRuns.clear()
    this.#liveGaps = []
    this.#liveCapacityGaps = []
    this.#detailError = undefined
  }

  #removeMissingSelection(error: unknown, selection: number): boolean {
    if (
      selection !== this.#selectionVersion ||
      !error ||
      typeof error !== 'object' ||
      !('status' in error) ||
      error.status !== 404
    )
      return false
    const id = this.#selectedInteraction?.id
    if (!id) return false
    const count = this.#roots.length
    this.#roots = this.#roots
      .map((root) => ({ ...root, interactions: root.interactions.filter((item) => item.id !== id) }))
      .filter((root) => root.interactions.length > 0)
    this.#rootTotal = Math.max(0, this.#rootTotal - (count - this.#roots.length))
    this.closeInspector()
    return true
  }

  #retryReset(): Promise<void> {
    return this.#onStreamUpdate({ type: 'reset_required', snapshot_sequence: this.#snapshotSequence }).catch((error) =>
      this.#hooks.onError(error),
    )
  }

  async #onStreamUpdate(update: ObservationStreamUpdate): Promise<void> {
    if (this.#disposed) return
    this.#updateLiveBounds()
    if (update.type === 'reset_required') {
      this.#rangeVersion += 1
      const version = this.#rangeVersion
      this.#resetRefreshState()
      this.#resetRecoveryPending = true
      this.#closeLiveScope()
      const selection = this.#selectionVersion
      const interactionId = this.#selectedInteraction?.id
      const failure = this.#selectedFailure
      this.#detailLoading = Boolean(interactionId || failure)
      this.#failureDetailError = undefined
      this.#publish()
      try {
        await Promise.all([
          this.#loadForest(true, false),
          this.#activeTab === 'failures' ? this.loadFailures() : Promise.resolve(),
          interactionId
            ? this.#api
                .interaction(interactionId, this.#query())
                .then((detail) => this.#applySelectedDetail(detail, selection))
                .catch((error) => {
                  if (selection !== this.#selectionVersion) return
                  if (!this.#removeMissingSelection(error, selection)) {
                    this.#detailError = error
                    throw error
                  }
                })
            : failure
              ? this.#api.failure(failure.kind, failure.id).then((detail) => {
                  if (selection === this.#selectionVersion) this.#failureDetail = detail
                })
              : Promise.resolve(),
        ])
        if (version !== this.#rangeVersion) return
        if (this.#failureError && this.#activeTab === 'failures')
          throw this.#failureError instanceof Error
            ? this.#failureError
            : new Error(
                typeof this.#failureError === 'string' ? this.#failureError : 'observation failures reload failed',
              )
        if (this.#loadError) {
          const loadError = this.#loadError
          throw loadError instanceof Error
            ? loadError
            : new Error(typeof loadError === 'string' ? loadError : 'observation reload failed')
        }
        this.#resetRecoveryPending = false
        this.#stream?.setCursor(this.#snapshotSequence)
        this.#scheduleRefresh(true)
      } catch (error) {
        if (version !== this.#rangeVersion) return
        if (failure && selection === this.#selectionVersion) this.#failureDetailError = error
        this.#loadError = error
        this.#publish()
        throw error
      } finally {
        if (selection === this.#selectionVersion) this.#detailLoading = false
        this.#publish()
      }
      return
    }
    const streamEvent = update.event
    if (
      this.#liveWindow &&
      this.#activeTab === 'failures' &&
      ['request_rejected', 'run_finished'].includes(streamEvent.kind)
    ) {
      this.#failurePending = Math.max(this.#failurePending, streamEvent.sequence)
    }
    // 「新活动·跟随」只响应输出预览（visible_tail）的追加；思考条目与工具调用
    // 仍照常落库刷新卡片数据，但不点亮跟随入口、不移动视口。
    const extendsOutputPreview = streamEvent.kind === 'client_visible_content'
    if (this.#followPaused && extendsOutputPreview) this.#hasNewActivity = true
    if (!this.#liveWindow && streamEvent.interaction_id && streamEvent.occurred_at >= this.#windowEnd) {
      const root = this.#roots.find((item) =>
        item.interactions.some((interaction) => interaction.id === streamEvent.interaction_id),
      )
      if (root) this.#migratedRoots = new Set([...this.#migratedRoots, root.id])
    }
    this.#publish()
    const interactionId = streamEvent.interaction_id
    const known = this.#interactions().find((item) => item.id === interactionId)
    const rootId = streamEvent.root_id ?? known?.root_id
    if (rootId && this.#activeTab === 'interactions' && (!known || known.last_event_sequence < streamEvent.sequence)) {
      this.#pendingRoots.set(rootId, Math.max(this.#pendingRoots.get(rootId) ?? 0, streamEvent.sequence))
    }
    if (interactionId && interactionId === this.#selectedInteraction?.id) {
      this.#eventsPending = Math.max(this.#eventsPending, streamEvent.sequence)
      if (streamEvent.boundary && streamEvent.kind === 'run_finished') {
        if (streamEvent.run_id) this.#finishLiveRun(streamEvent.run_id)
      }
      if (streamEvent.kind === 'run_started') this.#selectedLiveActive = true
    }
    this.#scheduleRefresh(streamEvent.boundary)
    if (!this.#followPaused && this.#liveWindow && extendsOutputPreview) void this.#hooks.focusLatest()
    this.#publish()
  }

  #scheduleRefresh(immediate: boolean): void {
    if (this.#disposed || this.#resetRecoveryPending) return
    if (immediate) {
      if (this.#refreshTimer) clearTimeout(this.#refreshTimer)
      this.#refreshTimer = undefined
      this.#refreshDeadline = undefined
      this.#flushRefresh()
    } else if (!this.#refreshTimer) {
      this.#refreshDeadline = this.#now() + OBSERVATION_REFRESH_WINDOW_MS
      this.#refreshTimer = setTimeout(() => {
        this.#refreshTimer = undefined
        this.#refreshDeadline = undefined
        this.#flushRefresh()
      }, OBSERVATION_REFRESH_WINDOW_MS)
    }
  }

  #flushRefresh(): void {
    if (this.#disposed || this.#resetRecoveryPending) return
    const ids = [...this.#pendingRoots.keys()].filter((id) => this.#inFlightRoots.get(id) !== this.#rangeVersion)
    if (ids.length && this.#activeTab === 'interactions') void this.#refreshRoots(ids)
    const selected = this.#selectedInteraction?.id
    if (selected && this.#eventsPending > (this.#interactionDetail?.snapshot_sequence ?? 0))
      void this.#refreshSelectedEvents(selected, this.#selectionVersion)
    if (this.#failurePending && !this.#failureFlight && this.#activeTab === 'failures') {
      const version = this.#rangeVersion
      const request = this.loadFailures().finally(() => {
        if (this.#failureFlight !== request) return
        this.#failureFlight = undefined
        if (version !== this.#rangeVersion) return
        if (!this.#failureError) {
          if (this.#failurePending <= this.#failureSequence) this.#failurePending = 0
          else this.#scheduleRefresh(false)
        }
      })
      this.#failureFlight = request
    }
  }

  async #refreshRoots(ids: string[]): Promise<void> {
    const version = this.#rangeVersion
    const selection = this.#selectionVersion
    ids.forEach((id) => this.#inFlightRoots.set(id, version))
    try {
      const page = await this.#api.changes({
        filters: this.#query(),
        roots: ids.map((root_id) => ({
          root_id,
          after_sequence: this.#rootSequences.get(root_id) ?? this.#snapshotSequence,
          known_interactions: (this.#roots.find((root) => root.id === root_id)?.interactions ?? []).map(
            ({ id, last_event_sequence, matched, debug_status }) => ({
              id,
              last_event_sequence,
              matched,
              debug_status,
            }),
          ),
        })),
      })
      if (version !== this.#rangeVersion) return
      if (page.reset_required) {
        await this.#onStreamUpdate({ type: 'reset_required', snapshot_sequence: page.snapshot_sequence })
        return
      }
      for (const change of page.changes) {
        if (page.snapshot_sequence < (this.#rootSequences.get(change.root_id) ?? 0)) continue
        const previous = this.#roots.find((root) => root.id === change.root_id)
        const keepWindowRoot =
          change.removal_reason === 'window' &&
          previous &&
          (!this.#liveWindow || change.last_active_at >= this.#windowStart)
        if (change.removal_reason && !keepWindowRoot) {
          this.#roots = this.#roots.filter((root) => root.id !== change.root_id)
          if (
            change.removal_reason === 'deleted' &&
            selection === this.#selectionVersion &&
            this.#selectedInteraction?.root_id === change.root_id
          )
            this.closeInspector()
          continue
        }
        if (change.removal_reason === 'window' && previous && !this.#liveWindow) {
          this.#migratedRoots = new Set([...this.#migratedRoots, change.root_id])
        }
        const changesById = new Map(change.interactions.map((item) => [item.id, item]))
        const knownIds = new Set(previous?.interactions.map((item) => item.id))
        const removed = new Set(change.removed_interaction_ids)
        if (
          (!change.removal_reason || change.removal_reason === 'window') &&
          selection === this.#selectionVersion &&
          this.#selectedInteraction &&
          removed.has(this.#selectedInteraction.id)
        )
          this.closeInspector()
        const root: ForestRoot = {
          id: change.root_id,
          last_active_at: change.last_active_at,
          interactions: [
            ...(previous?.interactions ?? [])
              .filter((item) => !removed.has(item.id))
              .map((item) => changesById.get(item.id) ?? item),
            ...change.interactions.filter((item) => !knownIds.has(item.id) && !removed.has(item.id)),
          ],
        }
        this.#roots = previous ? this.#roots.map((item) => (item.id === root.id ? root : item)) : [...this.#roots, root]
        if (selection === this.#selectionVersion) {
          const selected = root.interactions.find((item) => item.id === this.#selectedInteraction?.id)
          if (selected) {
            this.#selectedInteraction = selected
            if (this.#interactionDetail)
              this.#interactionDetail = { ...this.#interactionDetail, interaction: selected, root }
          }
        }
      }
      if (page.snapshot_sequence >= this.#rootTotalSequence) {
        this.#rootTotal = page.root_total
        this.#rootTotalSequence = page.snapshot_sequence
      }
      for (const id of ids) {
        this.#rootSequences.set(id, Math.max(this.#rootSequences.get(id) ?? 0, page.snapshot_sequence))
        if ((this.#pendingRoots.get(id) ?? 0) <= page.snapshot_sequence) this.#pendingRoots.delete(id)
      }
      this.#appliedSequence = Math.max(this.#appliedSequence, page.snapshot_sequence)
      ids.forEach((id) => this.#rootErrors.delete(id))
      this.#loadError = this.#rootErrors.values().next().value
      this.#publish()
    } catch (error) {
      if (version === this.#rangeVersion) {
        ids.forEach((id) => this.#rootErrors.set(id, error))
        this.#loadError = error
        this.#publish()
      }
    } finally {
      ids.forEach((id) => {
        if (this.#inFlightRoots.get(id) === version) this.#inFlightRoots.delete(id)
      })
      if (
        version === this.#rangeVersion &&
        !ids.some((id) => this.#rootErrors.has(id)) &&
        ids.some((id) => this.#pendingRoots.has(id))
      )
        this.#scheduleRefresh(false)
    }
  }

  #snapshot(): ObservationWorkspaceSnapshot {
    const minTokensFilter = this.#minTokens()
    const canvasRoots = this.#canvasRoots()
    const canvasInteractions = canvasRoots.flatMap((root) => root.interactions)
    return {
      windowStart: this.#windowStart,
      windowEnd: this.#windowEnd,
      durationMs: this.#durationMs,
      customRange: this.#customRange,
      liveWindow: this.#liveWindow,
      activeTab: this.#activeTab,
      providerFilter: this.#providerFilter,
      modelFilter: this.#modelFilter,
      apiKeyFilter: this.#apiKeyFilter,
      statusFilter: this.#statusFilter,
      minTokenStop: this.#minTokenStop,
      minTokensFilter,
      activeFilterCount:
        [
          this.#providerFilter,
          this.#modelFilter,
          this.#apiKeyFilter,
          ...(this.#activeTab === 'interactions' ? [this.#statusFilter] : []),
        ].filter((value) => value !== 'all').length + Number(this.#activeTab === 'interactions' && minTokensFilter > 0),
      canvasRoots,
      latestInteraction: [...canvasInteractions].sort(
        (a, b) =>
          Number(b.status === 'running') - Number(a.status === 'running') || b.last_active_at - a.last_active_at,
      )[0],
      selectedPath: this.#selectedPath(),
      selectedMigrated: this.#selectedInteraction
        ? this.#roots.find((root) => root.interactions.some((item) => item.id === this.#selectedInteraction?.id))?.id
        : undefined,
      migratedRoots: this.#migratedRoots,
      selectedInteraction: this.#selectedInteraction,
      selectedFailure: this.#selectedFailure,
      interactionDetail: this.#interactionDetail,
      failureDetail: this.#failureDetail,
      failureDetailError: this.#failureDetailError,
      detailLoading: this.#detailLoading,
      olderLoading: this.#olderLoading,
      selectedLiveBlocks: this.#liveBlocks.filter((block) => block.interaction_id === this.#selectedInteraction?.id),
      liveContentEpoch: this.#liveContentEpoch,
      selectedLiveActive: this.#selectedLiveActive,
      liveTerminalEpoch: this.#liveTerminalEpoch,
      detailError: this.#detailError,
      liveGaps: this.#liveGaps,
      liveCapacityGaps: this.#liveCapacityGaps,
      streamConnected: this.#streamConnected,
      followPaused: this.#followPaused,
      hasNewActivity: this.#hasNewActivity,
      fitProgress: this.#fitProgress,
      loading: this.#loading,
      loadingMore: this.#loadingMore,
      loadError: this.#loadError,
      nextCursor: this.#nextCursor,
      rootTotal: this.#rootTotal,
      failures: this.#failures,
      failureTotal: this.#failureTotal,
      failureCursor: this.#failureCursor,
      failureLoading: this.#failureLoading,
      failureError: this.#failureError,
    }
  }

  #publish(): void {
    this.#listener(this.#snapshot())
  }
}

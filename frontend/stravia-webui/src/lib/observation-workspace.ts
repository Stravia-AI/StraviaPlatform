import { CanvasLinkIndex } from '#lib/interaction-canvas-links.js'
import { hiddenFailureNode } from '#lib/observation-chain-visibility.js'
import { mergeObservationRuns, retainLiveBlocks, withoutCommittedBlocks } from '#lib/observation-state.js'
import type { ObservationSubscription } from '#lib/observation-stream.js'
import type {
  BundleResourceKind,
  ClientErrorDetail,
  ClientErrorPage,
  ClientErrorQuery,
  ClientErrorSummary,
  ClientErrorType,
  UpstreamErrorDetail,
  UpstreamErrorOutcome,
  UpstreamErrorPage,
  UpstreamErrorQuery,
  UpstreamErrorSummary,
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
} from '#lib/types/observation.js'

export const OBSERVATION_BATCH_SIZE = 12
export const OBSERVATION_MAX_WINDOW_MS = 86_400_000
export const OBSERVATION_DEFAULT_WINDOW_MS = 10 * 60_000
export const OBSERVATION_PRESET_MINUTES = [5, 10, 30, 60, 240, 720, 1440]
export const OBSERVATION_MIN_TOKEN_STOPS = [
  0, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000, 1_000_000,
]
export const OBSERVATION_DEFAULT_MIN_TOKEN_STOP = 4
const ERROR_BATCH_SIZE = 30
export const OBSERVATION_REFRESH_WINDOW_MS = 100

export function isValidObservationRange(startMs: number, endMs: number): boolean {
  return (
    Number.isFinite(startMs) &&
    Number.isFinite(endMs) &&
    endMs > startMs &&
    endMs - startMs <= OBSERVATION_MAX_WINDOW_MS
  )
}

// 卡片上的用户输入预览或模型输出预览是否有更新；新出现的卡片只在已带预览文本时算更新，
// 避免刚创建、尚无内容的卡片抢走视口。
function previewUpdated(before: InteractionSummary | undefined, after: InteractionSummary): boolean {
  if (!before) return Boolean(after.input_preview) || after.visible_tail !== ''
  return before.input_preview !== after.input_preview || before.visible_tail !== after.visible_tail
}

/** logs 页需要的 admin.observations 窄面；admin.observations 结构满足此接口。 */
export interface ObservationWorkspaceApi {
  forest(query: ForestQuery): Promise<ForestPage>
  changes(query: RootChangesQuery): Promise<RootChangesPage>
  clientErrors(query: ClientErrorQuery): Promise<ClientErrorPage>
  upstreamErrors(query: UpstreamErrorQuery): Promise<UpstreamErrorPage>
  interaction(id: string, query?: ForestQuery): Promise<InteractionDetail>
  interactionEvents(id: string, query: InteractionEventsQuery): Promise<InteractionEventsPage>
  clientError(kind: ClientErrorSummary['kind'], id: string): Promise<ClientErrorDetail>
  upstreamError(id: string): Promise<UpstreamErrorDetail>
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
  interactionDetail?: InteractionDetail
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
  clientErrorTypeFilter: ClientErrorType | 'all'
  selectedClientError?: ClientErrorSummary
  clientErrorDetail?: ClientErrorDetail
  clientErrorDetailError: unknown
  clientErrorDetailLoading: boolean
  clientErrors: ClientErrorSummary[]
  clientErrorTotal: number
  clientErrorCursor: string | null | undefined
  clientErrorLoading: boolean
  clientErrorError: unknown
  upstreamErrorOutcomeFilter: UpstreamErrorOutcome | 'all'
  selectedUpstreamError?: UpstreamErrorSummary
  upstreamErrorDetail?: UpstreamErrorDetail
  upstreamErrorDetailError: unknown
  upstreamErrorDetailLoading: boolean
  upstreamErrors: UpstreamErrorSummary[]
  upstreamErrorTotal: number
  upstreamErrorCursor: string | null | undefined
  upstreamErrorLoading: boolean
  upstreamErrorError: unknown
}

interface ObservationErrorPage<T> {
  items: T[]
  total: number
  next_cursor: string | null
  snapshot_sequence: number
}

/**
 * 一个观测错误列表的完整状态机：列表分页、待刷新水位与选中详情。
 * 客户端错误与上游错误各持一个实例；范围版本、请求版本与选择版本互不共享，
 * 任一侧的重查或重置不会覆盖另一侧。
 */
class ObservationErrorList<T, D> {
  readonly identity: (item: T) => string
  /** 查询范围（窗口与筛选）版本；作废该列表上所有在途请求。 */
  scope = 0
  /** 该列表单次请求的版本；同一范围内后发请求覆盖先回请求。 */
  requestVersion = 0
  items: T[] = []
  total = 0
  cursor: string | null | undefined
  loading = false
  error: unknown
  /** 最近写入列表的快照序列，用来判断待刷新水位是否已被覆盖。 */
  sequence = 0
  pending = 0
  query: Promise<void> | undefined
  queryAppend = false
  queryScope = -1
  flight: Promise<void> | undefined
  selected: T | undefined
  detail: D | undefined
  detailError: unknown
  detailLoading = false
  #detailVersion = 0

  constructor(identity: (item: T) => string) {
    this.identity = identity
  }

  get detailVersion(): number {
    return this.#detailVersion
  }

  /** 作废在途请求，保留已显示的行；替换查询会在结果到达后覆盖它们。 */
  invalidate(): void {
    this.scope += 1
    this.requestVersion += 1
    this.pending = 0
    this.flight = undefined
    this.query = undefined
    this.queryScope = -1
  }

  /** 范围重置：行、游标、错误与选择一并清空。 */
  reset(): void {
    this.invalidate()
    this.items = []
    this.total = 0
    this.cursor = undefined
    this.loading = false
    this.error = undefined
    this.sequence = 0
    this.clearSelection()
  }

  clearSelection(): void {
    this.#detailVersion += 1
    this.selected = undefined
    this.detail = undefined
    this.detailError = undefined
    this.detailLoading = false
  }

  beginDetail(): number {
    return ++this.#detailVersion
  }
}

/**
 * logs 页观测工作区的编排状态机：roots/cursor 和解、live 窗口、selection 竞态守卫、
 * live-block 保留、reset 恢复、clientErrors 分页与深链 reveal 都收敛在这里；
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
  readonly #clientErrorList = new ObservationErrorList<ClientErrorSummary, ClientErrorDetail>(
    (item) => `${item.kind}:${item.id}`,
  )
  readonly #upstreamErrorList = new ObservationErrorList<UpstreamErrorSummary, UpstreamErrorDetail>((item) => item.id)

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
  #clientErrorTypeFilter: ClientErrorType | 'all' = 'all'
  #upstreamErrorOutcomeFilter: UpstreamErrorOutcome | 'all' = 'all'
  #minTokenStop = OBSERVATION_DEFAULT_MIN_TOKEN_STOP

  #roots: ForestRoot[] = []
  // 画布派生只在 roots / reveal 集合换引用时重建；时钟、连接状态与选中正文不触发全图重算。
  #canvasViewCache:
    | {
        roots: ForestRoot[]
        revealed: ReadonlySet<string>
        canvasRoots: ForestRoot[]
        interactions: InteractionSummary[]
        linkIndex?: CanvasLinkIndex
      }
    | undefined
  #rootInteractionsCache:
    { roots: ForestRoot[]; byId: Map<string, InteractionSummary>; rootIds: Map<string, string> } | undefined
  #latestCache: { canvasRoots: ForestRoot[]; value: InteractionSummary | undefined } | undefined
  #selectedPathCache:
    { canvasRoots: ForestRoot[]; selection: InteractionSummary | undefined; value: Set<string> } | undefined
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
  #requestedInteractionId: string | undefined
  #interactionDetail: InteractionDetail | undefined
  #interactionDetailEpoch = -1
  #resetRecoveryPending = false
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
      this.#activeTab === 'client-errors' &&
      !this.#clientErrorList.loading &&
      this.#clientErrorList.items.some((item) => item.started_at < this.#windowStart)
    ) {
      void this.#requestErrors(this.#clientErrorList, true, true, (query) => this.#clientErrorPage(query))
    }
    if (
      this.#activeTab === 'upstream-errors' &&
      !this.#upstreamErrorList.loading &&
      this.#upstreamErrorList.items.some((item) => item.started_at < this.#windowStart)
    ) {
      void this.#requestErrors(this.#upstreamErrorList, true, true, (query) => this.#upstreamErrorPage(query))
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
      const interaction = this.#canvasView().interactions.find((item) => item.id === focusId)
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
    // 先推进实时窗口再查询，两次推进之间完成的错误不会被旧 end_at 排除。
    if (this.#activeTab === 'client-errors') {
      this.#updateLiveBounds()
      await this.loadClientErrors()
    } else if (this.#activeTab === 'upstream-errors') {
      this.#updateLiveBounds()
      await this.loadUpstreamErrors()
    } else await this.#loadForest(true)
  }

  clearFilters(): void {
    this.#providerFilter = 'all'
    this.#modelFilter = 'all'
    this.#apiKeyFilter = 'all'
    this.#statusFilter = 'all'
    this.#clientErrorTypeFilter = 'all'
    this.#upstreamErrorOutcomeFilter = 'all'
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

  setClientErrorTypeFilter(value: ClientErrorType | 'all'): void {
    this.#clientErrorTypeFilter = value
    this.#publish()
  }

  setUpstreamErrorOutcomeFilter(value: UpstreamErrorOutcome | 'all'): void {
    this.#upstreamErrorOutcomeFilter = value
    this.#publish()
  }

  async tabChanged(value: string): Promise<void> {
    this.#rangeVersion += 1
    this.#resetRefreshState()
    this.#activeTab = value
    this.closeInspector()
    this.#publish()
    // 先推进实时窗口再查询，两次推进之间完成的错误不会被旧 end_at 排除。
    if (value === 'client-errors') {
      this.#updateLiveBounds()
      await this.loadClientErrors()
    } else if (value === 'upstream-errors') {
      this.#updateLiveBounds()
      await this.loadUpstreamErrors()
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
    this.#requestedInteractionId = interaction.id
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

  async selectClientError(clientError: ClientErrorSummary): Promise<void> {
    this.closeInspector()
    await this.#loadErrorDetail(this.#clientErrorList, clientError, (item) => this.#api.clientError(item.kind, item.id))
  }

  async selectUpstreamError(upstreamError: UpstreamErrorSummary): Promise<void> {
    this.closeInspector()
    await this.#loadErrorDetail(this.#upstreamErrorList, upstreamError, (item) => this.#api.upstreamError(item.id))
  }

  /** 两个错误列表各自持有详情版本；后一次选择或关闭会作废先前在途的详情请求。 */
  async #loadErrorDetail<T, D>(
    state: ObservationErrorList<T, D>,
    item: T,
    load: (item: T) => Promise<D>,
    keepDetail = false,
    propagate = false,
  ): Promise<void> {
    const detailVersion = state.beginDetail()
    state.selected = item
    if (!keepDetail) state.detail = undefined
    state.detailError = undefined
    state.detailLoading = true
    this.#publish()
    try {
      const detail = await load(item)
      if (detailVersion === state.detailVersion) state.detail = detail
    } catch (error) {
      if (detailVersion === state.detailVersion) state.detailError = error
      if (propagate) throw error
    } finally {
      if (detailVersion === state.detailVersion) state.detailLoading = false
      this.#publish()
    }
  }

  /** 深链（page.url ?interaction=）：reveal + 选中 + 补齐事件。 */
  async deepLinkInteraction(id: string): Promise<void> {
    this.closeInspector()
    const selection = this.#selectionVersion
    // 深链尚无 summary；先保留目标，才能记录快照查询在途时收到的通知。
    this.#requestedInteractionId = id
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
    this.#requestedInteractionId = undefined
    this.#interactionDetail = undefined
    this.#clientErrorList.clearSelection()
    this.#upstreamErrorList.clearSelection()
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

  async openClientErrorInteraction(): Promise<void> {
    const id = this.#clientErrorList.detail?.request.interaction_id
    if (!id) return
    await this.#openInteractionById(id)
  }

  async openUpstreamErrorInteraction(): Promise<void> {
    const id = this.#upstreamErrorList.detail?.error.interaction_id
    if (!id) return
    await this.#openInteractionById(id)
  }

  /** 零输出终态失败不在交互链路中；从错误详情打开时显式 reveal，不改链路成员规则。 */
  async #openInteractionById(id: string): Promise<void> {
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
    try {
      if (interactionId && selection === this.#selectionVersion) {
        this.#applySelectedDetail(await this.#api.interaction(interactionId, this.#query()), selection)
        await this.#refreshSelectedEvents(interactionId, selection)
      }
    } catch (error) {
      if (selection === this.#selectionVersion && interactionId) {
        if (!this.#removeMissingSelection(error, selection)) this.#detailError = error
      }
    } finally {
      this.#publish()
    }
    const clientError = this.#clientErrorList.selected
    if (clientError)
      await this.#loadErrorDetail(
        this.#clientErrorList,
        clientError,
        (item) => this.#api.clientError(item.kind, item.id),
        true,
      )
    const upstreamError = this.#upstreamErrorList.selected
    if (upstreamError)
      await this.#loadErrorDetail(
        this.#upstreamErrorList,
        upstreamError,
        (item) => this.#api.upstreamError(item.id),
        true,
      )
  }

  loadClientErrors(replace = true): Promise<void> {
    return this.#requestErrors(this.#clientErrorList, replace, false, (query) => this.#clientErrorPage(query))
  }

  loadUpstreamErrors(replace = true): Promise<void> {
    return this.#requestErrors(this.#upstreamErrorList, replace, false, (query) => this.#upstreamErrorPage(query))
  }

  #clientErrorPage(query: { cursor?: string; limit: number }): Promise<ClientErrorPage> {
    return this.#api.clientErrors({
      ...query,
      start_at: this.#windowStart,
      end_at: this.#windowEnd,
      provider: this.#providerFilter === 'all' ? undefined : this.#providerFilter,
      model: this.#modelFilter === 'all' ? undefined : this.#modelFilter,
      api_key: this.#apiKeyFilter === 'all' ? undefined : this.#apiKeyFilter,
      type: this.#clientErrorTypeFilter === 'all' ? undefined : this.#clientErrorTypeFilter,
    })
  }

  #upstreamErrorPage(query: { cursor?: string; limit: number }): Promise<UpstreamErrorPage> {
    return this.#api.upstreamErrors({
      ...query,
      start_at: this.#windowStart,
      end_at: this.#windowEnd,
      provider: this.#providerFilter === 'all' ? undefined : this.#providerFilter,
      model: this.#modelFilter === 'all' ? undefined : this.#modelFilter,
      api_key: this.#apiKeyFilter === 'all' ? undefined : this.#apiKeyFilter,
      outcome: this.#upstreamErrorOutcomeFilter === 'all' ? undefined : this.#upstreamErrorOutcomeFilter,
    })
  }

  /**
   * 单个错误列表的分页请求。范围版本与请求版本都取自该列表自身，
   * 因此另一个 tab 的重置或重查不会作废这里的在途结果。
   * `background` 为实时事件或窗口滑动触发的静默重查：不置 loading，列表与错误提示保持到新结果到达。
   */
  #requestErrors<T>(
    state: ObservationErrorList<T, unknown>,
    replace: boolean,
    background: boolean,
    fetchPage: (query: { cursor?: string; limit: number }) => Promise<ObservationErrorPage<T>>,
  ): Promise<void> {
    const inflight = state.query
    if (inflight && state.queryScope === state.scope) {
      if (state.queryAppend === !replace) return inflight
      // 追加与重查依赖对方更新后的 cursor 和列表：并入进行中的请求会让后到的操作静默丢失，故排在其后。
      const scope = state.scope
      return inflight.then((): Promise<void> =>
        scope !== state.scope || (!replace && !state.cursor)
          ? Promise.resolve()
          : this.#requestErrors(state, replace, background, fetchPage),
      )
    }
    state.queryScope = state.scope
    state.queryAppend = !replace
    const request = this.#pullErrors(state, replace, background, fetchPage).finally(() => {
      if (state.query === request) state.query = undefined
    })
    state.query = request
    return request
  }

  async #pullErrors<T>(
    state: ObservationErrorList<T, unknown>,
    replace: boolean,
    background: boolean,
    fetchPage: (query: { cursor?: string; limit: number }) => Promise<ObservationErrorPage<T>>,
  ): Promise<void> {
    if (!replace && state.loading) return
    const scope = state.scope
    const requestVersion = ++state.requestVersion
    // 后台重查覆盖已加载的全部行，免得实时事件把「加载更多」翻出的页折回第一页；超出服务端上限时由其截断。
    const limit = background ? Math.max(ERROR_BATCH_SIZE, state.items.length) : ERROR_BATCH_SIZE
    if (!background) {
      state.loading = true
      state.error = undefined
      this.#publish()
    }
    try {
      const page = await fetchPage({ cursor: replace ? undefined : (state.cursor ?? undefined), limit })
      if (scope !== state.scope || requestVersion !== state.requestVersion) return
      state.items = replace
        ? page.items
        : [
            ...state.items,
            ...page.items.filter(
              (item) => !state.items.some((known) => state.identity(known) === state.identity(item)),
            ),
          ]
      state.sequence = page.snapshot_sequence
      state.total = page.total
      state.cursor = page.next_cursor
      state.error = undefined
    } catch (error) {
      if (scope === state.scope && requestVersion === state.requestVersion) state.error = error
    } finally {
      if (scope === state.scope && requestVersion === state.requestVersion) state.loading = false
      this.#publish()
    }
  }

  /** 实时事件只在对应 tab 且处于实时窗口时标记待刷新；范围本身不移动。 */
  #flushErrorList<T, D>(
    state: ObservationErrorList<T, D>,
    fetchPage: (query: { cursor?: string; limit: number }) => Promise<ObservationErrorPage<T>>,
  ): void {
    if (!state.pending || state.flight) return
    const scope = state.scope
    const flight = this.#requestErrors(state, true, true, fetchPage).finally(() => {
      if (state.flight !== flight) return
      state.flight = undefined
      if (scope !== state.scope) return
      if (!state.error) {
        if (state.pending <= state.sequence) state.pending = 0
        else this.#scheduleRefresh(false)
      }
    })
    state.flight = flight
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
    const clientError = this.#clientErrorList.detail?.request
    const interactionId =
      this.#interactionDetail?.interaction.id ??
      clientError?.interaction_id ??
      this.#upstreamErrorList.detail?.error.interaction_id
    if (interactionId) return { kind: 'interaction', id: interactionId }
    return clientError ? { kind: 'rejected_request', id: clientError.id } : null
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

  #rootIndex(): { byId: Map<string, InteractionSummary>; rootIds: Map<string, string> } {
    const cached = this.#rootInteractionsCache
    if (cached && cached.roots === this.#roots) return cached
    const byId = new Map<string, InteractionSummary>()
    const rootIds = new Map<string, string>()
    for (const root of this.#roots) {
      for (const interaction of root.interactions) {
        byId.set(interaction.id, interaction)
        rootIds.set(interaction.id, root.id)
      }
    }
    const index = { roots: this.#roots, byId, rootIds }
    this.#rootInteractionsCache = index
    return index
  }

  #canvasView(): { canvasRoots: ForestRoot[]; interactions: InteractionSummary[]; linkIndex?: CanvasLinkIndex } {
    const cached = this.#canvasViewCache
    if (cached && cached.roots === this.#roots && cached.revealed === this.#revealedFailures) return cached
    const canvasRoots = this.#roots
      .map((root) => ({
        ...root,
        interactions: root.interactions.filter(
          (item) => this.#revealedFailures.has(item.id) || !hiddenFailureNode(item),
        ),
      }))
      .filter((root) => root.interactions.length > 0)
    const next = {
      roots: this.#roots,
      revealed: this.#revealedFailures,
      canvasRoots,
      interactions: canvasRoots.flatMap((root) => root.interactions),
    }
    this.#canvasViewCache = next
    return next
  }

  /** 单遍取最新：运行中优先，其次 last_active_at；同值时保留先出现的成员（与原稳定排序一致）。 */
  #latestInteraction(): InteractionSummary | undefined {
    const view = this.#canvasView()
    const cached = this.#latestCache
    if (cached && cached.canvasRoots === view.canvasRoots) return cached.value
    let latest: InteractionSummary | undefined
    for (const interaction of view.interactions) {
      if (!latest) {
        latest = interaction
        continue
      }
      const runningDelta = Number(interaction.status === 'running') - Number(latest.status === 'running')
      if (runningDelta > 0 || (runningDelta === 0 && interaction.last_active_at > latest.last_active_at))
        latest = interaction
    }
    this.#latestCache = { canvasRoots: view.canvasRoots, value: latest }
    return latest
  }

  #selectedPath(): Set<string> {
    const view = this.#canvasView()
    const selection = this.#selectedInteraction
    const cached = this.#selectedPathCache
    if (cached && cached.canvasRoots === view.canvasRoots && cached.selection === selection) return cached.value
    const path = new Set<string>()
    let current = selection
    if (current) {
      // 一次构建视觉父边索引，整条选中路径共用快照内的祖先查询。
      const index = (view.linkIndex ??= new CanvasLinkIndex(view.interactions))
      while (current && !path.has(current.id)) {
        path.add(current.id)
        const parentId: string | undefined = index.visualParent(current)?.id
        current = parentId ? index.byId.get(parentId) : undefined
      }
    }
    this.#selectedPathCache = { canvasRoots: view.canvasRoots, selection, value: path }
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
    this.#clientErrorList.reset()
    this.#upstreamErrorList.reset()
    await Promise.all([
      this.#loadForest(true),
      this.#activeTab === 'client-errors'
        ? this.loadClientErrors()
        : this.#activeTab === 'upstream-errors'
          ? this.loadUpstreamErrors()
          : Promise.resolve(),
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
    if (selection !== this.#selectionVersion || this.#interactionDetail?.interaction.id !== id) return Promise.resolve()
    if (this.#eventsRequest) return this.#eventsRequest
    // 详情已带快照内的首批事件与 runs；只有待处理通知水位超过它时才串行补齐。
    if (this.#eventsPending <= (this.#interactionDetail?.snapshot_sequence ?? 0)) return Promise.resolve()
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
    // 两个错误列表各自作废在途请求；已显示的行保留到替换结果到达。
    this.#clientErrorList.invalidate()
    this.#upstreamErrorList.invalidate()
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
      const interactionId = this.#selectedInteraction?.id ?? this.#requestedInteractionId
      const clientError = this.#clientErrorList.selected
      const upstreamError = this.#upstreamErrorList.selected
      this.#detailLoading = Boolean(interactionId)
      this.#clientErrorList.detailError = undefined
      this.#upstreamErrorList.detailError = undefined
      this.#publish()
      try {
        await Promise.all([
          this.#loadForest(true, false),
          this.#activeTab === 'client-errors'
            ? this.loadClientErrors()
            : this.#activeTab === 'upstream-errors'
              ? this.loadUpstreamErrors()
              : Promise.resolve(),
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
            : Promise.resolve(),
          clientError
            ? this.#loadErrorDetail(
                this.#clientErrorList,
                clientError,
                (item) => this.#api.clientError(item.kind, item.id),
                true,
                true,
              )
            : Promise.resolve(),
          upstreamError
            ? this.#loadErrorDetail(
                this.#upstreamErrorList,
                upstreamError,
                (item) => this.#api.upstreamError(item.id),
                true,
                true,
              )
            : Promise.resolve(),
        ])
        if (version !== this.#rangeVersion) return
        const activeError =
          this.#activeTab === 'client-errors'
            ? this.#clientErrorList.error
            : this.#activeTab === 'upstream-errors'
              ? this.#upstreamErrorList.error
              : undefined
        if (activeError)
          throw activeError instanceof Error
            ? activeError
            : new Error(typeof activeError === 'string' ? activeError : 'observation error list reload failed')
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
    if (this.#liveWindow) {
      if (this.#activeTab === 'client-errors' && ['request_rejected', 'run_finished'].includes(streamEvent.kind)) {
        this.#clientErrorList.pending = Math.max(this.#clientErrorList.pending, streamEvent.sequence)
      }
      if (
        this.#activeTab === 'upstream-errors' &&
        streamEvent.kind === 'target_attempt_finished' &&
        streamEvent.error_source === 'upstream'
      ) {
        this.#upstreamErrorList.pending = Math.max(this.#upstreamErrorList.pending, streamEvent.sequence)
      }
    }
    if (!this.#liveWindow && streamEvent.interaction_id && streamEvent.occurred_at >= this.#windowEnd) {
      const root = this.#roots.find((item) =>
        item.interactions.some((interaction) => interaction.id === streamEvent.interaction_id),
      )
      if (root) this.#migratedRoots = new Set([...this.#migratedRoots, root.id])
    }
    this.#publish()
    const interactionId = streamEvent.interaction_id
    const known = interactionId ? this.#rootIndex().byId.get(interactionId) : undefined
    const rootId = streamEvent.root_id ?? known?.root_id
    if (rootId && this.#activeTab === 'interactions' && (!known || known.last_event_sequence < streamEvent.sequence)) {
      this.#pendingRoots.set(rootId, Math.max(this.#pendingRoots.get(rootId) ?? 0, streamEvent.sequence))
    }
    if (interactionId && interactionId === this.#requestedInteractionId) {
      this.#eventsPending = Math.max(this.#eventsPending, streamEvent.sequence)
      if (streamEvent.boundary && streamEvent.kind === 'run_finished') {
        if (streamEvent.run_id) this.#finishLiveRun(streamEvent.run_id)
      }
      if (streamEvent.kind === 'run_started') this.#selectedLiveActive = true
    }
    this.#scheduleRefresh(streamEvent.boundary)
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
    if (this.#activeTab === 'client-errors') {
      this.#flushErrorList(this.#clientErrorList, (query) => this.#clientErrorPage(query))
    } else if (this.#activeTab === 'upstream-errors') {
      this.#flushErrorList(this.#upstreamErrorList, (query) => this.#upstreamErrorPage(query))
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
      let updatedCard: InteractionSummary | undefined
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
        const knownById = new Map(previous?.interactions.map((item) => [item.id, item]))
        const removed = new Set(change.removed_interaction_ids)
        for (const item of change.interactions) {
          if (removed.has(item.id) || !previewUpdated(knownById.get(item.id), item)) continue
          if (!updatedCard || item.last_active_at >= updatedCard.last_active_at) updatedCard = item
        }
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
            ...change.interactions.filter((item) => !knownById.has(item.id) && !removed.has(item.id)),
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
      // 自动跟随只响应卡片上用户输入预览或模型输出预览的变化；状态、用量、思考与工具调用
      // 照常刷新卡片，但不点亮跟随入口，也不移动视口。
      if (updatedCard && this.#followPaused) this.#hasNewActivity = true
      this.#publish()
      if (updatedCard && !this.#followPaused && this.#liveWindow) void this.#hooks.focusNode(updatedCard.id)
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
          ...(this.#activeTab === 'client-errors' ? [this.#clientErrorTypeFilter] : []),
          ...(this.#activeTab === 'upstream-errors' ? [this.#upstreamErrorOutcomeFilter] : []),
        ].filter((value) => value !== 'all').length + Number(this.#activeTab === 'interactions' && minTokensFilter > 0),
      canvasRoots: this.#canvasView().canvasRoots,
      latestInteraction: this.#latestInteraction(),
      selectedPath: this.#selectedPath(),
      selectedMigrated: this.#selectedInteraction
        ? this.#rootIndex().rootIds.get(this.#selectedInteraction.id)
        : undefined,
      migratedRoots: this.#migratedRoots,
      selectedInteraction: this.#selectedInteraction,
      interactionDetail: this.#interactionDetail,
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
      clientErrorTypeFilter: this.#clientErrorTypeFilter,
      selectedClientError: this.#clientErrorList.selected,
      clientErrorDetail: this.#clientErrorList.detail,
      clientErrorDetailError: this.#clientErrorList.detailError,
      clientErrorDetailLoading: this.#clientErrorList.detailLoading,
      clientErrors: this.#clientErrorList.items,
      clientErrorTotal: this.#clientErrorList.total,
      clientErrorCursor: this.#clientErrorList.cursor,
      clientErrorLoading: this.#clientErrorList.loading,
      clientErrorError: this.#clientErrorList.error,
      upstreamErrorOutcomeFilter: this.#upstreamErrorOutcomeFilter,
      selectedUpstreamError: this.#upstreamErrorList.selected,
      upstreamErrorDetail: this.#upstreamErrorList.detail,
      upstreamErrorDetailError: this.#upstreamErrorList.detailError,
      upstreamErrorDetailLoading: this.#upstreamErrorList.detailLoading,
      upstreamErrors: this.#upstreamErrorList.items,
      upstreamErrorTotal: this.#upstreamErrorList.total,
      upstreamErrorCursor: this.#upstreamErrorList.cursor,
      upstreamErrorLoading: this.#upstreamErrorList.loading,
      upstreamErrorError: this.#upstreamErrorList.error,
    }
  }

  #publish(): void {
    this.#listener(this.#snapshot())
  }
}

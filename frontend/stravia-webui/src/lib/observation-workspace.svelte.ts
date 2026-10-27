import { SvelteSet } from 'svelte/reactivity'
import {
  ObservationWorkspaceController,
  type ObservationWorkspaceApi,
  type ObservationWorkspaceHooks,
  type ObservationWorkspaceSnapshot,
  type ObservationWorkspaceSubscribe,
  type ObservationWorkspaceSubscribeLive,
} from '#lib/observation-workspace.js'
import type {
  BundleResourceKind,
  ClientErrorDetail,
  ClientErrorSummary,
  ClientErrorType,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  LiveContentBlock,
  UpstreamErrorDetail,
  UpstreamErrorOutcome,
  UpstreamErrorSummary,
} from '#lib/types/observation.js'

export {
  isValidObservationRange,
  OBSERVATION_DEFAULT_MIN_TOKEN_STOP,
  OBSERVATION_MAX_WINDOW_MS,
  OBSERVATION_MIN_TOKEN_STOPS,
  OBSERVATION_PRESET_MINUTES,
} from '#lib/observation-workspace.js'
export type {
  ObservationWorkspaceApi,
  ObservationWorkspaceHooks,
  ObservationWorkspaceSnapshot,
} from '#lib/observation-workspace.js'

/**
 * ObservationWorkspaceController 的 Svelte 壳：把推送快照镜像为 $state 字段，
 * 方法逐字透传。编排语义全部在控制器里；这里不新增逻辑。
 */
export class ObservationWorkspace {
  windowStart = $state(0)
  windowEnd = $state(0)
  durationMs = $state(0)
  customRange = $state(false)
  liveWindow = $state(true)
  activeTab = $state('interactions')
  providerFilter = $state('all')
  modelFilter = $state('all')
  apiKeyFilter = $state('all')
  statusFilter = $state('all')
  minTokenStop = $state(0)
  minTokensFilter = $state(0)
  activeFilterCount = $state(0)
  canvasRoots = $state.raw<ForestRoot[]>([])
  latestInteraction = $state.raw<InteractionSummary>()
  selectedPath = $state.raw<Set<string>>(new SvelteSet())
  selectedMigrated = $state.raw<string>()
  migratedRoots = $state.raw<ReadonlySet<string>>(new SvelteSet())
  selectedInteraction = $state.raw<InteractionSummary>()
  interactionDetail = $state.raw<InteractionDetail>()
  detailLoading = $state(false)
  olderLoading = $state(false)
  selectedLiveBlocks = $state.raw<LiveContentBlock[]>([])
  liveContentEpoch = $state(0)
  selectedLiveActive = $state(false)
  liveTerminalEpoch = $state(0)
  detailError = $state.raw<unknown>()
  liveGaps = $state.raw<string[]>([])
  liveCapacityGaps = $state.raw<string[]>([])
  streamConnected = $state(false)
  followPaused = $state(false)
  hasNewActivity = $state(false)
  fitProgress = $state.raw<number>()
  loading = $state(true)
  loadingMore = $state(false)
  loadError = $state.raw<unknown>()
  nextCursor = $state.raw<string | null>()
  rootTotal = $state(0)
  clientErrorTypeFilter = $state<ClientErrorType | 'all'>('all')
  selectedClientError = $state.raw<ClientErrorSummary>()
  clientErrorDetail = $state.raw<ClientErrorDetail>()
  clientErrorDetailError = $state.raw<unknown>()
  clientErrorDetailLoading = $state(false)
  clientErrors = $state.raw<ClientErrorSummary[]>([])
  clientErrorTotal = $state(0)
  clientErrorCursor = $state.raw<string | null>()
  clientErrorLoading = $state(false)
  clientErrorError = $state.raw<unknown>()
  upstreamErrorOutcomeFilter = $state<UpstreamErrorOutcome | 'all'>('all')
  selectedUpstreamError = $state.raw<UpstreamErrorSummary>()
  upstreamErrorDetail = $state.raw<UpstreamErrorDetail>()
  upstreamErrorDetailError = $state.raw<unknown>()
  upstreamErrorDetailLoading = $state(false)
  upstreamErrors = $state.raw<UpstreamErrorSummary[]>([])
  upstreamErrorTotal = $state(0)
  upstreamErrorCursor = $state.raw<string | null>()
  upstreamErrorLoading = $state(false)
  upstreamErrorError = $state.raw<unknown>()

  readonly #controller: ObservationWorkspaceController

  constructor(
    api: ObservationWorkspaceApi,
    subscribe: ObservationWorkspaceSubscribe,
    hooks: ObservationWorkspaceHooks,
    subscribeLive: ObservationWorkspaceSubscribeLive,
  ) {
    this.#controller = new ObservationWorkspaceController({
      api,
      subscribe,
      subscribeLive,
      hooks,
      onSnapshot: (snapshot) => this.#applySnapshot(snapshot),
    })
  }

  start = (): Promise<void> => this.#controller.start()
  dispose = (): void => this.#controller.dispose()
  advanceClock = (): void => this.#controller.advanceClock()
  choosePreset = (minutes: number): Promise<void> => this.#controller.choosePreset(minutes)
  applyRange = (startMs: number, endMs: number): Promise<void> => this.#controller.applyRange(startMs, endMs)
  changeWindow = (delta: number): Promise<void> => this.#controller.changeWindow(delta)
  refreshAnchor = (focusId?: string): Promise<void> => this.#controller.refreshAnchor(focusId)
  reloadForest = (): Promise<void> => this.#controller.reloadForest()
  refreshData = (): Promise<void> => this.#controller.refreshData()
  applyFilters = (): Promise<void> => this.#controller.applyFilters()
  clearFilters = (): void => this.#controller.clearFilters()
  setProviderFilter = (value: string): void => this.#controller.setProviderFilter(value)
  setModelFilter = (value: string): void => this.#controller.setModelFilter(value)
  setApiKeyFilter = (value: string): void => this.#controller.setApiKeyFilter(value)
  setStatusFilter = (value: string): void => this.#controller.setStatusFilter(value)
  setMinTokenStop = (value: number): void => this.#controller.setMinTokenStop(value)
  setClientErrorTypeFilter = (value: ClientErrorType | 'all'): void => this.#controller.setClientErrorTypeFilter(value)
  setUpstreamErrorOutcomeFilter = (value: UpstreamErrorOutcome | 'all'): void =>
    this.#controller.setUpstreamErrorOutcomeFilter(value)
  tabChanged = (value: string): Promise<void> => this.#controller.tabChanged(value)
  loadNextRootBatch = (): Promise<void> => this.#controller.loadNextRootBatch()
  fitAll = (): Promise<void> => this.#controller.fitAll()
  selectInteraction = (interaction: InteractionSummary): Promise<void> =>
    this.#controller.selectInteraction(interaction)
  selectClientError = (clientError: ClientErrorSummary): Promise<void> =>
    this.#controller.selectClientError(clientError)
  selectUpstreamError = (upstreamError: UpstreamErrorSummary): Promise<void> =>
    this.#controller.selectUpstreamError(upstreamError)
  deepLinkInteraction = (id: string): Promise<void> => this.#controller.deepLinkInteraction(id)
  closeInspector = (): void => this.#controller.closeInspector()
  loadOlderEvents = (): Promise<void> => this.#controller.loadOlderEvents()
  openClientErrorInteraction = (): Promise<void> => this.#controller.openClientErrorInteraction()
  openUpstreamErrorInteraction = (): Promise<void> => this.#controller.openUpstreamErrorInteraction()
  refreshSelectedDetail = (): Promise<void> => this.#controller.refreshSelectedDetail()
  loadClientErrors = (replace = true): Promise<void> => this.#controller.loadClientErrors(replace)
  loadUpstreamErrors = (replace = true): Promise<void> => this.#controller.loadUpstreamErrors(replace)
  pauseFollow = (): void => this.#controller.pauseFollow()
  resumeFollow = (): void => this.#controller.resumeFollow()
  bundleTarget = (): { kind: BundleResourceKind; id: string } | null => this.#controller.bundleTarget()

  #applySnapshot(snapshot: ObservationWorkspaceSnapshot): void {
    this.windowStart = snapshot.windowStart
    this.windowEnd = snapshot.windowEnd
    this.durationMs = snapshot.durationMs
    this.customRange = snapshot.customRange
    this.liveWindow = snapshot.liveWindow
    this.activeTab = snapshot.activeTab
    this.providerFilter = snapshot.providerFilter
    this.modelFilter = snapshot.modelFilter
    this.apiKeyFilter = snapshot.apiKeyFilter
    this.statusFilter = snapshot.statusFilter
    this.minTokenStop = snapshot.minTokenStop
    this.minTokensFilter = snapshot.minTokensFilter
    this.activeFilterCount = snapshot.activeFilterCount
    this.canvasRoots = snapshot.canvasRoots
    this.latestInteraction = snapshot.latestInteraction
    this.selectedPath = snapshot.selectedPath
    this.selectedMigrated = snapshot.selectedMigrated
    this.migratedRoots = snapshot.migratedRoots
    this.selectedInteraction = snapshot.selectedInteraction
    this.interactionDetail = snapshot.interactionDetail
    this.detailLoading = snapshot.detailLoading
    this.olderLoading = snapshot.olderLoading
    this.selectedLiveBlocks = snapshot.selectedLiveBlocks
    this.liveContentEpoch = snapshot.liveContentEpoch
    this.selectedLiveActive = snapshot.selectedLiveActive
    this.liveTerminalEpoch = snapshot.liveTerminalEpoch
    this.detailError = snapshot.detailError
    this.liveGaps = snapshot.liveGaps
    this.liveCapacityGaps = snapshot.liveCapacityGaps
    this.streamConnected = snapshot.streamConnected
    this.followPaused = snapshot.followPaused
    this.hasNewActivity = snapshot.hasNewActivity
    this.fitProgress = snapshot.fitProgress
    this.loading = snapshot.loading
    this.loadingMore = snapshot.loadingMore
    this.loadError = snapshot.loadError
    this.nextCursor = snapshot.nextCursor
    this.rootTotal = snapshot.rootTotal
    this.clientErrorTypeFilter = snapshot.clientErrorTypeFilter
    this.selectedClientError = snapshot.selectedClientError
    this.clientErrorDetail = snapshot.clientErrorDetail
    this.clientErrorDetailError = snapshot.clientErrorDetailError
    this.clientErrorDetailLoading = snapshot.clientErrorDetailLoading
    this.clientErrors = snapshot.clientErrors
    this.clientErrorTotal = snapshot.clientErrorTotal
    this.clientErrorCursor = snapshot.clientErrorCursor
    this.clientErrorLoading = snapshot.clientErrorLoading
    this.clientErrorError = snapshot.clientErrorError
    this.upstreamErrorOutcomeFilter = snapshot.upstreamErrorOutcomeFilter
    this.selectedUpstreamError = snapshot.selectedUpstreamError
    this.upstreamErrorDetail = snapshot.upstreamErrorDetail
    this.upstreamErrorDetailError = snapshot.upstreamErrorDetailError
    this.upstreamErrorDetailLoading = snapshot.upstreamErrorDetailLoading
    this.upstreamErrors = snapshot.upstreamErrors
    this.upstreamErrorTotal = snapshot.upstreamErrorTotal
    this.upstreamErrorCursor = snapshot.upstreamErrorCursor
    this.upstreamErrorLoading = snapshot.upstreamErrorLoading
    this.upstreamErrorError = snapshot.upstreamErrorError
  }
}

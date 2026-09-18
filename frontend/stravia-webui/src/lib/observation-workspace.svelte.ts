import { SvelteSet } from 'svelte/reactivity'
import {
  ObservationWorkspaceController,
  type ObservationWorkspaceApi,
  type ObservationWorkspaceHooks,
  type ObservationWorkspaceSnapshot,
  type ObservationWorkspaceSubscribe,
} from '$lib/observation-workspace'
import type {
  BundleResourceKind,
  FailedRequestDetail,
  FailedRequestSummary,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  LiveContentBlock,
} from '$lib/types/observation'

export {
  isValidObservationRange,
  OBSERVATION_DEFAULT_MIN_TOKEN_STOP,
  OBSERVATION_MAX_WINDOW_MS,
  OBSERVATION_MIN_TOKEN_STOPS,
  OBSERVATION_PRESET_MINUTES,
} from '$lib/observation-workspace'
export type {
  ObservationWorkspaceApi,
  ObservationWorkspaceHooks,
  ObservationWorkspaceSnapshot,
} from '$lib/observation-workspace'

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
  selectedFailure = $state.raw<FailedRequestSummary>()
  interactionDetail = $state.raw<InteractionDetail>()
  failureDetail = $state.raw<FailedRequestDetail>()
  failureDetailError = $state.raw<unknown>()
  detailLoading = $state(false)
  olderLoading = $state(false)
  selectedLiveBlocks = $state.raw<LiveContentBlock[]>([])
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
  failures = $state.raw<FailedRequestSummary[]>([])
  failureTotal = $state(0)
  failureCursor = $state.raw<string | null>()
  failureLoading = $state(false)
  failureError = $state.raw<unknown>()

  readonly #controller: ObservationWorkspaceController

  constructor(
    api: ObservationWorkspaceApi,
    subscribe: ObservationWorkspaceSubscribe,
    hooks: ObservationWorkspaceHooks,
  ) {
    this.#controller = new ObservationWorkspaceController({
      api,
      subscribe,
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
  tabChanged = (value: string): Promise<void> => this.#controller.tabChanged(value)
  loadNextRootBatch = (): Promise<void> => this.#controller.loadNextRootBatch()
  fitAll = (): Promise<void> => this.#controller.fitAll()
  selectInteraction = (interaction: InteractionSummary): Promise<void> =>
    this.#controller.selectInteraction(interaction)
  selectFailure = (failure: FailedRequestSummary): Promise<void> => this.#controller.selectFailure(failure)
  deepLinkInteraction = (id: string): Promise<void> => this.#controller.deepLinkInteraction(id)
  closeInspector = (): void => this.#controller.closeInspector()
  loadOlderEvents = (): Promise<void> => this.#controller.loadOlderEvents()
  openFailureInteraction = (): Promise<void> => this.#controller.openFailureInteraction()
  refreshSelectedDetail = (): Promise<void> => this.#controller.refreshSelectedDetail()
  loadFailures = (replace = true): Promise<void> => this.#controller.loadFailures(replace)
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
    this.selectedFailure = snapshot.selectedFailure
    this.interactionDetail = snapshot.interactionDetail
    this.failureDetail = snapshot.failureDetail
    this.failureDetailError = snapshot.failureDetailError
    this.detailLoading = snapshot.detailLoading
    this.olderLoading = snapshot.olderLoading
    this.selectedLiveBlocks = snapshot.selectedLiveBlocks
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
    this.failures = snapshot.failures
    this.failureTotal = snapshot.failureTotal
    this.failureCursor = snapshot.failureCursor
    this.failureLoading = snapshot.failureLoading
    this.failureError = snapshot.failureError
  }
}

<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onMount, tick, untrack } from 'svelte'
import { page } from '$app/state'
import { SvelteSet } from 'svelte/reactivity'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { SvelteFlowProvider } from '@xyflow/svelte'
import BugIcon from '@lucide/svelte/icons/bug'
import CalendarRangeIcon from '@lucide/svelte/icons/calendar-range'
import MaximizeIcon from '@lucide/svelte/icons/maximize'
import MinimizeIcon from '@lucide/svelte/icons/minimize'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatCompactCount, formatLogTime } from '$lib/format'
import { visualParent } from '$lib/interaction-canvas-links'
import { hiddenFailureNode } from '$lib/observation-chain-visibility'
import { eventBlockId, mergeObservationRuns, retainLiveBlocks, withoutCommittedBlocks } from '$lib/observation-state'
import { observationStatusLabel } from '$lib/observation-labels'
import { navigateToBundle, subscribeToObservations, type ObservationSubscription } from '$lib/observation-stream'
import type {
  ForestPage,
  ForestQuery,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  LiveContentBlock,
  ObservationStreamUpdate,
  FailedRequestDetail,
  FailedRequestSummary,
} from '$lib/types'
import InteractionCanvas from '$lib/components/interaction-canvas.svelte'
import ObservationInspector from '$lib/components/observation-inspector.svelte'
import FailedRequestTable from '$lib/components/failed-request-table.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import * as Alert from '$lib/components/ui/alert'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { Slider } from '$lib/components/ui/slider'
import { Switch } from '$lib/components/ui/switch'
import * as Tabs from '$lib/components/ui/tabs'

const batchSize = 12
const maxWindowMs = 86_400_000
const defaultWindowMs = 10 * 60_000
const presetMinutes = [5, 10, 30, 60, 240, 720, 1440]
const minTokenStops = [0, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000, 1_000_000]
const defaultMinTokenStop = 4
const queryClient = useQueryClient()
let activeTab = $state('interactions')
let anchorAt = $state(Date.now())
let windowIndex = $state(0)
let durationMs = $state(defaultWindowMs)
let customRange = $state(false)
let rangeOpen = $state(false)
let draftStart = $state('')
let draftEnd = $state('')
let workspace = $state<HTMLElement>()
let fullscreenButton = $state<HTMLButtonElement | null>(null)
let fullscreen = $state(false)
let rangeVersion = 0
let selectionVersion = 0
let roots = $state.raw<ForestRoot[]>([])
let rootTotal = $state(0)
let nextCursor = $state<string | null>()
let snapshotSequence = $state(0)
let windowStart = $state(Date.now() - defaultWindowMs)
let windowEnd = $state(Date.now())
let loading = $state(true)
let loadingMore = $state(false)
let rootBatchRequest: Promise<void> | undefined
let loadError = $state<unknown>()
let selectedInteraction = $state<InteractionSummary>()
let selectedFailure = $state<FailedRequestSummary>()
let interactionDetail = $state.raw<InteractionDetail>()
let liveBlocks = $state.raw<LiveContentBlock[]>([])
let liveGaps = $state.raw<string[]>([])
let liveCapacityGaps = $state.raw<string[]>([])
let olderLoading = $state(false)
const selectedLiveBlocks = $derived(liveBlocks.filter((block) => block.interaction_id === selectedInteraction?.id))
let failureDetail = $state<FailedRequestDetail>()
let detailLoading = $state(false)
let inspectorWidth = $state(46)
let canvas = $state<{
  focusLatest(): Promise<void>
  fitAfterAllLoaded(): Promise<void>
  focusNode(id: string): Promise<void>
}>()
let stream: ObservationSubscription | undefined
let streamConnected = $state(false)
let filterOpen = $state(false)
let providerFilter = $state('all')
let modelFilter = $state('all')
let apiKeyFilter = $state('all')
let statusFilter = $state('all')
let minTokenStop = $state(defaultMinTokenStop)
let clearOpen = $state(false)
let clearing = $state(false)
let clearResult = $state<{ skipped_active: number }>()
let debugConfirmOpen = $state(false)
let changingDebug = $state(false)
let debugClearOpen = $state(false)
let clearingDebug = $state(false)
let fitProgress = $state<number>()
let followPaused = $state(false)
let hasNewActivity = $state(false)
let migratedRoots = $state.raw(new Set<string>())
let failures = $state.raw<FailedRequestSummary[]>([])
let failureTotal = $state(0)
let failureCursor = $state<string | null>()
let failureLoading = $state(false)
let failureError = $state<unknown>()
let failureDetailError = $state<unknown>()
let failureRequestVersion = 0

const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const keysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const debugQuery = createQuery(() => ({ queryKey: ['observation-debug'], queryFn: admin.observations.debug }))

const interactions = $derived(roots.flatMap((root) => root.interactions))
// 交互链路成员规则：零客户端可见输出且最终失败的交互默认不进画布；「失败的请求」页
// 跳转、深链与跟随聚焦把它显式 reveal 后才可见。
let revealedFailures = $state.raw(new Set<string>())
const canvasRoots = $derived(
  roots
    .map((root) => ({
      ...root,
      interactions: root.interactions.filter((item) => revealedFailures.has(item.id) || !hiddenFailureNode(item)),
    }))
    .filter((root) => root.interactions.length > 0),
)
const canvasInteractions = $derived(canvasRoots.flatMap((root) => root.interactions))
const minTokensFilter = $derived(minTokenStops[minTokenStop] ?? 0)
const activeFilterCount = $derived(
  [providerFilter, modelFilter, apiKeyFilter, ...(activeTab === 'interactions' ? [statusFilter] : [])].filter(
    (value) => value !== 'all',
  ).length + Number(activeTab === 'interactions' && minTokensFilter > 0),
)
const latestInteraction = $derived.by(
  () =>
    [...canvasInteractions].sort(
      (a, b) => Number(b.status === 'running') - Number(a.status === 'running') || b.last_active_at - a.last_active_at,
    )[0],
)
const selectedPath = $derived.by(() => {
  const path = new SvelteSet<string>()
  let current = selectedInteraction
  while (current && !path.has(current.id)) {
    path.add(current.id)
    const parentId = visualParent(current, canvasInteractions)?.id
    current = parentId ? canvasInteractions.find((candidate) => candidate.id === parentId) : undefined
  }
  return path
})
const currentQuery = $derived<ForestQuery>({
  start_at: windowStart,
  end_at: windowEnd,
  limit: batchSize,
  provider: providerFilter === 'all' ? undefined : providerFilter,
  model: modelFilter === 'all' ? undefined : modelFilter,
  api_key: apiKeyFilter === 'all' ? undefined : apiKeyFilter,
  status: statusFilter === 'all' ? undefined : statusFilter,
  min_tokens: activeTab === 'interactions' && minTokensFilter > 0 ? minTokensFilter : undefined,
})
const liveWindow = $derived(!customRange && windowIndex === 0)
const draftStartMs = $derived(new Date(draftStart).getTime())
const draftEndMs = $derived(new Date(draftEnd).getTime())
const validDraftRange = $derived(
  Number.isFinite(draftStartMs) &&
    Number.isFinite(draftEndMs) &&
    draftEndMs > draftStartMs &&
    draftEndMs - draftStartMs <= maxWindowMs,
)
const selectedMigrated = $derived(
  selectedInteraction
    ? roots.find((root) => root.interactions.some((item) => item.id === selectedInteraction?.id))?.id
    : undefined,
)

onMount(() => {
  updateLiveBounds()
  void loadForest(true)
  const clock = setInterval(() => {
    if (!liveWindow) return
    updateLiveBounds()
    // 到期时重新查询完整结果与计数，不只过滤已经加载的节点。
    if (!loading && !loadingMore && roots.some((root) => root.last_active_at < windowStart)) {
      void loadForest(true)
    }
    if (activeTab === 'failures' && !failureLoading && failures.some((item) => item.started_at < windowStart)) {
      void loadFailures()
    }
  }, 1000)
  return () => {
    clearInterval(clock)
    rangeVersion += 1
    selectionVersion += 1
    stream?.close()
    if (fullscreen && document.fullscreenElement === workspace) {
      void document.exitFullscreen().catch((error: unknown) => toast.error(localizeBackendErrorMessage(error)))
    }
  }
})

$effect(() => {
  if (!fullscreen) return
  const overflow = document.body.style.overflow
  document.body.style.overflow = 'hidden'
  return () => {
    document.body.style.overflow = overflow
  }
})

function durationLabel(minutes: number): string {
  if (minutes < 60) return m.observation_window_minutes({ count: minutes })
  return minutes === 60 ? m.observation_window_hour() : m.observation_window_hours({ count: minutes / 60 })
}

function localDateTime(timestamp: number): string {
  const date = new Date(timestamp)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}

function openRange(): void {
  draftStart = localDateTime(windowStart)
  draftEnd = localDateTime(windowEnd)
  rangeOpen = true
}

function updateLiveBounds(): void {
  if (!liveWindow) return
  windowEnd = Date.now()
  windowStart = windowEnd - durationMs
}

function clearFilters(): void {
  providerFilter = 'all'
  modelFilter = 'all'
  apiKeyFilter = 'all'
  statusFilter = 'all'
  // 滑到 0 才显示被默认 10k 阈值隐藏的小链路。
  minTokenStop = 0
}

async function reloadWindow(): Promise<void> {
  rangeVersion += 1
  closeInspector()
  migratedRoots = new Set()
  followPaused = !liveWindow
  hasNewActivity = false
  failures = []
  failureCursor = undefined
  await Promise.all([loadForest(true), activeTab === 'failures' ? loadFailures() : Promise.resolve()])
}

async function choosePreset(value: string): Promise<void> {
  const minutes = Number(value)
  if (!presetMinutes.includes(minutes)) return
  durationMs = minutes * 60_000
  customRange = false
  anchorAt = Date.now()
  windowIndex = 0
  updateLiveBounds()
  await reloadWindow()
}

async function applyRange(): Promise<void> {
  if (!validDraftRange) return
  windowStart = draftStartMs
  windowEnd = draftEndMs
  durationMs = windowEnd - windowStart
  customRange = true
  windowIndex = 0
  rangeOpen = false
  await reloadWindow()
}

async function toggleFullscreen(): Promise<void> {
  try {
    if (fullscreen) {
      if (document.fullscreenElement === workspace) await document.exitFullscreen()
      fullscreen = false
      fullscreenButton?.focus()
    } else if (workspace) {
      if (document.fullscreenEnabled) await workspace.requestFullscreen()
      fullscreen = true
    }
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  }
}

$effect(() => {
  const id = page.url.searchParams.get('interaction')
  if (!id) return
  let active = true
  untrack(closeInspector)
  const selection = selectionVersion
  followPaused = true
  // 读取已 reveal 集合必须 untrack：本 effect 同时写入它，否则读写循环无限重触发。
  const currentRevealed = untrack(() => revealedFailures)
  if (!currentRevealed.has(id)) revealedFailures = new Set([...currentRevealed, id])
  detailLoading = true
  void admin.observations
    .interaction(id)
    .then((detail) => {
      if (!active || selection !== selectionVersion) return
      applySelectedDetail(detail, selection)
      return refreshSelectedEvents(id, selection)
    })
    .catch((error: unknown) => {
      if (active && selection === selectionVersion) toast.error(localizeBackendErrorMessage(error))
    })
    .finally(() => {
      if (active && selection === selectionVersion) detailLoading = false
    })
  return () => {
    active = false
  }
})

function applyPage(page: ForestPage, replace: boolean, advanceStream: boolean): void {
  roots = replace
    ? page.roots
    : [...roots, ...page.roots.filter((root) => !roots.some((known) => known.id === root.id))]
  rootTotal = page.root_total
  nextCursor = page.next_cursor
  snapshotSequence = page.snapshot_sequence
  if (replace && advanceStream) stream?.setCursor(page.snapshot_sequence)
  if (!stream) {
    stream = subscribeToObservations(page.snapshot_sequence, handleObservationUpdate, (connected) => {
      streamConnected = connected
      if (!connected) liveBlocks = []
    })
  }
}

async function loadForest(replace: boolean, advanceStream = true): Promise<void> {
  const version = rangeVersion
  if (replace) {
    loading = true
    loadError = undefined
  } else loadingMore = true
  try {
    const page = await admin.observations.forest({
      ...currentQuery,
      cursor: replace ? undefined : (nextCursor ?? undefined),
    })
    if (version !== rangeVersion) return
    applyPage(page, replace, advanceStream)
    loadError = undefined
    if (replace && liveWindow && !followPaused) {
      await tick()
      await canvas?.focusLatest()
    }
  } catch (error) {
    if (version === rangeVersion) loadError = error
  } finally {
    if (version === rangeVersion) {
      loading = false
      loadingMore = false
    }
  }
}

async function reloadForFilters(): Promise<void> {
  rangeVersion += 1
  closeInspector()
  followPaused = !liveWindow
  hasNewActivity = false
  migratedRoots = new Set()
  // 先推进实时窗口再查询，两次推进之间完成的失败不会被旧 end_at 排除。
  if (activeTab === 'failures') {
    updateLiveBounds()
    await loadFailures()
  } else await loadForest(true)
}

function loadNextRootBatch(): Promise<void> {
  if (rootBatchRequest) return rootBatchRequest
  if (!nextCursor) return Promise.resolve()
  rootBatchRequest = loadForest(false).finally(() => {
    rootBatchRequest = undefined
  })
  return rootBatchRequest
}

async function fitAll(): Promise<void> {
  if (!nextCursor) {
    await canvas?.fitAfterAllLoaded()
    return
  }
  fitProgress = Math.round((roots.length / Math.max(rootTotal, 1)) * 100)
  while (nextCursor) {
    await loadNextRootBatch()
    fitProgress = Math.round((roots.length / Math.max(rootTotal, 1)) * 100)
    if (loadError) break
  }
  await tick()
  await canvas?.fitAfterAllLoaded()
  fitProgress = undefined
}

function applySelectedDetail(detail: InteractionDetail, selection: number): void {
  if (selection !== selectionVersion) return
  if (interactionDetail && interactionDetail.snapshot_sequence > detail.snapshot_sequence) return
  selectedInteraction = detail.interaction
  interactionDetail = detail
  liveBlocks = withoutCommittedBlocks(liveBlocks, detail)
  detailLoading = false
}

async function refreshSelectedEvents(id: string, selection: number): Promise<void> {
  if (!interactionDetail || selection !== selectionVersion) return
  let after = interactionDetail.snapshot_sequence
  let through: number | undefined
  do {
    const page = await admin.observations.interactionEvents(id, { after_sequence: after, through_sequence: through })
    if (selection !== selectionVersion || !interactionDetail) return
    through ??= page.snapshot_sequence
    interactionDetail = {
      ...interactionDetail,
      runs: mergeObservationRuns(
        interactionDetail.runs,
        page.runs,
        page.snapshot_sequence < interactionDetail.snapshot_sequence,
      ),
      snapshot_sequence:
        page.next_cursor === null
          ? Math.max(through, interactionDetail.snapshot_sequence)
          : interactionDetail.snapshot_sequence,
    }
    liveBlocks = withoutCommittedBlocks(liveBlocks, interactionDetail)
    if (page.next_cursor === null) return
    after = page.next_cursor
  } while (selection === selectionVersion)
}

async function loadOlderEvents(): Promise<void> {
  if (!interactionDetail || interactionDetail.older_events_cursor === null || olderLoading) return
  const selection = selectionVersion
  const id = interactionDetail.interaction.id
  olderLoading = true
  try {
    const page = await admin.observations.interactionEvents(id, {
      before_sequence: interactionDetail.older_events_cursor,
      through_sequence: interactionDetail.snapshot_sequence,
    })
    if (selection !== selectionVersion || !interactionDetail) return
    interactionDetail = {
      ...interactionDetail,
      runs: mergeObservationRuns(interactionDetail.runs, page.runs, true),
      older_events_cursor: page.next_cursor,
    }
    liveBlocks = withoutCommittedBlocks(liveBlocks, interactionDetail)
  } catch (error) {
    if (selection === selectionVersion) toast.error(localizeBackendErrorMessage(error))
  } finally {
    if (selection === selectionVersion) olderLoading = false
  }
}

async function selectInteraction(interaction: InteractionSummary): Promise<void> {
  closeInspector()
  const selection = selectionVersion
  selectedInteraction = interaction
  detailLoading = true
  if (interaction.id !== latestInteraction?.id) followPaused = true
  try {
    applySelectedDetail(await admin.observations.interaction(interaction.id, currentQuery), selection)
    await refreshSelectedEvents(interaction.id, selection)
  } catch (error) {
    if (selection === selectionVersion) toast.error(localizeBackendErrorMessage(error))
  } finally {
    if (selection === selectionVersion) detailLoading = false
  }
}

async function selectFailure(failure: FailedRequestSummary): Promise<void> {
  closeInspector()
  const selection = selectionVersion
  selectedFailure = failure
  detailLoading = true
  try {
    const detail = await admin.observations.failure(failure.kind, failure.id)
    if (selection === selectionVersion) failureDetail = detail
  } catch (error) {
    if (selection === selectionVersion) failureDetailError = error
  } finally {
    if (selection === selectionVersion) detailLoading = false
  }
}

function closeInspector(): void {
  selectionVersion += 1
  olderLoading = false
  liveBlocks = retainLiveBlocks(liveBlocks)
  selectedInteraction = undefined
  selectedFailure = undefined
  interactionDetail = undefined
  failureDetail = undefined
  failureDetailError = undefined
  detailLoading = false
}

function applyLiveBlocks(blocks: LiveContentBlock[]): void {
  const retained = retainLiveBlocks(blocks, selectedInteraction?.id)
  if (retained.length !== blocks.length) {
    const ids = new Set(retained.map((block) => block.block_id))
    liveCapacityGaps = [
      ...new Set([
        ...liveCapacityGaps,
        ...blocks.filter((block) => !ids.has(block.block_id)).map((block) => block.interaction_id),
      ]),
    ].slice(-64)
  }
  liveBlocks = retained
}

async function handleObservationUpdate(update: ObservationStreamUpdate): Promise<void> {
  if (update.type === 'live_content') {
    const previous = liveBlocks.find((block) => block.block_id === update.block.block_id)
    if (previous && previous.revision >= update.block.revision) return
    let blocks = previous
      ? liveBlocks.map((block) => (block.block_id === update.block.block_id ? update.block : block))
      : [...liveBlocks, update.block]
    if (interactionDetail) blocks = withoutCommittedBlocks(blocks, interactionDetail)
    applyLiveBlocks(blocks)
    return
  }
  if (update.type === 'live_snapshot') {
    applyLiveBlocks(interactionDetail ? withoutCommittedBlocks(update.blocks, interactionDetail) : update.blocks)
    return
  }
  if (update.type === 'live_gap') {
    if (update.reason === 'live_capacity') {
      liveCapacityGaps = [
        ...liveCapacityGaps.filter((id) => id !== update.interaction_id),
        update.interaction_id,
      ].slice(-64)
    } else {
      liveGaps = [...liveGaps.filter((id) => id !== update.interaction_id), update.interaction_id].slice(-64)
    }
    return
  }
  const version = rangeVersion
  updateLiveBounds()
  if (update.type === 'reset_required') {
    selectionVersion += 1
    olderLoading = false
    liveBlocks = []
    liveGaps = []
    liveCapacityGaps = []
    interactionDetail = undefined
    const selection = selectionVersion
    const interactionId = selectedInteraction?.id
    const failure = selectedFailure
    detailLoading = Boolean(interactionId || failure)
    failureDetailError = undefined
    try {
      await Promise.all([
        loadForest(true, false),
        activeTab === 'failures' ? loadFailures() : Promise.resolve(),
        interactionId
          ? admin.observations
              .interaction(interactionId, currentQuery)
              .then((detail) => applySelectedDetail(detail, selection))
          : failure
            ? admin.observations.failure(failure.kind, failure.id).then((detail) => {
                if (selection === selectionVersion) failureDetail = detail
              })
            : Promise.resolve(),
      ])
      if (version !== rangeVersion) return
      if (loadError) throw loadError instanceof Error ? loadError : new Error(localizeBackendErrorMessage(loadError))
      stream?.setCursor(snapshotSequence)
    } catch (error) {
      if (version !== rangeVersion) return
      if (failure && selection === selectionVersion) failureDetailError = error
      loadError = error
      throw error
    } finally {
      if (selection === selectionVersion) detailLoading = false
    }
    return
  }
  if (update.event.interaction_id !== selectedInteraction?.id) {
    const blockId = eventBlockId(update.event)
    if (blockId) liveBlocks = liveBlocks.filter((block) => block.block_id !== blockId)
  }
  snapshotSequence = Math.max(snapshotSequence, update.event.sequence)
  if (liveWindow && activeTab === 'failures' && ['request_rejected', 'run_finished'].includes(update.event.kind))
    await loadFailures()
  if (followPaused) hasNewActivity = true
  if (!liveWindow && update.event.interaction_id && update.event.occurred_at >= windowEnd) {
    const root = roots.find((item) =>
      item.interactions.some((interaction) => interaction.id === update.event.interaction_id),
    )
    if (root) migratedRoots = new Set([...migratedRoots, root.id])
  }
  const interactionId = update.event.interaction_id
  if (interactionId) {
    const known = interactions.find((item) => item.id === interactionId)
    const selected = selectedInteraction?.id === interactionId
    const inspectorNeedsEvent = selected && (interactionDetail?.snapshot_sequence ?? 0) < update.event.sequence
    if (known && known.last_event_sequence >= update.event.sequence && !inspectorNeedsEvent) return
    const eventInWindow = update.event.occurred_at >= windowStart && update.event.occurred_at < windowEnd
    if (!known && !selected && !liveWindow && !eventInWindow) return
    const selection = selectionVersion
    try {
      const [snapshot] = await Promise.all([
        admin.observations.interactionSummary(interactionId, currentQuery),
        selected ? refreshSelectedEvents(interactionId, selection) : Promise.resolve(),
      ])
      if (version !== rangeVersion) return
      updateLiveBounds()
      const root = snapshot.root
      const existing = roots.some((item) => item.id === root.id)
      const inWindow = root.last_active_at >= windowStart && root.last_active_at < windowEnd
      // Keep loaded historical roots after migration, and live roots that advanced during this request.
      const keepLoaded = existing && (!liveWindow || root.last_active_at >= windowStart)
      const visible = (inWindow || keepLoaded) && root.interactions.some((item) => item.matched)
      if (visible) {
        roots = existing ? roots.map((item) => (item.id === root.id ? root : item)) : [...roots, root]
        if (!existing) rootTotal += 1
      } else if (existing) {
        roots = roots.filter((item) => item.id !== root.id)
        rootTotal = Math.max(0, rootTotal - 1)
      }
      if (selected && selection === selectionVersion) {
        selectedInteraction = snapshot.interaction
        if (interactionDetail)
          interactionDetail = { ...interactionDetail, interaction: snapshot.interaction, root: snapshot.root }
      }
      loadError = undefined
    } catch (error) {
      if (version !== rangeVersion) return
      loadError = error
      throw error
    }
  }
  if (!followPaused && liveWindow) {
    await tick()
    await canvas?.focusLatest()
  }
}

async function changeWindow(delta: number): Promise<void> {
  if (customRange) {
    windowStart -= delta * durationMs
    windowEnd -= delta * durationMs
  } else {
    if (liveWindow) anchorAt = windowEnd
    windowIndex = Math.max(0, windowIndex + delta)
    windowEnd = anchorAt - windowIndex * durationMs
    windowStart = windowEnd - durationMs
    updateLiveBounds()
  }
  await reloadWindow()
}

async function refreshAnchor(focusId?: string): Promise<void> {
  if (focusId) revealedFailures = new Set([...revealedFailures, focusId])
  if (focusId || !customRange) {
    customRange = false
    anchorAt = Date.now()
    windowIndex = 0
    updateLiveBounds()
  }
  await reloadWindow()
  if (focusId) {
    const interaction = canvasInteractions.find((item) => item.id === focusId)
    if (interaction) await selectInteraction(interaction)
  }
}

async function loadFailures(replace = true): Promise<void> {
  if (!replace && failureLoading) return
  const version = rangeVersion
  const requestVersion = ++failureRequestVersion
  failureLoading = true
  failureError = undefined
  try {
    const page = await admin.observations.failures({
      start_at: windowStart,
      end_at: windowEnd,
      limit: 30,
      cursor: replace ? undefined : (failureCursor ?? undefined),
      provider: currentQuery.provider,
      model: currentQuery.model,
      api_key: currentQuery.api_key,
    })
    if (version !== rangeVersion || requestVersion !== failureRequestVersion) return
    failures = replace ? page.items : [...failures, ...page.items]
    failureTotal = page.total
    failureCursor = page.next_cursor
  } catch (error) {
    if (version === rangeVersion && requestVersion === failureRequestVersion) failureError = error
  } finally {
    if (version === rangeVersion && requestVersion === failureRequestVersion) failureLoading = false
  }
}

async function openFailureInteraction(): Promise<void> {
  const id = failureDetail?.request.interaction_id
  if (!id) return
  const selection = selectionVersion
  try {
    const snapshot = await admin.observations.interactionSummary(id)
    if (selection !== selectionVersion) return
    closeInspector()
    activeTab = 'interactions'
    followPaused = true
    revealedFailures = new Set([...revealedFailures, id])
    roots = [...roots.filter((root) => root.id !== snapshot.root.id), snapshot.root]
    await tick()
    await canvas?.focusNode(id)
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  }
}

async function tabChanged(value: string): Promise<void> {
  activeTab = value
  closeInspector()
  // 先推进实时窗口再查询，两次推进之间完成的失败不会被旧 end_at 排除。
  if (value === 'failures') {
    updateLiveBounds()
    await loadFailures()
  } else await loadForest(true)
}

async function disableDebug(): Promise<void> {
  changingDebug = true
  try {
    await admin.observations.setDebug(false)
    await queryClient.invalidateQueries({ queryKey: ['observation-debug'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    changingDebug = false
  }
}

async function enableDebug(): Promise<void> {
  changingDebug = true
  try {
    await admin.observations.setDebug(true)
    debugConfirmOpen = false
    await queryClient.invalidateQueries({ queryKey: ['observation-debug'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    changingDebug = false
  }
}

async function clearDebugData(): Promise<void> {
  clearingDebug = true
  try {
    await admin.observations.clearDebug()
    debugClearOpen = false
    const selection = selectionVersion
    const interactionId = selectedInteraction?.id
    const failure = selectedFailure
    await queryClient.invalidateQueries({ queryKey: ['observation-debug'] })
    if (interactionId && selection === selectionVersion) {
      applySelectedDetail(await admin.observations.interaction(interactionId, currentQuery), selection)
      await refreshSelectedEvents(interactionId, selection)
    }
    if (failure && selection === selectionVersion) {
      failureDetail = await admin.observations.failure(failure.kind, failure.id)
    }
    toast.success(m.observation_debug_cleared())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    clearingDebug = false
  }
}

async function clearHistory(): Promise<void> {
  clearing = true
  try {
    const result = await admin.observations.clearHistory()
    clearResult = result
    clearOpen = false
    await Promise.all([
      loadForest(true),
      activeTab === 'failures' ? loadFailures() : Promise.resolve(),
      queryClient.invalidateQueries({ queryKey: ['observation-debug'] }),
    ])
    toast.success(
      m.observation_history_cleared({
        interactions: result.deleted_interactions,
        rejections: result.deleted_rejections,
        skipped: result.skipped_active,
      }),
    )
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    clearing = false
  }
}

async function downloadBundle(): Promise<void> {
  const kind = interactionDetail || failureDetail?.request.interaction_id ? 'interaction' : 'rejected_request'
  const id = interactionDetail?.interaction.id ?? failureDetail?.request.interaction_id ?? failureDetail?.request.id
  if (!id) return
  try {
    await navigateToBundle(await admin.observations.issueBundleTicket(kind, id))
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  }
}

function formatBytes(value: number | undefined): string {
  if (value == null) return '–'
  const unit = value >= 1024 ** 3 ? 'GiB' : 'MiB'
  const divisor = unit === 'GiB' ? 1024 ** 3 : 1024 ** 2
  return `${(value / divisor).toFixed(value % divisor === 0 ? 0 : 1)} ${unit}`
}
</script>

<svelte:head><title>{m.common_request_history()} · Stravia</title></svelte:head>

<svelte:document
  onfullscreenchange={() => {
    fullscreen = document.fullscreenElement === workspace
    if (!fullscreen) fullscreenButton?.focus()
  }} />
<svelte:window
  onkeydown={(event: KeyboardEvent) => {
    if (event.key === 'Escape' && fullscreen && !rangeOpen && !event.defaultPrevented) {
      void toggleFullscreen()
    }
  }} />

{#snippet liveMeta()}
  <StatusIndicator
    compact
    label={streamConnected ? m.observation_live() : m.observation_reconnecting()}
    tone={streamConnected ? 'healthy' : 'neutral'} />
{/snippet}

{#snippet headerActions()}
  <div class="flex flex-wrap items-center gap-2">
    <div class="debug-toggle">
      <BugIcon aria-hidden="true" /><span>{m.observation_debug()}</span><Switch
        bind:checked={
          () => debugQuery.data?.enabled ?? false,
          (enabled) => (enabled ? (debugConfirmOpen = true) : void disableDebug())
        }
        disabled={changingDebug || debugQuery.isPending || !debugQuery.data}
        aria-label={m.observation_debug()} />
    </div>
    {#if debugQuery.data?.enabled}
      <Button variant="outline" onclick={() => (debugClearOpen = true)}
        ><Trash2Icon data-icon="inline-start" />{m.observation_clear_debug()}</Button>
    {/if}
    <Button variant="outline" onclick={() => (filterOpen = true)}
      ><SlidersHorizontalIcon data-icon="inline-start" />{m.observation_filters()}{#if activeFilterCount}<span
          >· {activeFilterCount}</span
        >{/if}</Button>
    <Button variant="destructive" onclick={() => (clearOpen = true)}
      ><Trash2Icon data-icon="inline-start" />{m.observation_clear_history()}</Button>
  </div>
{/snippet}

<div class="route-page observation-page">
  <PageHeader
    eyebrow={m.common_monitor()}
    title={m.common_request_history()}
    description={m.observation_page_summary()}
    meta={liveMeta}
    actions={headerActions} />

  {#if clearResult?.skipped_active}
    <Alert.Root variant="warning" role="status">
      <Alert.Description>{m.observation_clear_skipped_active({ count: clearResult.skipped_active })}</Alert.Description>
    </Alert.Root>
  {/if}

  <section
    bind:this={workspace}
    class={['observation-workspace', { 'workspace-fullscreen': fullscreen }]}
    aria-labelledby="observation-workspace-title">
    <h2 id="observation-workspace-title" class="sr-only">{m.observation_interaction_chains()}</h2>
    <div class="workspace-toolbar">
      <Tabs.Root value={activeTab} onValueChange={(value: string) => void tabChanged(value)}>
        <Tabs.List
          ><Tabs.Trigger value="interactions">{m.observation_interaction_chains()}</Tabs.Trigger><Tabs.Trigger
            value="failures">{m.observation_failed_requests()}</Tabs.Trigger
          ></Tabs.List>
      </Tabs.Root>
      <div class="window-controls">
        <Select.Root
          type="single"
          value={customRange ? '' : String(durationMs / 60_000)}
          onValueChange={(value: string) => void choosePreset(value)}>
          <Select.Trigger aria-label={m.observation_time_window()} class="w-28">
            {customRange ? m.observation_custom_range() : durationLabel(durationMs / 60_000)}
          </Select.Trigger>
          <Select.Content portalProps={{ to: fullscreen ? workspace : undefined }}>
            <Select.Group>
              {#each presetMinutes as minutes (minutes)}
                <Select.Item value={String(minutes)} label={durationLabel(minutes)}>
                  {durationLabel(minutes)}
                </Select.Item>
              {/each}
            </Select.Group>
          </Select.Content>
        </Select.Root>
        <Button variant="ghost" size="sm" disabled={loading} onclick={() => void changeWindow(1)}
          >{m.observation_older_window()}</Button>
        <Button
          variant="ghost"
          size="sm"
          class="range-picker"
          aria-label={m.observation_choose_range()}
          onclick={openRange}>
          <CalendarRangeIcon data-icon="inline-start" />
          <span class="range-label">
            {formatLogTime(windowStart)} — {liveWindow ? m.observation_live() : formatLogTime(windowEnd)}
          </span>
        </Button>
        <Button variant="ghost" size="sm" disabled={liveWindow || loading} onclick={() => void changeWindow(-1)}
          >{m.observation_newer_window()}</Button>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={m.observation_refresh_anchor()}
          onclick={() => void refreshAnchor()}><RefreshCwIcon /></Button>
        <Button
          bind:ref={fullscreenButton}
          variant="ghost"
          size="icon-sm"
          aria-label={fullscreen ? m.observation_exit_fullscreen() : m.observation_enter_fullscreen()}
          title={fullscreen ? m.observation_exit_fullscreen() : m.observation_enter_fullscreen()}
          onclick={() => void toggleFullscreen()}>
          {#if fullscreen}<MinimizeIcon />{:else}<MaximizeIcon />{/if}
        </Button>
      </div>
    </div>

    {#if activeTab === 'interactions'}
      <div class="canvas-stage">
        {#if loading}
          <div class="stage-state"><p>{m.observation_loading_chains()}</p></div>
        {:else if loadError}
          <div class="stage-state">
            <RequestFailure message={localizeBackendErrorMessage(loadError)} retry={() => loadForest(true)} />
          </div>
        {:else if canvasRoots.length === 0}
          <div class="stage-state">
            <Empty.Root
              ><Empty.Header
                ><Empty.Title
                  >{activeFilterCount ? m.observation_no_matching_chains() : m.observation_no_chains()}</Empty.Title
                ><Empty.Description
                  >{activeFilterCount
                    ? m.observation_clear_filters_help()
                    : m.observation_new_requests_appear()}</Empty.Description
                ></Empty.Header
              >{#if activeFilterCount}<Empty.Content
                  ><Button
                    variant="outline"
                    onclick={() => {
                      clearFilters()
                      void reloadForFilters()
                    }}>{m.observation_clear_filters()}</Button>
                  ></Empty.Content
                >{/if}</Empty.Root>
          </div>
        {:else}
          <SvelteFlowProvider>
            <InteractionCanvas
              bind:this={canvas}
              roots={canvasRoots}
              selectedId={selectedInteraction?.id}
              {selectedPath}
              {loadingMore}
              {nextCursor}
              {rootTotal}
              latestId={latestInteraction?.id}
              {followPaused}
              newActivityAvailable={hasNewActivity}
              {fitProgress}
              onselect={(item: InteractionSummary) => void selectInteraction(item)}
              onloadmore={() => void loadNextRootBatch()}
              onfitall={() => void fitAll()}
              onmanualmove={() => (followPaused = true)}
              onfollow={() => {
                followPaused = false
                hasNewActivity = false
              }} />
          </SvelteFlowProvider>
        {/if}
        {#if selectedInteraction}
          <ObservationInspector
            portalTarget={fullscreen ? workspace : undefined}
            interaction={interactionDetail}
            liveBlocks={selectedLiveBlocks}
            liveGap={liveGaps.includes(selectedInteraction.id)}
            liveCapacity={liveCapacityGaps.includes(selectedInteraction.id)}
            {olderLoading}
            onolder={loadOlderEvents}
            loading={detailLoading}
            width={inspectorWidth}
            onwidthchange={(value: number) => (inspectorWidth = value)}
            onclose={closeInspector}
            onbundle={() => void downloadBundle()}
            onlatest={selectedMigrated && migratedRoots.has(selectedMigrated)
              ? () => void refreshAnchor(selectedInteraction?.id)
              : undefined} />
        {/if}
      </div>
    {:else}
      <div class="failures-view">
        <header class="flex items-center justify-between gap-3 border-b p-4">
          <h2 class="sr-only">{m.observation_failed_requests()}</h2>
          <span class="font-technical text-xs text-muted-foreground">{failures.length} / {failureTotal}</span>
        </header>
        {#if failureError}
          <RequestFailure
            message={localizeBackendErrorMessage(failureError)}
            retry={() => loadFailures()}
            retrying={failureLoading} />
        {/if}
        {#if failureLoading && failures.length === 0}
          <div class="stage-state" role="status">{m.observation_loading_failures()}</div>
        {:else if failures.length === 0 && !failureError}<Empty.Root class="flex-1"
            ><Empty.Header
              ><Empty.Title>{m.observation_no_failures()}</Empty.Title><Empty.Description
                >{m.observation_no_failures_description()}</Empty.Description
              ></Empty.Header
            ></Empty.Root>
        {:else if failures.length > 0}
          <FailedRequestTable
            items={failures}
            loading={failureLoading}
            onselect={(failure: FailedRequestSummary) => void selectFailure(failure)} />
        {/if}
        {#if failureCursor}<div class="border-t p-3 text-center">
            <Button variant="outline" disabled={failureLoading} onclick={() => void loadFailures(false)}
              >{failureLoading ? m.observation_loading_more() : m.observation_load_more()}</Button>
          </div>{/if}
        {#if selectedFailure}<ObservationInspector
            portalTarget={fullscreen ? workspace : undefined}
            failure={failureDetail}
            error={failureDetailError ? localizeBackendErrorMessage(failureDetailError) : undefined}
            onretry={() => selectedFailure && void selectFailure(selectedFailure)}
            oninteraction={failureDetail?.request.interaction_id ? () => void openFailureInteraction() : undefined}
            loading={detailLoading}
            width={inspectorWidth}
            onwidthchange={(value: number) => (inspectorWidth = value)}
            onclose={closeInspector}
            onbundle={() => void downloadBundle()} />{/if}
      </div>
    {/if}
  </section>
</div>

<Dialog.Root bind:open={rangeOpen}>
  <Dialog.Content portalProps={{ to: fullscreen ? workspace : undefined }}>
    <Dialog.Header>
      <Dialog.Title>{m.observation_date_time_range()}</Dialog.Title>
      <Dialog.Description>{m.observation_range_help()}</Dialog.Description>
    </Dialog.Header>
    <form
      class="flex flex-col gap-4"
      onsubmit={(event) => {
        event.preventDefault()
        void applyRange()
      }}>
      <Field.FieldGroup>
        <Field.Field data-invalid={!validDraftRange || undefined}>
          <Field.FieldLabel for="observation-start">{m.observation_start_time()}</Field.FieldLabel>
          <Input
            id="observation-start"
            type="datetime-local"
            step="1"
            required
            bind:value={draftStart}
            aria-invalid={!validDraftRange}
            aria-describedby="observation-range-help" />
        </Field.Field>
        <Field.Field data-invalid={!validDraftRange || undefined}>
          <Field.FieldLabel for="observation-end">{m.observation_end_time()}</Field.FieldLabel>
          <Input
            id="observation-end"
            type="datetime-local"
            step="1"
            required
            bind:value={draftEnd}
            aria-invalid={!validDraftRange}
            aria-describedby="observation-range-help" />
        </Field.Field>
      </Field.FieldGroup>
      <p id="observation-range-help" class="text-sm text-muted-foreground" role="status">
        {validDraftRange ? m.observation_range_local_time() : m.observation_range_invalid()}
      </p>
      <Dialog.Footer>
        <Button variant="outline" onclick={() => (rangeOpen = false)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={!validDraftRange}>{m.observation_apply_range()}</Button>
      </Dialog.Footer>
    </form>
  </Dialog.Content>
</Dialog.Root>

<Sheet.Root bind:open={filterOpen}
  ><Sheet.Content side="right" class="w-full! max-w-none! sm:max-w-sm!" closeLabel={m.observation_close_filters()}
    ><Sheet.Header
      ><Sheet.Title>{m.observation_filters()}</Sheet.Title><Sheet.Description
        >{m.observation_filters_description()}</Sheet.Description
      ></Sheet.Header>
    <div class="route-overlay-body">
      <Field.FieldGroup>
        <Field.Field
          ><Field.FieldLabel for="observation-provider">{m.common_model_service()}</Field.FieldLabel><Select.Root
            type="single"
            bind:value={providerFilter}
            ><Select.Trigger id="observation-provider" class="w-full"
              >{providersQuery.data?.find((item) => item.id === providerFilter)?.name ??
                m.observation_all()}</Select.Trigger
            ><Select.Content
              ><Select.Group
                ><Select.Item value="all">{m.observation_all()}</Select.Item
                >{#each providersQuery.data ?? [] as provider (provider.id)}<Select.Item
                    value={provider.id}
                    label={provider.name}>{provider.name}</Select.Item
                  >{/each}</Select.Group
              ></Select.Content
            ></Select.Root
          ></Field.Field>
        <Field.Field
          ><Field.FieldLabel for="observation-model">{m.common_model()}</Field.FieldLabel><Select.Root
            type="single"
            bind:value={modelFilter}
            ><Select.Trigger id="observation-model" class="w-full"
              >{modelsQuery.data?.find((item) => item.id === modelFilter)?.display_name ??
                m.observation_all()}</Select.Trigger
            ><Select.Content
              ><Select.Group
                ><Select.Item value="all">{m.observation_all()}</Select.Item
                >{#each modelsQuery.data ?? [] as model (model.id)}<Select.Item
                    value={model.id}
                    label={model.display_name || model.id}>{model.display_name || model.id}</Select.Item
                  >{/each}</Select.Group
              ></Select.Content
            ></Select.Root
          ></Field.Field>
        <Field.Field
          ><Field.FieldLabel for="observation-key">{m.common_api_key()}</Field.FieldLabel><Select.Root
            type="single"
            bind:value={apiKeyFilter}
            ><Select.Trigger id="observation-key" class="w-full"
              >{keysQuery.data?.find((item) => item.id === apiKeyFilter)?.name ?? m.observation_all()}</Select.Trigger
            ><Select.Content
              ><Select.Group
                ><Select.Item value="all">{m.observation_all()}</Select.Item
                >{#each keysQuery.data ?? [] as key (key.id)}<Select.Item value={key.id} label={key.name}
                    >{key.name}</Select.Item
                  >{/each}</Select.Group
              ></Select.Content
            ></Select.Root
          ></Field.Field>
        {#if activeTab === 'interactions'}<Field.Field
            ><Field.FieldLabel for="observation-status">{m.common_status()}</Field.FieldLabel><Select.Root
              type="single"
              bind:value={statusFilter}
              ><Select.Trigger id="observation-status" class="w-full"
                >{statusFilter === 'all' ? m.observation_all() : observationStatusLabel(statusFilter)}</Select.Trigger
              ><Select.Content
                ><Select.Group
                  >{#each ['all', 'running', 'waiting_client', 'completed', 'interrupted'] as status (status)}<Select.Item
                      value={status}
                      >{status === 'all' ? m.observation_all() : observationStatusLabel(status)}</Select.Item
                    >{/each}</Select.Group
                ></Select.Content
              ></Select.Root
            ></Field.Field>
          <Field.Field>
            <Field.FieldLabel for="observation-min-tokens" hint={m.observation_min_tokens_hint()}
              >{m.observation_min_tokens()}</Field.FieldLabel>
            <div class="flex min-h-10 items-center gap-3">
              <Slider
                id="observation-min-tokens"
                bind:value={minTokenStop}
                min={0}
                max={minTokenStops.length - 1}
                step={1}
                class="flex-1"
                aria-label={m.observation_min_tokens()} />
              <span class="font-technical w-14 shrink-0 text-right text-sm tabular-nums" aria-live="polite"
                >{minTokensFilter > 0 ? formatCompactCount(minTokensFilter) : m.observation_all()}</span>
            </div>
          </Field.Field>
        {/if}
      </Field.FieldGroup>
    </div>
    <Sheet.Footer
      ><Button variant="ghost" onclick={clearFilters}>{m.observation_clear_filters()}</Button><Button
        onclick={() => {
          filterOpen = false
          void reloadForFilters()
        }}>{m.observation_apply_filters()}</Button
      ></Sheet.Footer
    ></Sheet.Content
  ></Sheet.Root>

<AlertDialog.Root bind:open={debugConfirmOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.observation_enable_debug()}</AlertDialog.Title>
      <AlertDialog.Description>
        {m.observation_debug_warning({ retention_days: debugQuery.data?.retention_days ?? 0 })}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <div class="rounded-md border p-3 text-sm">
      <p>{m.observation_debug_retained({ retained: formatBytes(debugQuery.data?.retained_bytes) })}</p>
      <p class="mt-1 text-muted-foreground">{m.observation_debug_disable_retains()}</p>
    </div>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action disabled={changingDebug} onclick={() => void enableDebug()}
        >{changingDebug ? m.observation_enabling() : m.observation_enable_debug()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<AlertDialog.Root bind:open={debugClearOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.observation_clear_debug()}</AlertDialog.Title>
      <AlertDialog.Description
        >{m.observation_clear_debug_warning({
          retained: formatBytes(debugQuery.data?.retained_bytes),
        })}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" disabled={clearingDebug} onclick={() => void clearDebugData()}
        >{clearingDebug ? m.observation_clearing() : m.observation_clear_debug()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<AlertDialog.Root bind:open={clearOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.observation_clear_history()}</AlertDialog.Title>
      <AlertDialog.Description>{m.observation_clear_warning()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" disabled={clearing} onclick={() => void clearHistory()}
        >{clearing ? m.observation_clearing() : m.observation_clear_history()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<style>
.observation-page {
  min-height: 0;
  /* 与壳层保持一致：扣除标题栏、内容内边距和桌面底部沟槽。 */
  height: calc(100svh - 5rem);
}
.observation-workspace {
  position: relative;
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
  overflow: hidden;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--background);
}
.workspace-toolbar {
  display: flex;
  flex-shrink: 0;
  min-height: 3.5rem;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  flex-wrap: wrap;
  border-bottom: 1px solid var(--border);
  padding: 0.55rem 0.75rem;
}
.window-controls {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.3rem;
}
.range-label {
  font-family: var(--font-technical);
  font-size: 0.7rem;
}
.observation-workspace.workspace-fullscreen,
.observation-workspace:fullscreen {
  position: fixed;
  inset: 0;
  z-index: 40;
  width: 100%;
  height: 100dvh;
  min-height: 0;
  border: 0;
  border-radius: 0;
}
.canvas-stage {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: hidden;
}
.stage-state {
  display: grid;
  height: 100%;
  place-items: center;
  gap: 0.75rem;
  padding: 2rem;
  text-align: center;
  color: var(--muted-foreground);
}
.debug-toggle {
  display: inline-flex;
  height: 2.25rem;
  align-items: center;
  gap: 0.5rem;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding-inline: 0.65rem;
  font-size: 0.75rem;
  font-weight: 500;
}
.debug-toggle > :global(svg) {
  width: 1rem;
}
.failures-view {
  position: relative;
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
  overflow: hidden;
}
.failures-view > header,
.failures-view > .border-t {
  flex-shrink: 0;
}
.failures-view > .stage-state {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}
@media (max-width: 767px) {
  .observation-page {
    height: calc(100svh - 4.5rem);
  }
  .workspace-toolbar {
    align-items: stretch;
    flex-direction: column;
  }
  .window-controls {
    justify-content: space-between;
  }
  .window-controls :global(.range-picker) {
    order: 1;
    width: 100%;
  }
}
</style>

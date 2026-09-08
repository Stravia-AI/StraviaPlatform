<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onMount, tick } from 'svelte'
import { page } from '$app/state'
import { SvelteSet } from 'svelte/reactivity'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { SvelteFlowProvider } from '@xyflow/svelte'
import BugIcon from '@lucide/svelte/icons/bug'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatLogTime } from '$lib/format'
import { observationDebugStatusLabel, observationStatusLabel } from '$lib/observation-labels'
import { navigateToBundle, subscribeToObservations, type ObservationSubscription } from '$lib/observation-stream'
import type {
  ForestPage,
  ForestQuery,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  RejectionDetail,
  RejectionSummary,
} from '$lib/types'
import InteractionCanvas from '$lib/components/interaction-canvas.svelte'
import ObservationInspector from '$lib/components/observation-inspector.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import * as Alert from '$lib/components/ui/alert'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { Switch } from '$lib/components/ui/switch'
import * as Tabs from '$lib/components/ui/tabs'

const batchSize = 12
const queryClient = useQueryClient()
let activeTab = $state('interactions')
let anchorAt = $state(Date.now())
let windowIndex = $state(0)
let roots = $state.raw<ForestRoot[]>([])
let rootTotal = $state(0)
let nextCursor = $state<string | null>()
let snapshotSequence = $state(0)
let windowStart = $state(0)
let windowEnd = $state(0)
let loading = $state(true)
let loadingMore = $state(false)
let rootBatchRequest: Promise<void> | undefined
let loadError = $state<unknown>()
let selectedInteraction = $state<InteractionSummary>()
let selectedRejection = $state<RejectionSummary>()
let interactionDetail = $state<InteractionDetail>()
let rejectionDetail = $state<RejectionDetail>()
let detailLoading = $state(false)
let inspectorWidth = $state(46)
let canvas = $state<InteractionCanvas>()
let stream: ObservationSubscription | undefined
let streamConnected = $state(false)
let filterOpen = $state(false)
let providerFilter = $state('all')
let modelFilter = $state('all')
let apiKeyFilter = $state('all')
let statusFilter = $state('all')
let clearOpen = $state(false)
let clearing = $state(false)
let clearResult = $state<{ skipped_active: number }>()
let debugConfirmOpen = $state(false)
let changingDebug = $state(false)
let fitProgress = $state<number>()
let followPaused = $state(false)
let hasNewActivity = $state(false)
let migratedRoots = $state.raw(new Set<string>())
let rejections = $state.raw<RejectionSummary[]>([])
let rejectionTotal = $state(0)
let rejectionCursor = $state<string | null>()
let rejectionLoading = $state(false)

const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const keysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const debugQuery = createQuery(() => ({ queryKey: ['observation-debug'], queryFn: admin.observations.debug }))

const interactions = $derived(roots.flatMap((root) => root.interactions))
const activeFilterCount = $derived(
  [providerFilter, modelFilter, apiKeyFilter, statusFilter].filter((value) => value !== 'all').length,
)
const latestInteraction = $derived.by(
  () =>
    [...interactions].sort(
      (a, b) => Number(b.status === 'running') - Number(a.status === 'running') || b.last_active_at - a.last_active_at,
    )[0],
)
const selectedPath = $derived.by(() => {
  const path = new SvelteSet<string>()
  let current = selectedInteraction
  while (current) {
    path.add(current.id)
    current = interactions.find((candidate) => candidate.id === current?.parent_interaction_id)
  }
  return path
})
const currentQuery = $derived<ForestQuery>({
  anchor_at: anchorAt,
  window_index: windowIndex,
  limit: batchSize,
  provider: providerFilter === 'all' ? undefined : providerFilter,
  model: modelFilter === 'all' ? undefined : modelFilter,
  api_key: apiKeyFilter === 'all' ? undefined : apiKeyFilter,
  status: statusFilter === 'all' ? undefined : statusFilter,
})
const selectedMigrated = $derived(
  selectedInteraction
    ? roots.find((root) => root.interactions.some((item) => item.id === selectedInteraction?.id))?.id
    : undefined,
)

onMount(() => {
  void loadForest(true)
  return () => stream?.close()
})

$effect(() => {
  const id = page.url.searchParams.get('interaction')
  if (!id) return
  let active = true
  followPaused = true
  detailLoading = true
  void admin.observations
    .interaction(id)
    .then((detail) => {
      if (!active) return
      selectedInteraction = detail.interaction
      selectedRejection = undefined
      interactionDetail = detail
      rejectionDetail = undefined
    })
    .catch((error: unknown) => {
      if (active) toast.error(localizeBackendErrorMessage(error))
    })
    .finally(() => {
      if (active) detailLoading = false
    })
  return () => {
    active = false
  }
})

function applyPage(page: ForestPage, replace: boolean): void {
  roots = replace
    ? page.roots
    : [...roots, ...page.roots.filter((root) => !roots.some((known) => known.id === root.id))]
  rootTotal = page.root_total
  nextCursor = page.next_cursor
  snapshotSequence = page.snapshot_sequence
  windowStart = page.window_start
  windowEnd = page.window_end
  if (replace) stream?.setCursor(page.snapshot_sequence)
  if (!stream) {
    stream = subscribeToObservations(
      page.snapshot_sequence,
      handleObservationUpdate,
      (connected) => (streamConnected = connected),
    )
  }
}

async function loadForest(replace: boolean): Promise<void> {
  if (replace) {
    loading = true
    loadError = undefined
  } else loadingMore = true
  try {
    const page = await admin.observations.forest({
      ...currentQuery,
      cursor: replace ? undefined : (nextCursor ?? undefined),
    })
    applyPage(page, replace)
    loadError = undefined
    if (replace && windowIndex === 0 && !followPaused) {
      await tick()
      await canvas?.focusLatest()
    }
  } catch (error) {
    loadError = error
  } finally {
    loading = false
    loadingMore = false
  }
}

async function reloadForFilters(): Promise<void> {
  selectedInteraction = undefined
  interactionDetail = undefined
  followPaused = false
  hasNewActivity = false
  migratedRoots = new Set()
  await loadForest(true)
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

async function selectInteraction(interaction: InteractionSummary): Promise<void> {
  selectedInteraction = interaction
  selectedRejection = undefined
  interactionDetail = undefined
  rejectionDetail = undefined
  detailLoading = true
  if (interaction.id !== latestInteraction?.id) followPaused = true
  try {
    interactionDetail = await admin.observations.interaction(interaction.id, currentQuery)
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    detailLoading = false
  }
}

async function selectRejection(rejection: RejectionSummary): Promise<void> {
  selectedRejection = rejection
  selectedInteraction = undefined
  rejectionDetail = undefined
  interactionDetail = undefined
  detailLoading = true
  try {
    rejectionDetail = await admin.observations.rejection(rejection.id)
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    detailLoading = false
  }
}

function closeInspector(): void {
  selectedInteraction = undefined
  selectedRejection = undefined
  interactionDetail = undefined
  rejectionDetail = undefined
}

async function handleObservationUpdate(update: import('$lib/types').ObservationStreamUpdate): Promise<void> {
  if (update.type === 'reset_required') {
    await loadForest(true)
    if (loadError) throw loadError
    return
  }
  snapshotSequence = Math.max(snapshotSequence, update.event.sequence)
  if (followPaused) hasNewActivity = true
  if (windowIndex > 0 && update.event.interaction_id && update.event.occurred_at >= windowEnd) {
    const root = roots.find((item) =>
      item.interactions.some((interaction) => interaction.id === update.event.interaction_id),
    )
    if (root) migratedRoots = new Set([...migratedRoots, root.id])
  }
  if (update.event.interaction_id && interactions.some((item) => item.id === update.event.interaction_id)) {
    const known = interactions.find((item) => item.id === update.event.interaction_id)
    const inspectorNeedsEvent =
      selectedInteraction?.id === update.event.interaction_id &&
      (interactionDetail?.snapshot_sequence ?? 0) < update.event.sequence
    if (known && known.last_event_sequence >= update.event.sequence && !inspectorNeedsEvent) return
    try {
      const detail = await admin.observations.interaction(update.event.interaction_id, currentQuery)
      if (detail.root.interactions.some((item) => item.matched)) {
        roots = roots.map((root) => (root.id === detail.root.id ? detail.root : root))
      } else {
        roots = roots.filter((root) => root.id !== detail.root.id)
        rootTotal = Math.max(0, rootTotal - 1)
      }
      if (selectedInteraction?.id === detail.interaction.id) {
        selectedInteraction = detail.interaction
        interactionDetail = detail
      }
      loadError = undefined
    } catch (error) {
      loadError = error
      throw error
    }
  } else if (windowIndex === 0 && update.event.interaction_id) {
    try {
      const detail = await admin.observations.interaction(update.event.interaction_id, currentQuery)
      const existingIndex = roots.findIndex((root) => root.id === detail.root.id)
      if (existingIndex >= 0) roots = roots.map((root) => (root.id === detail.root.id ? detail.root : root))
      else if (detail.root.interactions.some((interaction) => interaction.matched)) {
        roots = [...roots, detail.root]
        rootTotal += 1
      }
      loadError = undefined
    } catch (error) {
      loadError = error
      throw error
    }
  }
  if (!followPaused && windowIndex === 0) {
    await tick()
    await canvas?.focusLatest()
  }
}

async function changeWindow(delta: number): Promise<void> {
  windowIndex = Math.max(0, windowIndex + delta)
  closeInspector()
  migratedRoots = new Set()
  followPaused = windowIndex !== 0
  hasNewActivity = false
  await loadForest(true)
}

async function refreshAnchor(focusId?: string): Promise<void> {
  anchorAt = Date.now()
  windowIndex = 0
  migratedRoots = new Set()
  followPaused = false
  hasNewActivity = false
  await loadForest(true)
  if (focusId) {
    const interaction = interactions.find((item) => item.id === focusId)
    if (interaction) await selectInteraction(interaction)
  }
}

async function loadRejections(replace = true): Promise<void> {
  rejectionLoading = true
  try {
    const page = await admin.observations.rejections({
      anchor_at: anchorAt,
      window_index: windowIndex,
      limit: 30,
      cursor: replace ? undefined : (rejectionCursor ?? undefined),
    })
    rejections = replace ? page.items : [...rejections, ...page.items]
    rejectionTotal = page.total
    rejectionCursor = page.next_cursor
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    rejectionLoading = false
  }
}

async function tabChanged(value: string): Promise<void> {
  activeTab = value
  closeInspector()
  if (value === 'rejections' && rejections.length === 0) await loadRejections()
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

async function clearHistory(): Promise<void> {
  clearing = true
  try {
    const result = await admin.observations.clearHistory()
    clearResult = result
    clearOpen = false
    await Promise.all([loadForest(true), activeTab === 'rejections' ? loadRejections() : Promise.resolve()])
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
  const kind = interactionDetail ? 'interaction' : 'rejected_request'
  const id = interactionDetail?.interaction.id ?? rejectionDetail?.rejection.id
  const through = interactionDetail?.snapshot_sequence ?? rejectionDetail?.snapshot_sequence
  if (!id) return
  try {
    await navigateToBundle(await admin.observations.issueBundleTicket(kind, id, through))
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
    <Button variant="outline" onclick={() => (filterOpen = true)} disabled={activeTab !== 'interactions'}
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

  {#if (debugQuery.data?.partial_trace_count ?? 0) > 0}
    <Alert.Root variant="warning" role="status">
      <Alert.Description
        >{m.observation_partial_traces_warning({
          count: debugQuery.data?.partial_trace_count ?? 0,
        })}</Alert.Description>
    </Alert.Root>
  {/if}
  {#if clearResult?.skipped_active}
    <Alert.Root variant="warning" role="status">
      <Alert.Description>{m.observation_clear_skipped_active({ count: clearResult.skipped_active })}</Alert.Description>
    </Alert.Root>
  {/if}

  <section class="observation-workspace" aria-labelledby="observation-workspace-title">
    <h2 id="observation-workspace-title" class="sr-only">{m.observation_interaction_chains()}</h2>
    <div class="workspace-toolbar">
      <Tabs.Root value={activeTab} onValueChange={(value) => void tabChanged(value)}>
        <Tabs.List
          ><Tabs.Trigger value="interactions">{m.observation_interaction_chains()}</Tabs.Trigger><Tabs.Trigger
            value="rejections">{m.observation_rejected_requests()}</Tabs.Trigger
          ></Tabs.List>
      </Tabs.Root>
      <div class="window-controls">
        <Button variant="ghost" size="sm" onclick={() => void changeWindow(1)}>{m.observation_older_window()}</Button>
        <span class="font-technical text-xs text-muted-foreground"
          >{windowStart
            ? `${formatLogTime(windowStart)} — ${windowIndex === 0 ? m.observation_live() : formatLogTime(windowEnd)}`
            : m.observation_loading_window()}</span>
        <Button variant="ghost" size="sm" disabled={windowIndex === 0} onclick={() => void changeWindow(-1)}
          >{m.observation_newer_window()}</Button>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={m.observation_refresh_anchor()}
          onclick={() => void refreshAnchor()}><RefreshCwIcon /></Button>
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
        {:else if roots.length === 0}
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
                      providerFilter = 'all'
                      modelFilter = 'all'
                      apiKeyFilter = 'all'
                      statusFilter = 'all'
                      void reloadForFilters()
                    }}>{m.observation_clear_filters()}</Button
                  ></Empty.Content
                >{/if}</Empty.Root>
          </div>
        {:else}
          <SvelteFlowProvider>
            <InteractionCanvas
              bind:this={canvas}
              {roots}
              selectedId={selectedInteraction?.id}
              {selectedPath}
              {loadingMore}
              {nextCursor}
              {rootTotal}
              latestId={latestInteraction?.id}
              {followPaused}
              newActivityAvailable={hasNewActivity}
              {fitProgress}
              onselect={(item) => void selectInteraction(item)}
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
            interaction={interactionDetail}
            loading={detailLoading}
            width={inspectorWidth}
            onwidthchange={(value) => (inspectorWidth = value)}
            onclose={closeInspector}
            onbundle={() => void downloadBundle()}
            onlatest={selectedMigrated && migratedRoots.has(selectedMigrated)
              ? () => void refreshAnchor(selectedInteraction?.id)
              : undefined} />
        {/if}
      </div>
    {:else}
      <div class="rejections-view">
        <header class="flex items-center justify-between gap-3 border-b p-4">
          <div>
            <h2 class="font-structural text-lg font-semibold">{m.observation_rejected_requests()}</h2>
            <p class="text-sm text-muted-foreground">{m.observation_rejections_description()}</p>
          </div>
          <span class="font-technical text-xs text-muted-foreground">{rejections.length} / {rejectionTotal}</span>
        </header>
        {#if rejectionLoading && rejections.length === 0}<div class="stage-state">
            {m.observation_loading_rejections()}
          </div>
        {:else if rejections.length === 0}<Empty.Root class="py-16"
            ><Empty.Header
              ><Empty.Title>{m.observation_no_rejections()}</Empty.Title><Empty.Description
                >{m.observation_no_rejections_description()}</Empty.Description
              ></Empty.Header
            ></Empty.Root>
        {:else}<ol class="rejection-list">
            {#each rejections as rejection (rejection.id)}<li>
                <button
                  class={selectedRejection?.id === rejection.id ? 'selected' : undefined}
                  onclick={() => void selectRejection(rejection)}
                  ><span class="font-technical text-xs text-muted-foreground"
                    >{formatLogTime(rejection.occurred_at)}</span
                  ><span class="min-w-0 flex-1"
                    ><strong>{rejection.method} {rejection.path}</strong><small
                      >{rejection.stage} · {rejection.code} · HTTP {rejection.status_code}</small
                    ></span
                  ><Badge variant={rejection.debug_status === 'partial' ? 'destructive' : 'outline'}
                    >{observationDebugStatusLabel(rejection.debug_status)}</Badge
                  ></button>
              </li>{/each}
          </ol>{/if}
        {#if rejectionCursor}<div class="border-t p-3 text-center">
            <Button variant="outline" disabled={rejectionLoading} onclick={() => void loadRejections(false)}
              >{rejectionLoading ? m.observation_loading_more() : m.observation_load_more()}</Button>
          </div>{/if}
        {#if selectedRejection}<ObservationInspector
            rejection={rejectionDetail}
            loading={detailLoading}
            width={inspectorWidth}
            onwidthchange={(value) => (inspectorWidth = value)}
            onclose={closeInspector}
            onbundle={() => void downloadBundle()} />{/if}
      </div>
    {/if}
  </section>
</div>

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
        <Field.Field
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
      </Field.FieldGroup>
    </div>
    <Sheet.Footer
      ><Button
        variant="ghost"
        onclick={() => {
          providerFilter = 'all'
          modelFilter = 'all'
          apiKeyFilter = 'all'
          statusFilter = 'all'
        }}>{m.observation_clear_filters()}</Button
      ><Button
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
        {m.observation_debug_warning({
          run_limit: formatBytes(debugQuery.data?.run_limit_bytes),
          total_limit: formatBytes(debugQuery.data?.total_limit_bytes),
          retention_days: debugQuery.data?.retention_days ?? 0,
        })}
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
}
.observation-workspace {
  position: relative;
  min-height: 38rem;
  overflow: hidden;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--background);
}
.workspace-toolbar {
  display: flex;
  min-height: 3.5rem;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  border-bottom: 1px solid var(--border);
  padding: 0.55rem 0.75rem;
}
.window-controls {
  display: flex;
  align-items: center;
  gap: 0.3rem;
}
.canvas-stage {
  position: relative;
  height: min(68vh, 50rem);
  min-height: 34rem;
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
.rejections-view {
  position: relative;
  min-height: 34rem;
}
.rejection-list {
  max-height: 65vh;
  overflow-y: auto;
}
.rejection-list button {
  display: flex;
  width: 100%;
  align-items: center;
  gap: 1rem;
  border-bottom: 1px solid var(--border);
  padding: 0.8rem 1rem;
  text-align: start;
}
.rejection-list button:hover,
.rejection-list button.selected {
  background: var(--muted);
}
.rejection-list strong,
.rejection-list small {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.rejection-list small {
  margin-top: 0.2rem;
  color: var(--muted-foreground);
  font-size: 0.72rem;
}
@media (max-width: 767px) {
  .workspace-toolbar {
    align-items: stretch;
    flex-direction: column;
  }
  .window-controls {
    justify-content: space-between;
    overflow-x: auto;
  }
  .window-controls > span {
    min-width: max-content;
  }
  .canvas-stage {
    height: 70svh;
    min-height: 30rem;
  }
  .observation-workspace {
    min-height: 32rem;
  }
}
</style>

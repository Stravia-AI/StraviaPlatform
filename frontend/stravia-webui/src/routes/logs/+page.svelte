<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onMount, tick } from 'svelte'
import { page } from '$app/state'
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
import { formatBytes, formatCompactCount, formatLogTime } from '$lib/format'
import { observationStatusLabel } from '$lib/observation-labels'
import { navigateToBundle, subscribeToObservations } from '$lib/observation-stream'
import {
  isValidObservationRange,
  OBSERVATION_MIN_TOKEN_STOPS,
  OBSERVATION_PRESET_MINUTES,
  ObservationWorkspace,
} from '$lib/observation-workspace.svelte'
import type { FailedRequestSummary, InteractionSummary } from '$lib/types'
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

const queryClient = useQueryClient()
let rangeOpen = $state(false)
let draftStart = $state('')
let draftEnd = $state('')
let workspaceEl = $state<HTMLElement>()
let fullscreenButton = $state<HTMLButtonElement | null>(null)
let fullscreen = $state(false)
let inspectorWidth = $state(46)
let canvas = $state<{
  focusLatest(): Promise<void>
  fitAfterAllLoaded(): Promise<void>
  focusNode(id: string): Promise<void>
}>()
let filterOpen = $state(false)
let clearOpen = $state(false)
let clearing = $state(false)
let clearResult = $state<{ skipped_active: number }>()
let debugConfirmOpen = $state(false)
let changingDebug = $state(false)
let debugClearOpen = $state(false)
let clearingDebug = $state(false)

const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const keysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const debugQuery = createQuery(() => ({ queryKey: ['observation-debug'], queryFn: admin.observations.debug }))

const ws = new ObservationWorkspace(admin.observations, subscribeToObservations, {
  focusLatest: async () => {
    await tick()
    await canvas?.focusLatest()
  },
  focusNode: async (id) => {
    await tick()
    await canvas?.focusNode(id)
  },
  fitAfterAllLoaded: async () => {
    await tick()
    await canvas?.fitAfterAllLoaded()
  },
  onError: (error) => toast.error(localizeBackendErrorMessage(error)),
})

const draftStartMs = $derived(new Date(draftStart).getTime())
const draftEndMs = $derived(new Date(draftEnd).getTime())
const validDraftRange = $derived(isValidObservationRange(draftStartMs, draftEndMs))

onMount(() => {
  void ws.start()
  const clock = setInterval(() => ws.advanceClock(), 1000)
  return () => {
    clearInterval(clock)
    ws.dispose()
    if (fullscreen && document.fullscreenElement === workspaceEl) {
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

$effect(() => {
  const id = page.url.searchParams.get('interaction')
  if (!id) return
  void ws.deepLinkInteraction(id)
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
  draftStart = localDateTime(ws.windowStart)
  draftEnd = localDateTime(ws.windowEnd)
  rangeOpen = true
}

async function applyRange(): Promise<void> {
  if (!validDraftRange) return
  rangeOpen = false
  await ws.applyRange(draftStartMs, draftEndMs)
}

async function toggleFullscreen(): Promise<void> {
  try {
    if (fullscreen) {
      if (document.fullscreenElement === workspaceEl) await document.exitFullscreen()
      fullscreen = false
      fullscreenButton?.focus()
    } else if (workspaceEl) {
      if (document.fullscreenEnabled) await workspaceEl.requestFullscreen()
      fullscreen = true
    }
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  }
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
    await queryClient.invalidateQueries({ queryKey: ['observation-debug'] })
    await ws.refreshSelectedDetail()
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
    await Promise.all([ws.refreshData(), queryClient.invalidateQueries({ queryKey: ['observation-debug'] })])
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
  const target = ws.bundleTarget()
  if (!target) return
  try {
    await navigateToBundle(await admin.observations.issueBundleTicket(target.kind, target.id))
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  }
}
</script>

<svelte:head><title>{m.common_request_history()} · Stravia</title></svelte:head>

<svelte:document
  onfullscreenchange={() => {
    fullscreen = document.fullscreenElement === workspaceEl
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
    label={ws.streamConnected ? m.observation_live() : m.observation_reconnecting()}
    tone={ws.streamConnected ? 'healthy' : 'neutral'} />
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
      ><SlidersHorizontalIcon data-icon="inline-start" />{m.observation_filters()}{#if ws.activeFilterCount}<span
          >· {ws.activeFilterCount}</span
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
    bind:this={workspaceEl}
    class={['observation-workspace', { 'workspace-fullscreen': fullscreen }]}
    aria-labelledby="observation-workspace-title">
    <h2 id="observation-workspace-title" class="sr-only">{m.observation_interaction_chains()}</h2>
    <div class="workspace-toolbar">
      <Tabs.Root value={ws.activeTab} onValueChange={(value: string) => void ws.tabChanged(value)}>
        <Tabs.List
          ><Tabs.Trigger value="interactions">{m.observation_interaction_chains()}</Tabs.Trigger><Tabs.Trigger
            value="failures">{m.observation_failed_requests()}</Tabs.Trigger
          ></Tabs.List>
      </Tabs.Root>
      <div class="window-controls">
        <Select.Root
          type="single"
          value={ws.customRange ? '' : String(ws.durationMs / 60_000)}
          onValueChange={(value: string) => void ws.choosePreset(Number(value))}>
          <Select.Trigger aria-label={m.observation_time_window()} class="w-28">
            {ws.customRange ? m.observation_custom_range() : durationLabel(ws.durationMs / 60_000)}
          </Select.Trigger>
          <Select.Content portalProps={{ to: fullscreen ? workspaceEl : undefined }}>
            <Select.Group>
              {#each OBSERVATION_PRESET_MINUTES as minutes (minutes)}
                <Select.Item value={String(minutes)} label={durationLabel(minutes)}>
                  {durationLabel(minutes)}
                </Select.Item>
              {/each}
            </Select.Group>
          </Select.Content>
        </Select.Root>
        <Button variant="ghost" size="sm" disabled={ws.loading} onclick={() => void ws.changeWindow(1)}
          >{m.observation_older_window()}</Button>
        <Button
          variant="ghost"
          size="sm"
          class="range-picker"
          aria-label={m.observation_choose_range()}
          onclick={openRange}>
          <CalendarRangeIcon data-icon="inline-start" />
          <span class="range-label">
            {formatLogTime(ws.windowStart)} — {ws.liveWindow ? m.observation_live() : formatLogTime(ws.windowEnd)}
          </span>
        </Button>
        <Button
          variant="ghost"
          size="sm"
          disabled={ws.liveWindow || ws.loading}
          onclick={() => void ws.changeWindow(-1)}>{m.observation_newer_window()}</Button>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={m.observation_refresh_anchor()}
          onclick={() => void ws.refreshAnchor()}><RefreshCwIcon /></Button>
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

    {#if ws.activeTab === 'interactions'}
      <div class="canvas-stage">
        {#if ws.loading}
          <div class="stage-state"><p>{m.observation_loading_chains()}</p></div>
        {:else if ws.loadError}
          <div class="stage-state">
            <RequestFailure message={localizeBackendErrorMessage(ws.loadError)} retry={() => ws.reloadForest()} />
          </div>
        {:else if ws.canvasRoots.length === 0}
          <div class="stage-state">
            <Empty.Root
              ><Empty.Header
                ><Empty.Title
                  >{ws.activeFilterCount ? m.observation_no_matching_chains() : m.observation_no_chains()}</Empty.Title
                ><Empty.Description
                  >{ws.activeFilterCount
                    ? m.observation_clear_filters_help()
                    : m.observation_new_requests_appear()}</Empty.Description
                ></Empty.Header
              >{#if ws.activeFilterCount}<Empty.Content
                  ><Button
                    variant="outline"
                    onclick={() => {
                      ws.clearFilters()
                      void ws.applyFilters()
                    }}>{m.observation_clear_filters()}</Button>
                  ></Empty.Content
                >{/if}</Empty.Root>
          </div>
        {:else}
          <SvelteFlowProvider>
            <InteractionCanvas
              bind:this={canvas}
              roots={ws.canvasRoots}
              selectedId={ws.selectedInteraction?.id}
              selectedPath={ws.selectedPath}
              loadingMore={ws.loadingMore}
              nextCursor={ws.nextCursor}
              rootTotal={ws.rootTotal}
              latestId={ws.latestInteraction?.id}
              followPaused={ws.followPaused}
              newActivityAvailable={ws.hasNewActivity}
              fitProgress={ws.fitProgress}
              onselect={(item: InteractionSummary) => void ws.selectInteraction(item)}
              onloadmore={() => void ws.loadNextRootBatch()}
              onfitall={() => void ws.fitAll()}
              onmanualmove={() => ws.pauseFollow()}
              onfollow={() => ws.resumeFollow()} />
          </SvelteFlowProvider>
        {/if}
        {#if ws.selectedInteraction}
          <ObservationInspector
            portalTarget={fullscreen ? workspaceEl : undefined}
            interaction={ws.interactionDetail}
            liveBlocks={ws.selectedLiveBlocks}
            liveGap={ws.liveGaps.includes(ws.selectedInteraction.id)}
            liveCapacity={ws.liveCapacityGaps.includes(ws.selectedInteraction.id)}
            olderLoading={ws.olderLoading}
            onolder={ws.loadOlderEvents}
            loading={ws.detailLoading}
            width={inspectorWidth}
            onwidthchange={(value: number) => (inspectorWidth = value)}
            onclose={ws.closeInspector}
            onbundle={() => void downloadBundle()}
            onlatest={ws.selectedMigrated && ws.migratedRoots.has(ws.selectedMigrated)
              ? () => void ws.refreshAnchor(ws.selectedInteraction?.id)
              : undefined} />
        {/if}
      </div>
    {:else}
      <div class="failures-view">
        <header class="flex items-center justify-between gap-3 border-b p-4">
          <h2 class="sr-only">{m.observation_failed_requests()}</h2>
          <span class="font-technical text-xs text-muted-foreground">{ws.failures.length} / {ws.failureTotal}</span>
        </header>
        {#if ws.failureError}
          <RequestFailure
            message={localizeBackendErrorMessage(ws.failureError)}
            retry={() => ws.loadFailures()}
            retrying={ws.failureLoading} />
        {/if}
        {#if ws.failureLoading && ws.failures.length === 0}
          <div class="stage-state" role="status">{m.observation_loading_failures()}</div>
        {:else if ws.failures.length === 0 && !ws.failureError}<Empty.Root class="flex-1"
            ><Empty.Header
              ><Empty.Title>{m.observation_no_failures()}</Empty.Title><Empty.Description
                >{m.observation_no_failures_description()}</Empty.Description
              ></Empty.Header
            ></Empty.Root>
        {:else if ws.failures.length > 0}
          <FailedRequestTable
            items={ws.failures}
            loading={ws.failureLoading}
            onselect={(failure: FailedRequestSummary) => void ws.selectFailure(failure)} />
        {/if}
        {#if ws.failureCursor}<div class="border-t p-3 text-center">
            <Button variant="outline" disabled={ws.failureLoading} onclick={() => void ws.loadFailures(false)}
              >{ws.failureLoading ? m.observation_loading_more() : m.observation_load_more()}</Button>
          </div>{/if}
        {#if ws.selectedFailure}<ObservationInspector
            portalTarget={fullscreen ? workspaceEl : undefined}
            failure={ws.failureDetail}
            error={ws.failureDetailError ? localizeBackendErrorMessage(ws.failureDetailError) : undefined}
            onretry={() => ws.selectedFailure && void ws.selectFailure(ws.selectedFailure)}
            oninteraction={ws.failureDetail?.request.interaction_id
              ? () => void ws.openFailureInteraction()
              : undefined}
            loading={ws.detailLoading}
            width={inspectorWidth}
            onwidthchange={(value: number) => (inspectorWidth = value)}
            onclose={ws.closeInspector}
            onbundle={() => void downloadBundle()} />{/if}
      </div>
    {/if}
  </section>
</div>

<Dialog.Root bind:open={rangeOpen}>
  <Dialog.Content portalProps={{ to: fullscreen ? workspaceEl : undefined }}>
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
            bind:value={() => ws.providerFilter, (value: string) => ws.setProviderFilter(value)}
            ><Select.Trigger id="observation-provider" class="w-full"
              >{providersQuery.data?.find((item) => item.id === ws.providerFilter)?.name ??
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
            bind:value={() => ws.modelFilter, (value: string) => ws.setModelFilter(value)}
            ><Select.Trigger id="observation-model" class="w-full"
              >{modelsQuery.data?.find((item) => item.id === ws.modelFilter)?.display_name ??
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
            bind:value={() => ws.apiKeyFilter, (value: string) => ws.setApiKeyFilter(value)}
            ><Select.Trigger id="observation-key" class="w-full"
              >{keysQuery.data?.find((item) => item.id === ws.apiKeyFilter)?.name ??
                m.observation_all()}</Select.Trigger
            ><Select.Content
              ><Select.Group
                ><Select.Item value="all">{m.observation_all()}</Select.Item
                >{#each keysQuery.data ?? [] as key (key.id)}<Select.Item value={key.id} label={key.name}
                    >{key.name}</Select.Item
                  >{/each}</Select.Group
              ></Select.Content
            ></Select.Root
          ></Field.Field>
        {#if ws.activeTab === 'interactions'}<Field.Field
            ><Field.FieldLabel for="observation-status">{m.common_status()}</Field.FieldLabel><Select.Root
              type="single"
              bind:value={() => ws.statusFilter, (value: string) => ws.setStatusFilter(value)}
              ><Select.Trigger id="observation-status" class="w-full"
                >{ws.statusFilter === 'all'
                  ? m.observation_all()
                  : observationStatusLabel(ws.statusFilter)}</Select.Trigger
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
                bind:value={() => ws.minTokenStop, (value: number) => ws.setMinTokenStop(value)}
                min={0}
                max={OBSERVATION_MIN_TOKEN_STOPS.length - 1}
                step={1}
                class="flex-1"
                aria-label={m.observation_min_tokens()} />
              <span class="font-technical w-14 shrink-0 text-right text-sm tabular-nums" aria-live="polite"
                >{ws.minTokensFilter > 0 ? formatCompactCount(ws.minTokensFilter) : m.observation_all()}</span>
            </div>
          </Field.Field>
        {/if}
      </Field.FieldGroup>
    </div>
    <Sheet.Footer
      ><Button variant="ghost" onclick={() => ws.clearFilters()}>{m.observation_clear_filters()}</Button><Button
        onclick={() => {
          filterOpen = false
          void ws.applyFilters()
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

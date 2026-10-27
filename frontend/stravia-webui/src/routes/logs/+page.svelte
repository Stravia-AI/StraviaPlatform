<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { onMount, tick } from 'svelte'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { SvelteFlowProvider } from '@xyflow/svelte'
import MaximizeIcon from '@lucide/svelte/icons/maximize'
import MinimizeIcon from '@lucide/svelte/icons/minimize'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import { toast } from 'svelte-sonner'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatCompactCount } from '#lib/format.js'
import { HIT_HIGHLIGHT_QUERY, HIT_HIGHLIGHT_VALUE } from '#lib/credential-hit-records.js'
import { credentialRulesQuery, customCredentialRulesQuery } from '#lib/credential-protection-queries.js'
import { effectiveModelDisplayName } from '#lib/logical-model.js'
import { observationStatusLabel } from '#lib/observation-labels.js'
import { observationOutputPreview } from '#lib/observation-conversation.js'
import { navigateToBundle, subscribeToInteractionLive, subscribeToObservations } from '#lib/observation-stream.js'
import {
  isValidObservationRange,
  OBSERVATION_MIN_TOKEN_STOPS,
  OBSERVATION_PRESET_MINUTES,
  ObservationWorkspace,
} from '#lib/observation-workspace.svelte.js'
import type { FailedRequestSummary, InteractionSummary } from '#lib/types.js'
import InteractionCanvas from '#lib/components/interaction-canvas.svelte'
import ObservationInspector from '#lib/components/observation-inspector.svelte'
import FailedRequestTable from '#lib/components/failed-request-table.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import TimeWindowControls from '#lib/components/time-window-controls.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Slider } from '#lib/components/ui/slider/index.js'
import * as Tabs from '#lib/components/ui/tabs/index.js'

const queryClient = useQueryClient()
let rangeOpen = $state(false)
let draftStart = $state('')
let draftEnd = $state('')
let workspaceEl = $state<HTMLElement>()
let fullscreenButton = $state<HTMLButtonElement | null>(null)
let fullscreen = $state(false)
// 检查器默认宽度（rem）；按所在工作区钳制，不随 viewport 变化。
let inspectorWidth = $state(40)
let workspaceWidth = $state(0)
let canvas = $state<{
  focusLatest(): Promise<void>
  fitAfterAllLoaded(): Promise<void>
  focusNode(id: string): Promise<void>
}>()
let filterOpen = $state(false)
let clearOpen = $state(false)
let clearing = $state(false)
let clearResult = $state<{ skipped_active: number }>()

const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const keysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))

const ws = new ObservationWorkspace(
  admin.observations,
  subscribeToObservations,
  {
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
  },
  subscribeToInteractionLive,
)

const selectedOutputPreview = $derived(
  ws.interactionDetail
    ? observationOutputPreview(ws.interactionDetail, ws.selectedLiveBlocks)
    : ws.selectedInteraction
      ? { text: ws.selectedInteraction.visible_tail, start: 0 }
      : undefined,
)
const selectedFilterModel = $derived(modelsQuery.data?.find((item) => item.id === ws.modelFilter))
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

// 派生值按值去重：只改 highlight 参数的导航不会重新打开 Interaction。
const deepLinkId = $derived(page.url.searchParams.get('interaction'))
$effect(() => {
  if (deepLinkId) void ws.deepLinkInteraction(deepLinkId)
})

// 仅从命中记录进入（highlight 参数）时高亮；离开该 Interaction 或点「清除高亮」即清除参数。
const highlightId = $derived(page.url.searchParams.get(HIT_HIGHLIGHT_QUERY) === HIT_HIGHLIGHT_VALUE ? deepLinkId : null)
const hitRulesQuery = createQuery(() => credentialRulesQuery(highlightId !== null))
const hitCustomRulesQuery = createQuery(() => customCredentialRulesQuery(highlightId !== null))
const hitRuleNames = $derived(
  new Map(
    [...(hitCustomRulesQuery.data ?? []), ...(hitRulesQuery.data?.rules ?? [])].map((rule) => [rule.id, rule.name]),
  ),
)

function clearHighlight(): void {
  if (!highlightId) return
  void goto(resolve(`logs?interaction=${encodeURIComponent(highlightId)}`), { replace: true, reset: false })
}
const credentialHits = $derived(
  highlightId !== null && ws.selectedInteraction?.id === highlightId
    ? { ruleNames: hitRuleNames, onclear: clearHighlight }
    : undefined,
)

function durationLabel(minutes: number): string {
  if (minutes < 60) return m.observation_window_minutes({ count: minutes })
  return minutes === 60 ? m.observation_window_hour() : m.observation_window_hours({ count: minutes / 60 })
}

const windowPresets = $derived(
  OBSERVATION_PRESET_MINUTES.map((minutes) => ({ value: String(minutes), label: durationLabel(minutes) })),
)
const windowSelectedLabel = $derived(
  ws.customRange ? m.observation_custom_range() : durationLabel(ws.durationMs / 60_000),
)
const windowPresetValue = $derived(ws.customRange ? '' : String(ws.durationMs / 60_000))

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

{#snippet windowTrailing()}
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
{/snippet}

{#snippet headerActions()}
  <div class="flex flex-wrap items-center gap-2">
    <Button variant="outline" onclick={() => (filterOpen = true)}
      ><SlidersHorizontalIcon data-icon="inline-start" />{m.observation_filters()}{#if ws.activeFilterCount}<span
          >· {ws.activeFilterCount}</span
        >{/if}</Button>
    <DropdownMenu.Root>
      <DropdownMenu.Trigger>
        {#snippet child({ props })}
          <Button {...props} variant="outline" size="icon" aria-label={m.common_more_actions()}
            ><MoreHorizontalIcon /></Button>
        {/snippet}
      </DropdownMenu.Trigger>
      <DropdownMenu.Content class="w-48" align="end">
        <DropdownMenu.Item variant="destructive" onSelect={() => (clearOpen = true)}
          >{m.observation_clear_history()}</DropdownMenu.Item>
      </DropdownMenu.Content>
    </DropdownMenu.Root>
  </div>
{/snippet}

<div class="route-page observation-page">
  <PageHeader title={m.common_request_history()} meta={liveMeta} actions={headerActions} />

  {#if clearResult?.skipped_active}
    <Alert.Root variant="warning" role="status">
      <Alert.Description>{m.observation_clear_skipped_active({ count: clearResult.skipped_active })}</Alert.Description>
    </Alert.Root>
  {/if}

  <section
    bind:this={workspaceEl}
    bind:clientWidth={workspaceWidth}
    class={['observation-workspace', { 'workspace-fullscreen': fullscreen }]}
    aria-labelledby="observation-workspace-title">
    <h2 id="observation-workspace-title" class="sr-only">{m.observation_interaction_chains()}</h2>
    <div class="workspace-toolbar">
      <div class="toolbar-tabs">
        <Tabs.Root value={ws.activeTab} onValueChange={(value: string) => void ws.tabChanged(value)}>
          <Tabs.List
            ><Tabs.Trigger value="interactions">{m.observation_interaction_chains()}</Tabs.Trigger><Tabs.Trigger
              value="failures">{m.observation_failed_requests()}</Tabs.Trigger
            ></Tabs.List>
        </Tabs.Root>
        {#if ws.activeTab === 'failures'}
          <span class="font-technical text-xs tabular-nums whitespace-nowrap text-muted-foreground" role="status"
            >{ws.failures.length} / {ws.failureTotal}</span>
        {/if}
      </div>
      <TimeWindowControls
        presets={windowPresets}
        value={windowPresetValue}
        selectedLabel={windowSelectedLabel}
        startMs={ws.windowStart}
        endMs={ws.windowEnd}
        live={ws.liveWindow}
        selectLabel={m.observation_time_window()}
        rangeLabel={m.observation_choose_range()}
        olderLabel={m.observation_older_window()}
        newerLabel={m.observation_newer_window()}
        olderDisabled={ws.loading}
        newerDisabled={ws.liveWindow || ws.loading}
        portalTarget={fullscreen ? workspaceEl : undefined}
        onpreset={(value: string) => void ws.choosePreset(Number(value))}
        onolder={() => void ws.changeWindow(1)}
        onnewer={() => void ws.changeWindow(-1)}
        onrange={openRange}
        trailing={windowTrailing} />
    </div>

    {#if ws.activeTab === 'interactions'}
      <div class="canvas-stage">
        {#if ws.loadError && ws.canvasRoots.length > 0}
          <div class="canvas-refresh-error">
            <RequestFailure message={localizeBackendErrorMessage(ws.loadError)} retry={() => ws.reloadForest()} />
          </div>
        {/if}
        {#if ws.loading && ws.canvasRoots.length === 0}
          <div class="stage-state"><p>{m.observation_loading_chains()}</p></div>
        {:else if ws.loadError && ws.canvasRoots.length === 0}
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
                    }}>{m.observation_clear_filters()}</Button
                  ></Empty.Content
                >{/if}</Empty.Root>
          </div>
        {:else}
          <SvelteFlowProvider>
            <InteractionCanvas
              bind:this={canvas}
              roots={ws.canvasRoots}
              selectedId={ws.selectedInteraction?.id}
              {selectedOutputPreview}
              selectedOutputActive={ws.selectedLiveActive}
              liveContentEpoch={ws.liveContentEpoch + ws.liveTerminalEpoch}
              selectedPath={ws.selectedPath}
              loadingMore={ws.loadingMore}
              nextCursor={ws.nextCursor}
              latestId={ws.latestInteraction?.id}
              followPaused={ws.followPaused}
              newActivityAvailable={ws.hasNewActivity}
              fitProgress={ws.fitProgress}
              onselect={(item: InteractionSummary) => {
                if (item.id !== highlightId) clearHighlight()
                void ws.selectInteraction(item)
              }}
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
            liveActive={ws.selectedLiveActive}
            liveContentEpoch={ws.liveContentEpoch + ws.liveTerminalEpoch}
            error={ws.detailError ? localizeBackendErrorMessage(ws.detailError) : undefined}
            onretry={() => void ws.refreshSelectedDetail()}
            liveGap={ws.liveGaps.includes(ws.selectedInteraction.id)}
            liveCapacity={ws.liveCapacityGaps.includes(ws.selectedInteraction.id)}
            olderLoading={ws.olderLoading}
            onolder={ws.loadOlderEvents}
            {credentialHits}
            loading={ws.detailLoading}
            width={inspectorWidth}
            containerWidth={workspaceWidth}
            onwidthchange={(value: number) => (inspectorWidth = value)}
            onclose={() => {
              clearHighlight()
              ws.closeInspector()
            }}
            onbundle={() => void downloadBundle()}
            onlatest={ws.selectedMigrated && ws.migratedRoots.has(ws.selectedMigrated)
              ? () => void ws.refreshAnchor(ws.selectedInteraction?.id)
              : undefined} />
        {/if}
      </div>
    {:else}
      <div class="failures-view">
        <h2 class="sr-only">{m.observation_failed_requests()}</h2>
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
            onretry={() => void ws.refreshSelectedDetail()}
            oninteraction={ws.failureDetail?.request.interaction_id
              ? () => void ws.openFailureInteraction()
              : undefined}
            loading={ws.detailLoading}
            width={inspectorWidth}
            containerWidth={workspaceWidth}
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
      <div class="flex items-center gap-1.5">
        <Dialog.Title>{m.observation_date_time_range()}</Dialog.Title>
        <Field.Help hint={m.observation_range_local_time()} />
      </div>
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
            aria-describedby={validDraftRange ? undefined : 'observation-range-help'} />
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
            aria-describedby={validDraftRange ? undefined : 'observation-range-help'} />
        </Field.Field>
      </Field.FieldGroup>
      {#if !validDraftRange}
        <p id="observation-range-help" class="text-sm text-destructive" role="alert">
          {m.observation_range_invalid()}
        </p>
      {/if}
      <Dialog.Footer>
        <Button variant="outline" onclick={() => (rangeOpen = false)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={!validDraftRange}>{m.observation_apply_range()}</Button>
      </Dialog.Footer>
    </form>
  </Dialog.Content>
</Dialog.Root>

<Sheet.Root bind:open={filterOpen}
  ><Sheet.Content
    side="right"
    class="w-full! max-w-none! sm:max-w-sm!"
    closeLabel={m.observation_close_filters()}
    aria-describedby={undefined}
    ><Sheet.Header
      ><div class="flex items-center gap-1.5">
        <Sheet.Title>{m.observation_filters()}</Sheet.Title>
        <Field.Help hint={m.observation_filters_description()} />
      </div></Sheet.Header>
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
              >{selectedFilterModel
                ? effectiveModelDisplayName(selectedFilterModel)
                : m.observation_all()}</Select.Trigger
            ><Select.Content
              ><Select.Group
                ><Select.Item value="all">{m.observation_all()}</Select.Item
                >{#each modelsQuery.data ?? [] as model (model.id)}<Select.Item
                    value={model.id}
                    label={effectiveModelDisplayName(model)}>{effectiveModelDisplayName(model)}</Select.Item
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
            <div class="flex items-center gap-3">
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
.toolbar-tabs {
  display: flex;
  min-width: 0;
  align-items: center;
  gap: 0.75rem;
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
.canvas-refresh-error {
  position: absolute;
  z-index: 10;
  top: 0;
  right: 0;
  left: 0;
}
.stage-state {
  display: grid;
  height: 100%;
  place-items: center;
  gap: 0.75rem;
  padding: 2rem;
  text-align: center;
  color: var(--muted-foreground);
  /* Windows Chromium 会在非滚动容器上残留 scrollbar-button 残影；此处无滚动语义，显式关闭 */
  scrollbar-width: none;
}
.failures-view {
  position: relative;
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
  overflow: hidden;
}
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
}
</style>

<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import CirclePlusIcon from '@lucide/svelte/icons/circle-plus'
import GripVerticalIcon from '@lucide/svelte/icons/grip-vertical'
import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { modelIdFromCatalogId } from '$lib/catalog-model-id'
import { formatNumber } from '$lib/format'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import type {
  Provider,
  ProviderModelSummary,
  Route,
  RouteSelectionStrategy,
  TargetThinkingControl,
  ThinkingLevel,
  ThinkingLevelMapping,
} from '$lib/types'
import {
  addRouteTarget,
  buildRouteTargets,
  createRouteTargetForms,
  moveRouteTargetToDock,
  moveRouteTargetToEdge,
  moveRouteTargetToLane,
  priorityLanes,
  removeRouteTarget,
  reorderRouteTargetBefore,
  type RouteTargetForm,
  type RouteTargetEdge,
} from './route-targets-form.js'
import ModelCombobox from '$lib/components/model-combobox.svelte'
import ModelIdCombobox from '$lib/components/model-id-combobox.svelte'
import ModelDetailsDialog from '$lib/components/model-details-dialog.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import * as Tooltip from '$lib/components/ui/tooltip'

interface Props {
  model?: Route
  providers: Provider[]
  initialProviderId?: string
  initialModelId?: string
  onSaved?: () => void
}

const thinkingLevels: ThinkingLevel[] = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']

let { model, providers, initialProviderId = '', initialModelId = '', onSaved }: Props = $props()
const initialModel = untrack(() => model)
const queryClient = useQueryClient()
let form = $state({
  modelId: initialModel?.model_id ?? '',
  displayName: initialModel?.display_name ?? '',
  balance: initialModel?.balance ?? 'traffic_equalization',
  enabled: initialModel?.is_enabled ?? true,
})
let targets = $state<RouteTargetForm[]>(
  untrack(() => createRouteTargetForms(initialModel, initialProviderId, initialModelId)),
)
let saving = $state(false)
let initialized = $state(false)
let regenerateOpen = $state(false)
let regenerateTarget = $state<RouteTargetForm>()
let targetEditorOpen = $state(false)
let targetEditorTarget = $state<RouteTargetForm>()
let targetEditorIsNew = $state(false)
let targetEditorClosing = false
let draggedTargetKey = $state('')

const availableProviders = $derived(providers)
const targetLanes = $derived(priorityLanes(targets))
const disabledTargets = $derived(targets.filter((target) => !target.enabled))
const canonicalModelsQuery = createQuery(() => ({
  queryKey: ['catalog', 'canonical-models'],
  queryFn: () => admin.catalog.canonicalModels(),
}))
const canonicalModels = $derived(canonicalModelsQuery.data?.models ?? [])

function strategyLabel(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'traffic_equalization':
      return m.model_editor_traffic_equalization()
    case 'latency_preference':
      return m.model_editor_latency_preference()
  }
}

function strategyHelp(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'traffic_equalization':
      return m.model_editor_traffic_equalization_help()
    case 'latency_preference':
      return m.model_editor_latency_preference_help()
  }
}

function strategySummary(strategy: RouteSelectionStrategy): string {
  return strategyLabel(strategy)
}

$effect(() => {
  if (!initialized && providers.length > 0) {
    initialized = true
    for (const target of targets) {
      if (target.providerId) void loadInventory(target)
    }
  }
})

function modelCandidates(target: RouteTargetForm): ProviderModelSummary[] {
  return target.inventory.filter((item) => {
    const keepExisting = target.persisted && item.id === target.model
    return item.available || keepExisting
  })
}

function selectedSummary(target: RouteTargetForm): ProviderModelSummary | undefined {
  return target.inventory.find((item) => item.id === target.model)
}

async function loadInventory(target: RouteTargetForm): Promise<void> {
  if (!target.providerId) return
  target.loading = true
  target.validationError = ''
  try {
    target.inventory = (await admin.providers.models(target.providerId)).models
    if (target.model) {
      const summary = selectedSummary(target)
      target.custom = !summary
      if (summary) await loadCapabilities(target, !target.persisted)
    }
  } catch (error) {
    target.inventory = []
    target.validationError = localizeBackendErrorMessage(error)
  } finally {
    target.loading = false
  }
}

async function changeProvider(target: RouteTargetForm, providerId: string): Promise<void> {
  if (target.providerId === providerId) return
  target.providerId = providerId
  target.model = ''
  target.inventory = []
  target.capabilities = undefined
  target.custom = false
  target.persisted = false
  target.validationError = ''
  target.thinkingLevelMap = []
  await loadInventory(target)
}

async function selectModel(target: RouteTargetForm, modelId: string): Promise<void> {
  target.model = modelId
  target.custom = false
  target.validationError = ''
  target.capabilities = undefined
  target.thinkingLevelMap = []
  await loadCapabilities(target, true)
}

async function loadCapabilities(target: RouteTargetForm, refreshThinkingMap = false): Promise<void> {
  if (!target.providerId || !target.model) return
  target.loading = true
  try {
    const [capabilities, detail] = await Promise.all([
      admin.providers.capabilities(target.providerId, target.model),
      refreshThinkingMap ? admin.providers.model(target.providerId, target.model) : undefined,
    ])
    target.capabilities = capabilities
    if (detail) {
      target.thinkingLevelMap = detail.thinking_level_map?.map((row) => ({ ...row, control: { ...row.control } })) ?? []
    }
  } catch (error) {
    target.capabilities = undefined
    target.validationError = m.model_editor_model_details_load_failed({ error: localizeBackendErrorMessage(error) })
  } finally {
    target.loading = false
  }
}

function useInventory(target: RouteTargetForm): void {
  target.custom = false
  target.model = ''
  target.capabilities = undefined
  target.validationError = ''
}

function addTarget(): void {
  const target = addRouteTarget(targets)
  editTarget(target, true)
}

function cloneTarget(target: RouteTargetForm): RouteTargetForm {
  return {
    ...target,
    inventory: [...target.inventory],
    capabilities: target.capabilities ? { ...target.capabilities } : undefined,
    thinkingLevelMap: target.thinkingLevelMap.map((row) => ({ ...row, control: { ...row.control } })),
  }
}

function editTarget(target: RouteTargetForm, isNew = false): void {
  targetEditorClosing = false
  targetEditorTarget = cloneTarget(target)
  targetEditorIsNew = isNew
  targetEditorOpen = true
  if (targetEditorTarget.providerId && targetEditorTarget.inventory.length === 0) void loadInventory(targetEditorTarget)
}

function closeTargetEditor(save: boolean): void {
  targetEditorClosing = true
  const target = targetEditorTarget
  if (target) {
    const index = targets.findIndex((candidate) => candidate.key === target.key)
    if (save && index >= 0) {
      Object.assign(targets[index], cloneTarget(target))
    } else if (!save && targetEditorIsNew && index >= 0) {
      removeRouteTarget(targets, index)
    }
  }
  targetEditorTarget = undefined
  targetEditorIsNew = false
  targetEditorOpen = false
}

function deleteEditedTarget(): void {
  const target = targetEditorTarget
  if (!target || target.enabled) return
  targetEditorClosing = true
  const index = targets.findIndex((candidate) => candidate.key === target.key)
  if (index >= 0) removeRouteTarget(targets, index)
  targetEditorTarget = undefined
  targetEditorIsNew = false
  targetEditorOpen = false
}

function removeDisabledTarget(target: RouteTargetForm): void {
  if (target.enabled) return
  const index = targets.findIndex((candidate) => candidate.key === target.key)
  if (index >= 0) removeRouteTarget(targets, index)
}

function targetIndex(target: RouteTargetForm): number {
  return targets.findIndex((candidate) => candidate.key === target.key)
}

function targetConfigured(target: RouteTargetForm): boolean {
  return Boolean(target.providerId && target.model.trim())
}

function startTargetDrag(event: DragEvent, target: RouteTargetForm): void {
  draggedTargetKey = target.key
  event.dataTransfer?.setData('text/plain', target.key)
  if (event.dataTransfer) event.dataTransfer.effectAllowed = 'move'
}

function draggedKey(event: DragEvent): string {
  return draggedTargetKey || event.dataTransfer?.getData('text/plain') || ''
}

function dropOnLane(event: DragEvent, priority: number, beforeKey?: string): void {
  event.preventDefault()
  event.stopPropagation()
  const key = draggedKey(event)
  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return
  if (beforeKey && target.enabled && target.priority === priority) {
    reorderRouteTargetBefore(targets, key, beforeKey)
    return
  }
  if (!moveRouteTargetToLane(targets, key, priority)) toast.error(m.model_editor_complete_target_before_enabling())
}

function dropOnEdge(event: DragEvent, edge: RouteTargetEdge): void {
  event.preventDefault()
  const key = draggedKey(event)
  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return
  if (!targetConfigured(target)) {
    toast.error(m.model_editor_complete_target_before_enabling())
    return
  }
  if (!moveRouteTargetToEdge(targets, key, edge)) toast.error(m.model_editor_cannot_create_priority_layer())
}

function dropInDock(event: DragEvent): void {
  event.preventDefault()
  const key = draggedKey(event)
  if (!moveRouteTargetToDock(targets, key)) toast.error(m.model_editor_last_enabled_target_required())
}

function targetSupportsThinkingLevel(target: RouteTargetForm, level: ThinkingLevel): boolean {
  return target.thinkingLevelMap.some((row) => row.level === level && row.control.type !== 'hidden')
}

function targetLabel(target: RouteTargetForm, index: number): string {
  const destination = m.model_editor_destination_value({ index: index + 1 })
  const provider = providers.find((candidate) => candidate.id === target.providerId)
  return [destination, provider?.name ?? target.providerId, target.model.trim()].filter(Boolean).join(' · ')
}

function thinkingLevelBlockers(level: ThinkingLevel): string[] {
  return targets.flatMap((target, index) =>
    !target.enabled ? [] : targetSupportsThinkingLevel(target, level) ? [] : [targetLabel(target, index)],
  )
}

function changeThinkingControlKind(row: ThinkingLevelMapping, type: TargetThinkingControl['type']): void {
  row.control =
    type === 'effort'
      ? { type, value: row.level === 'off' ? 'none' : row.level }
      : type === 'budget'
        ? { type, value: 1024 }
        : { type }
  row.source = 'overridden'
}

function changeThinkingControlValue(row: ThinkingLevelMapping, value: string | number): void {
  if (row.control.type === 'effort') row.control.value = String(value)
  if (row.control.type === 'budget') row.control.value = Math.max(0, Math.trunc(Number(value) || 0))
  row.source = 'overridden'
}

function thinkingControlLabel(type: TargetThinkingControl['type']): string {
  return {
    effort: m.model_editor_thinking_effort(),
    budget: m.model_editor_thinking_budget(),
    enabled: m.model_editor_thinking_enabled(),
    disabled: m.model_editor_thinking_disabled(),
    hidden: m.model_editor_thinking_hidden(),
  }[type]
}

async function resetThinkingRow(target: RouteTargetForm, level: ThinkingLevel): Promise<void> {
  if (!initialModel || !target.id) return
  target.validationError = ''
  try {
    const updated = await admin.models.resetThinkingMapping(initialModel.model_id, target.id, level)
    target.thinkingLevelMap =
      updated.targets.find((candidate) => candidate.id === target.id)?.thinking_level_map ?? target.thinkingLevelMap
  } catch (error) {
    target.validationError = localizeBackendErrorMessage(error)
  }
}

function requestThinkingMapRegeneration(target: RouteTargetForm): void {
  regenerateTarget = target
  regenerateOpen = true
}

async function regenerateThinkingMap(): Promise<void> {
  const target = regenerateTarget
  if (!initialModel || !target?.id) return

  regenerateOpen = false
  target.validationError = ''
  try {
    const updated = await admin.models.regenerateThinkingMap(initialModel.model_id, target.id)
    target.thinkingLevelMap =
      updated.targets.find((candidate) => candidate.id === target.id)?.thinking_level_map ?? target.thinkingLevelMap
  } catch (error) {
    target.validationError = localizeBackendErrorMessage(error)
  } finally {
    regenerateTarget = undefined
  }
}

async function saveModel(): Promise<void> {
  const result = buildRouteTargets(targets)
  if (result.error && result.error !== 'incomplete-target' && result.error !== 'no-enabled-target') {
    toast.error(m.model_editor_invalid_target_controls())
    return
  }
  const cleanTargets = result.targets
  const firstTarget = cleanTargets.find((target) => target.enabled)
  if (
    !form.modelId.trim() ||
    !firstTarget ||
    result.error === 'incomplete-target' ||
    result.error === 'no-enabled-target'
  ) {
    toast.error(m.model_editor_enabled_destination_required())
    return
  }

  saving = true
  try {
    for (const target of targets) {
      const modelId = target.model.trim()
      const needsSnapshot =
        target.custom && !target.persisted && !target.inventory.some((providerModel) => providerModel.id === modelId)
      if (needsSnapshot) {
        await admin.providers.createManualModel(
          target.providerId,
          modelId,
          JSON.stringify({ id: modelId, name: modelId }),
        )
      }
    }

    const input = {
      model_id: form.modelId.trim(),
      display_name: form.displayName.trim(),
      balance: form.balance,
      target_provider: firstTarget.provider_id,
      target_model: firstTarget.model,
      targets: cleanTargets,
    }
    if (initialModel) {
      await admin.models.update(initialModel.model_id, { ...input, is_enabled: form.enabled })
    } else {
      const created = await admin.models.create(input)
      if (!form.enabled) {
        try {
          await admin.models.update(created.model_id, { is_enabled: false })
        } catch {
          await queryClient.invalidateQueries({ queryKey: ['models'] })
          toast.error(m.model_editor_model_was_added_but_not_disabled_review_status())
          await goto(resolve('/models/[id]', { id: created.model_id }))
          return
        }
      }
    }
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['models'] }),
      queryClient.invalidateQueries({ queryKey: ['api-keys'] }),
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
    ])
    toast.success(m.model_editor_model_saved())
    onSaved?.()
    if (!onSaved) await goto(resolve('/models'))
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}
</script>

{#snippet targetCapabilityBadges(target: RouteTargetForm, summary: ProviderModelSummary | undefined)}
  {#if target.capabilities}
    <Badge variant="outline">
      {formatNumber(target.capabilities.context_window)}
      {m.common_context()}
    </Badge>
  {/if}
  {#if target.capabilities?.reasoning}<Badge variant="outline">{m.common_reasoning()}</Badge>{/if}
  {#if target.capabilities?.tool_call}<Badge variant="outline">{m.common_tool_calls()}</Badge>{/if}
  {#each target.capabilities?.input_modalities ?? [] as modality (modality)}
    <Badge variant="outline">{m.model_editor_modality_input({ modality })}</Badge>
  {/each}
  {#if summary?.capabilities.attachment}<Badge variant="outline">{m.common_attachments()}</Badge>{/if}
{/snippet}

<div class="route-page mx-auto min-h-[calc(100svh-5rem)] w-full max-w-[90rem]">
  <PageHeader
    eyebrow={m.common_setup()}
    title={initialModel ? m.model_editor_edit_model() : m.common_add_model()}
    description={m.model_editor_choose_model_name_apps_use_where_stravia_send()} />

  <form
    class="flex flex-1 flex-col gap-6"
    onsubmit={(event) => {
      event.preventDefault()
      void saveModel()
    }}>
    <section class="route-section" aria-labelledby="route-contract-title">
      <div class="route-section-header max-sm:flex-col">
        <div class="min-w-0 flex-1">
          <h2 id="route-contract-title" class="route-section-title">{m.model_editor_client_model()}</h2>
          <p class="route-section-description">
            {m.model_editor_set_model_name_request_type_client_apps_use()}
          </p>
        </div>
        <div data-slot="model-enabled-control" class="flex min-h-10 shrink-0 items-center gap-2">
          <Field.Label for="route-enabled">{m.common_enable_action()}</Field.Label>
          <Switch
            id="route-enabled"
            aria-label={m.common_enable_action()}
            bind:checked={() => form.enabled, (checked) => (form.enabled = checked)} />
        </div>
      </div>
      <div class="grid gap-4 md:grid-cols-4">
        <Field.Field orientation="vertical" class="md:col-span-2">
          <Field.Label for="route-model-id">{m.model_editor_model_id()}</Field.Label>
          <ModelIdCombobox
            id="route-model-id"
            value={form.modelId}
            models={canonicalModels}
            placeholder={m.model_editor_model_id_placeholder()}
            emptyText={m.model_editor_no_models_found()}
            ariaLabel={m.model_editor_model_id()}
            clearAriaLabel={m.model_editor_clear_selected_model()}
            onInput={(value) => {
              form.modelId = value
            }}
            onSelect={(model) => {
              form.modelId = modelIdFromCatalogId(model.id)
              form.displayName = model.name
            }}
            onClear={() => {
              form.modelId = ''
              form.displayName = ''
            }} />
        </Field.Field>
        <Field.Field orientation="vertical">
          <Field.Label for="route-display-name">{m.model_editor_model_name()}</Field.Label>
          <Input
            id="route-display-name"
            bind:value={form.displayName}
            placeholder={m.model_editor_model_name_placeholder()} />
        </Field.Field>
        <Field.Field orientation="vertical">
          <Field.Label for="route-balance">{m.model_editor_how_requests_sent()}</Field.Label>
          <Select.Root type="single" bind:value={form.balance}>
            <Select.Trigger id="route-balance" class="w-full" aria-label={m.model_editor_how_requests_sent()}>
              {strategyLabel(form.balance)}
            </Select.Trigger>
            <Select.Content>
              <Select.Item value="traffic_equalization">{m.model_editor_traffic_equalization()}</Select.Item>
              <Select.Item value="latency_preference">{m.model_editor_latency_preference()}</Select.Item>
            </Select.Content>
          </Select.Root>
        </Field.Field>
      </div>
    </section>

    <section class="route-section" aria-labelledby="route-targets-title">
      <div class="route-section-header">
        <div>
          <h2 id="route-targets-title" class="route-section-title">{m.model_editor_request_destinations()}</h2>
          <p class="route-section-description">{strategyHelp(form.balance)}</p>
        </div>
        <Button type="button" variant="outline" onclick={addTarget}
          ><CirclePlusIcon data-icon="inline-start" />{m.model_editor_add_destination()}</Button>
      </div>

      <div class="grid gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(16rem,1fr)]">
        <div
          data-slot="target-lane-stack"
          class="min-w-0 rounded-2xl border bg-muted/20 p-3"
          aria-label={m.model_editor_enabled_targets()}>
          <div class="mb-3 flex items-center justify-between gap-3 px-1">
            <div>
              <h3 class="font-medium">{m.model_editor_enabled_targets()}</h3>
              <p class="text-sm text-muted-foreground">{m.model_editor_priority_lanes_help()}</p>
            </div>
            <Badge variant="outline">{targets.filter((target) => target.enabled).length}</Badge>
          </div>

          <div
            class="flex min-h-12 items-center justify-center rounded-xl border border-dashed px-3 text-sm text-muted-foreground transition-colors hover:border-primary/50 hover:text-foreground"
            role="button"
            tabindex="0"
            ondragover={(event) => event.preventDefault()}
            ondrop={(event) => dropOnEdge(event, 'top')}>
            {m.model_editor_add_higher_layer()}
          </div>

          <div class="my-2 flex flex-col gap-2">
            {#each targetLanes as lane, laneIndex (lane.priority)}
              <section
                class="grid min-h-28 grid-cols-[4.5rem_minmax(0,1fr)] overflow-hidden rounded-xl border bg-background shadow-xs"
                aria-label={m.model_editor_layer_value({ index: laneIndex + 1 })}
                ondragover={(event) => event.preventDefault()}
                ondrop={(event) => dropOnLane(event, lane.priority)}>
                <div class="flex flex-col items-center justify-center border-r bg-muted/50 px-2 text-center">
                  <span class="font-technical text-xs uppercase tracking-[0.16em] text-muted-foreground">
                    {m.model_editor_layer()}
                  </span>
                  <strong class="font-technical text-xl">{laneIndex + 1}</strong>
                </div>
                <div class="flex min-w-0 flex-wrap content-start gap-2 p-2">
                  {#each lane.targets as target (target.key)}
                    {@const summary = selectedSummary(target)}
                    <button
                      type="button"
                      draggable="true"
                      class="group flex min-h-24 min-w-48 flex-1 cursor-grab flex-col items-start rounded-lg border bg-card p-3 text-left shadow-xs transition-[border-color,box-shadow,transform] hover:-translate-y-0.5 hover:border-primary/40 hover:shadow-sm focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 active:cursor-grabbing"
                      aria-label={m.model_editor_edit_destination_value({ index: targetIndex(target) + 1 })}
                      ondragstart={(event) => startTargetDrag(event, target)}
                      ondragend={() => (draggedTargetKey = '')}
                      ondragover={(event) => event.preventDefault()}
                      ondrop={(event) => dropOnLane(event, lane.priority, target.key)}
                      onclick={() => editTarget(target)}>
                      <span class="flex w-full min-w-0 items-center gap-2">
                        <GripVerticalIcon class="size-4 shrink-0 text-muted-foreground" />
                        <span class="truncate font-medium">
                          {providers.find((provider) => provider.id === target.providerId)?.name ?? target.providerId}
                        </span>
                      </span>
                      <span class="mt-1 w-full truncate pl-6 font-technical text-sm text-muted-foreground">
                        {target.model}
                      </span>
                      <span class="mt-auto flex flex-wrap gap-1.5 pl-6 pt-2">
                        {#if target.persisted && summary && !summary.available}
                          <Badge variant="destructive">{m.model_editor_model_no_longer_available()}</Badge>
                        {/if}
                        {@render targetCapabilityBadges(target, summary)}
                      </span>
                    </button>
                  {/each}
                </div>
              </section>
            {:else}
              <div
                data-slot="target-lane-empty"
                class="flex min-h-28 items-center justify-center rounded-xl border border-dashed px-4 text-center text-sm text-muted-foreground"
                role="group"
                aria-label={m.model_editor_no_enabled_targets()}
                ondragover={(event) => event.preventDefault()}
                ondrop={(event) => dropOnEdge(event, 'top')}>
                {m.model_editor_no_enabled_targets()}
              </div>
            {/each}
          </div>

          <div
            class="flex min-h-12 items-center justify-center rounded-xl border border-dashed px-3 text-sm text-muted-foreground transition-colors hover:border-primary/50 hover:text-foreground"
            role="button"
            tabindex="0"
            ondragover={(event) => event.preventDefault()}
            ondrop={(event) => dropOnEdge(event, 'bottom')}>
            {m.model_editor_add_lower_layer()}
          </div>
        </div>

        <aside
          data-slot="target-dock"
          class="min-w-0 rounded-2xl border border-dashed bg-muted/30 p-3"
          aria-label={m.model_editor_disabled_targets()}
          ondragover={(event) => event.preventDefault()}
          ondrop={dropInDock}>
          <div class="mb-3 flex items-center justify-between gap-3 px-1">
            <div>
              <h3 class="font-medium">{m.model_editor_disabled_targets()}</h3>
              <p class="text-sm text-muted-foreground">{m.model_editor_disabled_targets_help()}</p>
            </div>
            <Badge variant="secondary">{disabledTargets.length}</Badge>
          </div>
          <div class="flex flex-col gap-2">
            {#each disabledTargets as target (target.key)}
              {@const summary = selectedSummary(target)}
              <div class="relative">
                <button
                  type="button"
                  draggable="true"
                  class="group flex min-h-20 w-full cursor-grab flex-col items-start rounded-lg border bg-background p-3 pr-12 text-left shadow-xs transition-[border-color,box-shadow,transform] hover:-translate-y-0.5 hover:border-primary/40 hover:shadow-sm focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 active:cursor-grabbing"
                  aria-label={m.model_editor_edit_destination_value({ index: targetIndex(target) + 1 })}
                  ondragstart={(event) => startTargetDrag(event, target)}
                  ondragend={() => (draggedTargetKey = '')}
                  onclick={() => editTarget(target)}>
                  <span class="flex w-full min-w-0 items-center gap-2">
                    <GripVerticalIcon class="size-4 shrink-0 text-muted-foreground" />
                    <span class="truncate font-medium">
                      {providers.find((provider) => provider.id === target.providerId)?.name ||
                        target.providerId ||
                        m.model_editor_unconfigured_target()}
                    </span>
                  </span>
                  <span class="mt-1 w-full truncate pl-6 font-technical text-sm text-muted-foreground">
                    {target.model || m.model_editor_choose_model()}
                  </span>
                  <span class="mt-auto flex flex-wrap gap-1.5 pl-6 pt-2">
                    {#if target.persisted && summary && !summary.available}
                      <Badge variant="destructive">{m.model_editor_model_no_longer_available()}</Badge>
                    {/if}
                    {@render targetCapabilityBadges(target, summary)}
                  </span>
                </button>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  class="absolute right-1.5 top-1.5 size-9"
                  aria-label={m.model_editor_remove_destination_value({ index: targetIndex(target) + 1 })}
                  onclick={() => removeDisabledTarget(target)}>
                  <Trash2Icon />
                </Button>
              </div>
            {:else}
              <div
                class="flex min-h-28 items-center justify-center rounded-xl border border-dashed bg-background/50 px-4 text-center text-sm text-muted-foreground">
                {m.model_editor_no_disabled_targets()}
              </div>
            {/each}
          </div>
        </aside>
      </div>

      <Dialog.Root
        bind:open={targetEditorOpen}
        onOpenChange={(open) => {
          if (open) {
            targetEditorClosing = false
          } else if (!targetEditorClosing && targetEditorTarget) {
            closeTargetEditor(false)
          }
        }}>
        <Dialog.Content class="max-h-[90svh] overflow-y-auto sm:max-w-4xl">
          {#if targetEditorTarget}
            {@const target = targetEditorTarget}
            {@const index = targetIndex(target)}
            {@const summary = selectedSummary(target)}
            <div aria-labelledby={`target-title-${target.key}`}>
              <Dialog.Header class="mb-4">
                <Dialog.Title id={`target-title-${target.key}`}>
                  {m.model_editor_edit_destination_value({ index: index + 1 })}
                </Dialog.Title>
                <Dialog.Description>{m.model_editor_target_dialog_help()}</Dialog.Description>
              </Dialog.Header>
              <div class="mb-4 flex items-center justify-between gap-3 border-b pb-4">
                <div class="flex items-center gap-2">
                  <Badge variant={target.enabled ? 'default' : 'secondary'}>
                    {target.enabled ? m.model_editor_enabled() : m.model_editor_disabled()}
                  </Badge>
                  {#if target.persisted && summary && !summary.available}<Badge variant="destructive"
                      >{m.model_editor_model_no_longer_available()}</Badge
                    >{/if}
                </div>
                <div class="flex items-center gap-1">
                  {#if !target.enabled}
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      class="size-10"
                      aria-label={m.model_editor_remove_destination_value({ index: index + 1 })}
                      onclick={deleteEditedTarget}><Trash2Icon /></Button>
                  {/if}
                </div>
              </div>

              <div class="grid gap-4 lg:grid-cols-2">
                <Field.Field size="select">
                  <Field.Label for={`target-provider-${target.key}`}>{m.common_model_service()}</Field.Label>
                  <Select.Root
                    type="single"
                    value={target.providerId}
                    onValueChange={(value) => value && void changeProvider(target, value)}>
                    <Select.Trigger
                      id={`target-provider-${target.key}`}
                      class="w-full"
                      aria-label={m.model_editor_destination_value_model_service({ index: index + 1 })}>
                      {providers.find((provider) => provider.id === target.providerId)?.name ??
                        m.model_editor_choose_model_service()}
                    </Select.Trigger>
                    <Select.Content>
                      {#each availableProviders as provider (provider.id)}<Select.Item
                          value={provider.id}
                          label={provider.name}>{provider.name}</Select.Item
                        >{/each}
                    </Select.Content>
                  </Select.Root>
                </Field.Field>

                <Field.Field size="fill">
                  <Field.Label for={`target-model-${target.key}`}>{m.common_model()}</Field.Label>
                  {#if target.custom}
                    <Input
                      id={`target-model-${target.key}`}
                      class="font-technical"
                      bind:value={target.model}
                      aria-label={m.model_editor_destination_value_custom_model_id({ index: index + 1 })}
                      placeholder="private-model" />
                    <Field.Description class="text-warning"
                      >{m.model_editor_model_not_synced_list_requests_fail_if_id()}</Field.Description>
                    <Button class="mt-2" type="button" variant="ghost" size="sm" onclick={() => useInventory(target)}
                      >{m.model_editor_choose_synced_model()}</Button>
                  {:else}
                    <ModelCombobox
                      id={`target-model-${target.key}`}
                      value={target.model}
                      models={modelCandidates(target)}
                      placeholder={m.model_editor_choose_model()}
                      searchPlaceholder={m.model_editor_search_model_id()}
                      emptyText={m.model_editor_no_models_found()}
                      ariaLabel={m.model_editor_destination_value_model({ index: index + 1 })}
                      searchAriaLabel={m.model_editor_search_models_destination_value({ index: index + 1 })}
                      disabled={!target.providerId || target.loading}
                      onSelect={(value) => void selectModel(target, value)} />
                  {/if}
                </Field.Field>
              </div>

              <div class="mt-4 grid gap-4 border-t pt-4 md:grid-cols-3">
                <Field.Field size="number">
                  <Field.Label for={`target-first-token-timeout-${target.key}`}
                    >{m.model_editor_first_token_timeout()}</Field.Label>
                  <Input
                    id={`target-first-token-timeout-${target.key}`}
                    type="number"
                    min="0"
                    step="0.001"
                    bind:value={target.firstTokenTimeoutSeconds} />
                  <Field.Description>{m.model_editor_first_token_timeout_help()}</Field.Description>
                </Field.Field>
                <Field.Field size="number">
                  <Field.Label for={`target-retry-budget-${target.key}`}
                    >{m.model_editor_target_retry_budget()}</Field.Label>
                  <Input
                    id={`target-retry-budget-${target.key}`}
                    type="number"
                    min="0"
                    step="1"
                    bind:value={target.targetRetryBudget} />
                  <Field.Description>{m.model_editor_target_retry_budget_help()}</Field.Description>
                </Field.Field>
                <Field.Field size="number">
                  <Field.Label for={`target-cooldown-${target.key}`}>{m.model_editor_target_cooldown()}</Field.Label>
                  <Input
                    id={`target-cooldown-${target.key}`}
                    type="number"
                    min="0"
                    step="0.001"
                    bind:value={target.targetCooldownSeconds} />
                  <Field.Description>{m.model_editor_target_cooldown_help()}</Field.Description>
                </Field.Field>
              </div>

              {#if target.loading}
                <div class="mt-3 flex items-center gap-2 text-sm text-muted-foreground">
                  <Spinner />{m.model_editor_loading_models_supported_features()}
                </div>
              {:else if target.capabilities || summary}
                <div
                  class="mt-4 flex flex-wrap items-center gap-2 border-t pt-3"
                  aria-label={m.model_editor_destination_value_supported_features({ index: index + 1 })}>
                  {@render targetCapabilityBadges(target, summary)}
                  {#if summary}
                    <ModelDetailsDialog
                      providerId={target.providerId}
                      modelId={target.model}
                      triggerLabel={m.model_editor_view_model_details()} />
                  {/if}
                </div>
              {/if}
              {#if target.providerId && target.model.trim() && target.thinkingLevelMap.length > 0}
                <div class="mt-4 border-t pt-4">
                  <div class="mb-3 flex flex-wrap items-start justify-between gap-3">
                    <div>
                      <h4 class="font-medium">{m.model_editor_thinking_map()}</h4>
                      <p class="text-sm text-muted-foreground">{m.model_editor_thinking_map_help()}</p>
                    </div>
                    {#if target.id}
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        onclick={() => requestThinkingMapRegeneration(target)}>
                        {m.model_editor_thinking_regenerate()}
                      </Button>
                    {/if}
                  </div>
                  <div data-slot="thinking-map" class="divide-y border-y">
                    {#each target.thinkingLevelMap as row (row.level)}
                      <div
                        data-slot="thinking-map-row"
                        class="grid grid-cols-[minmax(0,1fr)_2.5rem] items-center gap-x-3 gap-y-2 py-2 sm:grid-cols-[minmax(8rem,0.8fr)_minmax(0,2.2fr)_2.5rem]">
                        <div class="flex min-w-0 items-center gap-2">
                          <span class="font-technical text-sm">{row.level}</span>
                          {#if row.source === 'overridden'}
                            <span class="text-xs text-muted-foreground">
                              {m.model_editor_thinking_overridden()}
                            </span>
                          {/if}
                        </div>
                        <div class="col-span-2 flex min-w-0 gap-2 sm:col-span-1">
                          <Select.Root
                            type="single"
                            value={row.control.type}
                            onValueChange={(value) =>
                              value && changeThinkingControlKind(row, value as TargetThinkingControl['type'])}>
                            <Select.Trigger
                              class="w-28 shrink-0 sm:w-32"
                              aria-label={`${row.level} ${m.model_editor_thinking_control()}`}>
                              {thinkingControlLabel(row.control.type)}
                            </Select.Trigger>
                            <Select.Content>
                              {#each ['effort', 'budget', 'enabled', 'disabled', 'hidden'] as type (type)}
                                <Select.Item value={type}
                                  >{thinkingControlLabel(type as TargetThinkingControl['type'])}</Select.Item>
                              {/each}
                            </Select.Content>
                          </Select.Root>
                          {#if row.control.type === 'effort'}
                            <Input
                              class="min-w-0 flex-1"
                              value={row.control.value}
                              aria-label={`${row.level} ${m.model_editor_thinking_effort()}`}
                              oninput={(event) => changeThinkingControlValue(row, event.currentTarget.value)} />
                          {:else if row.control.type === 'budget'}
                            <Input
                              class="min-w-0 flex-1"
                              type="number"
                              min="0"
                              step="1"
                              value={row.control.value}
                              aria-label={`${row.level} ${m.model_editor_thinking_budget()}`}
                              oninput={(event) => changeThinkingControlValue(row, event.currentTarget.value)} />
                          {/if}
                        </div>
                        {#if target.id}
                          <Button
                            type="button"
                            variant="ghost"
                            size="icon"
                            class="col-start-2 row-start-1 size-10 justify-self-end sm:col-auto sm:row-auto"
                            aria-label={`${m.model_editor_thinking_reset_row()}: ${row.level}`}
                            title={m.model_editor_thinking_reset_row()}
                            onclick={() => void resetThinkingRow(target, row.level)}>
                            <RotateCcwIcon />
                          </Button>
                        {/if}
                      </div>
                    {/each}
                  </div>
                </div>
              {/if}
              {#if target.validationError}<p class="mt-3 text-sm text-warning" role="status">
                  {target.validationError}
                </p>{/if}
              <Dialog.Footer class="mt-5 border-t pt-4">
                <Button type="button" variant="outline" onclick={() => closeTargetEditor(false)}>
                  {m.common_cancel()}
                </Button>
                <Button
                  type="button"
                  disabled={target.enabled && !targetConfigured(target)}
                  onclick={() => closeTargetEditor(true)}>{m.common_confirm()}</Button>
              </Dialog.Footer>
            </div>
          {/if}
        </Dialog.Content>
      </Dialog.Root>
    </section>

    <section class="route-section" aria-labelledby="route-thinking-title">
      <div class="route-section-header">
        <div>
          <h2 id="route-thinking-title" class="route-section-title">{m.model_editor_thinking_levels()}</h2>
          <p class="route-section-description">{m.model_editor_thinking_levels_help()}</p>
        </div>
      </div>
      <div class="grid grid-cols-2 gap-1 rounded-xl bg-muted/60 p-1 sm:grid-cols-4 xl:grid-cols-7">
        {#each thinkingLevels as level (level)}
          {@const blockers = thinkingLevelBlockers(level)}
          {#if blockers.length > 0}
            <Tooltip.Root>
              <Tooltip.Trigger
                type="button"
                data-slot="route-thinking-level"
                data-level={level}
                data-supported="false"
                class="flex min-h-10 w-full cursor-help items-center justify-center rounded-lg px-3 text-muted-foreground outline-none transition-colors hover:bg-background/50 focus-visible:ring-[3px] focus-visible:ring-ring/50">
                <span class="font-technical">{level}</span>
              </Tooltip.Trigger>
              <Tooltip.Content side="top" sideOffset={8} class="max-w-96 flex-col items-start gap-1.5">
                <p class="font-medium">{m.model_editor_thinking_blocked_by()}</p>
                <ul class="w-full list-disc space-y-0.5 pl-4 text-left">
                  {#each blockers as blocker (blocker)}
                    <li>{blocker}</li>
                  {/each}
                </ul>
              </Tooltip.Content>
            </Tooltip.Root>
          {:else}
            <div
              data-slot="route-thinking-level"
              data-level={level}
              data-supported="true"
              class="flex min-h-10 items-center justify-center rounded-lg bg-background px-3 shadow-xs">
              <span class="font-technical">{level}</span>
            </div>
          {/if}
        {/each}
      </div>
    </section>

    <div
      data-slot="model-editor-footer"
      class="sticky bottom-0 z-20 mt-auto flex translate-y-2 flex-wrap items-center justify-between gap-3 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
      <p class="text-sm text-muted-foreground">
        {targets.length === 1
          ? m.common_1_destination()
          : m.model_editor_value_destinations({ target_count: targets.length })} · {strategySummary(form.balance)}
      </p>
      <div class="flex gap-2">
        <Button href="/models" variant="outline">{m.common_cancel()}</Button>
        <Button type="submit" disabled={saving}
          >{#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_model()}</Button>
      </div>
    </div>
  </form>
</div>

<AlertDialog.Root bind:open={regenerateOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.model_editor_thinking_regenerate()}</AlertDialog.Title>
      <AlertDialog.Description>{m.model_editor_thinking_regenerate_confirm()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={() => (regenerateTarget = undefined)}>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action onclick={() => void regenerateThinkingMap()}>
        {m.model_editor_thinking_regenerate()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

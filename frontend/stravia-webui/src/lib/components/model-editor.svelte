<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import ArrowUpIcon from '@lucide/svelte/icons/arrow-up'
import CirclePlusIcon from '@lucide/svelte/icons/circle-plus'
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
  moveRouteTarget,
  removeRouteTarget,
  type RouteTargetForm,
} from './route-targets-form.js'
import ModelCombobox from '$lib/components/model-combobox.svelte'
import ModelDetailsDialog from '$lib/components/model-details-dialog.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
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
  name: initialModel?.name ?? '',
  balance: initialModel?.balance ?? 'weighted',
  enabled: initialModel?.is_enabled ?? true,
})
let targets = $state<RouteTargetForm[]>(
  untrack(() => createRouteTargetForms(initialModel, initialProviderId, initialModelId)),
)
let saving = $state(false)
let initialized = $state(false)
let regenerateOpen = $state(false)
let regenerateTarget = $state<RouteTargetForm>()

const availableProviders = $derived(providers)
const canonicalModelsQuery = createQuery(() => ({
  queryKey: ['catalog', 'canonical-models'],
  queryFn: () => admin.catalog.canonicalModels(),
}))
const canonicalModels = $derived(canonicalModelsQuery.data?.models ?? [])

function strategyLabel(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'weighted':
      return m.model_editor_split_traffic()
    case 'priority':
      return m.common_try_order()
    case 'cooldown':
      return m.model_editor_rotate_destinations()
    case 'latency':
      return m.model_editor_prefer_low_latency()
  }
}

function strategyHelp(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'weighted':
      return m.model_editor_traffic_share_help()
    case 'priority':
      return m.model_editor_failover_help()
    case 'cooldown':
      return m.model_editor_cooldown_help()
    case 'latency':
      return m.model_editor_latency_help()
  }
}

function strategySummary(strategy: RouteSelectionStrategy): string {
  return strategy === 'weighted' ? m.model_editor_traffic_split_share() : strategyLabel(strategy)
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
      target.thinkingLevelMap =
        detail.thinking_level_map?.map((row) => ({
          ...row,
          control: { ...row.control },
        })) ?? []
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
  addRouteTarget(targets)
}

function removeTarget(index: number): void {
  removeRouteTarget(targets, index)
}

function moveTarget(index: number, offset: -1 | 1): void {
  moveRouteTarget(targets, index, offset)
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
    targetSupportsThinkingLevel(target, level) ? [] : [targetLabel(target, index)],
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
    const updated = await admin.models.resetThinkingMapping(initialModel.name, target.id, level)
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
    const updated = await admin.models.regenerateThinkingMap(initialModel.name, target.id)
    target.thinkingLevelMap =
      updated.targets.find((candidate) => candidate.id === target.id)?.thinking_level_map ?? target.thinkingLevelMap
  } catch (error) {
    target.validationError = localizeBackendErrorMessage(error)
  } finally {
    regenerateTarget = undefined
  }
}

async function saveModel(): Promise<void> {
  const result = buildRouteTargets(form.balance, targets)
  if (result.error === 'invalid-weight') {
    toast.error(m.model_editor_every_traffic_share_must_positive_integer())
    return
  }
  const cleanTargets = result.targets
  const firstTarget = cleanTargets[0]
  if (!form.name.trim() || !firstTarget || result.error === 'incomplete-target') {
    toast.error(m.model_editor_destinations_help())
    return
  }

  saving = true
  try {
    for (const target of targets) {
      const modelId = target.model.trim()
      const needsSnapshot =
        target.custom &&
        !target.persisted &&
        !target.inventory.some((providerModel) => providerModel.id === modelId)
      if (needsSnapshot) {
        await admin.providers.createManualModel(
          target.providerId,
          modelId,
          JSON.stringify({ id: modelId, name: modelId }),
        )
      }
    }

    const input = {
      name: form.name.trim(),
      balance: form.balance,
      target_provider: firstTarget.provider_id,
      target_model: firstTarget.model,
      targets: cleanTargets,
    }
    if (initialModel) {
      await admin.models.update(initialModel.name, { ...input, is_enabled: form.enabled })
    } else {
      const created = await admin.models.create(input)
      if (!form.enabled) {
        try {
          await admin.models.update(created.name, { is_enabled: false })
        } catch {
          await queryClient.invalidateQueries({ queryKey: ['models'] })
          toast.error(m.model_editor_model_was_added_but_not_disabled_review_status())
          await goto(resolve('/models/[id]', { id: created.name }))
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
            bind:checked={
              () => form.enabled,
              (checked) => (form.enabled = checked)
            } />
        </div>
      </div>
      <div class="grid gap-4 md:grid-cols-3">
        <Field.Field orientation="vertical" class="md:col-span-2">
          {#if initialModel}
            <Field.Label for="route-name">{m.model_editor_model_name_used_clients()}</Field.Label>
            <Input id="route-name" class="font-technical" bind:value={form.name} placeholder="gpt-5.4" required />
          {:else}
            <Field.Label>{m.model_editor_search_model()}</Field.Label>
            <ModelCombobox
              id="route-model-search"
              value={form.name}
              models={canonicalModels}
              placeholder={m.model_editor_search_model()}
              searchPlaceholder={m.model_editor_search_model()}
              emptyText={m.model_editor_no_models_found()}
              ariaLabel={m.model_editor_search_model()}
              searchAriaLabel={m.model_editor_search_model()}
              clearAriaLabel={m.model_editor_clear_selected_model()}
              disabled={canonicalModelsQuery.isPending}
              onSelect={(id) => (form.name = modelIdFromCatalogId(id))}
              onClear={() => (form.name = '')} />
          {/if}
        </Field.Field>
        <Field.Field orientation="vertical">
          <Field.Label for="route-balance">{m.model_editor_how_requests_sent()}</Field.Label>
          <Select.Root type="single" bind:value={form.balance}>
            <Select.Trigger id="route-balance" class="w-full" aria-label={m.model_editor_how_requests_sent()}>
              {strategyLabel(form.balance)}
            </Select.Trigger>
            <Select.Content>
              <Select.Item value="weighted">{m.model_editor_split_traffic()}</Select.Item>
              <Select.Item value="priority">{m.common_try_order()}</Select.Item>
              <Select.Item value="cooldown">{m.model_editor_rotate_destinations()}</Select.Item>
              <Select.Item value="latency">{m.model_editor_prefer_low_latency()}</Select.Item>
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

      <div class="flex flex-col gap-4">
        {#each targets as target, index (target.key)}
          {@const summary = selectedSummary(target)}
          <article class="rounded-xl border p-4" aria-labelledby={`target-title-${target.key}`}>
            <div class="mb-4 flex items-center justify-between gap-3">
              <div class="flex items-center gap-2">
                <h3 id={`target-title-${target.key}`} class="font-medium">
                  {m.model_editor_destination_value({ index: index + 1 })}
                </h3>
                {#if target.persisted && summary && !summary.available}<Badge variant="destructive"
                    >{m.model_editor_model_no_longer_available()}</Badge
                  >{/if}
              </div>
              <div class="flex items-center gap-1">
                {#if form.balance === 'priority'}
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    class="size-10"
                    aria-label={m.model_editor_move_destination_value_up({ index: index + 1 })}
                    disabled={index === 0}
                    onclick={() => moveTarget(index, -1)}><ArrowUpIcon /></Button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    class="size-10"
                    aria-label={m.model_editor_move_destination_value_down({ index: index + 1 })}
                    disabled={index === targets.length - 1}
                    onclick={() => moveTarget(index, 1)}><ArrowDownIcon /></Button>
                {/if}
                {#if targets.length > 1}
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    class="size-10"
                    aria-label={m.model_editor_remove_destination_value({ index: index + 1 })}
                    onclick={() => removeTarget(index)}><Trash2Icon /></Button>
                {/if}
              </div>
            </div>

            <div class="grid gap-4 lg:grid-cols-[minmax(13rem,0.7fr)_minmax(18rem,1.3fr)_minmax(9rem,0.45fr)]">
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

              {#if form.balance === 'weighted'}
                <Field.Field size="number">
                  <Field.Label for={`target-weight-${target.key}`}>{m.model_editor_traffic_share()}</Field.Label>
                  <Input
                    id={`target-weight-${target.key}`}
                    type="number"
                    min="1"
                    step="1"
                    bind:value={target.weight}
                    aria-label={m.model_editor_destination_value_traffic_share({ index: index + 1 })} />
                </Field.Field>
              {/if}
            </div>

            {#if target.loading}
              <div class="mt-3 flex items-center gap-2 text-sm text-muted-foreground">
                <Spinner />{m.model_editor_loading_models_supported_features()}
              </div>
            {:else if target.capabilities || summary}
              <div
                class="mt-4 flex flex-wrap items-center gap-2 border-t pt-3"
                aria-label={m.model_editor_destination_value_supported_features({ index: index + 1 })}>
                {#if target.capabilities}
                  <Badge variant="outline"
                    >{formatNumber(target.capabilities.context_window)} {m.common_context()}</Badge>
                  {#if target.capabilities.reasoning}<Badge variant="outline">{m.common_reasoning()}</Badge>{/if}
                  {#if target.capabilities.tool_call}<Badge variant="outline">{m.common_tool_calls()}</Badge>{/if}
                  {#each target.capabilities.input_modalities as modality (modality)}<Badge variant="outline"
                      >{m.model_editor_modality_input({ modality })}</Badge
                    >{/each}
                {/if}
                {#if summary?.capabilities.attachment}<Badge variant="outline">{m.common_attachments()}</Badge>{/if}
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
                              <Select.Item value={type}>{thinkingControlLabel(type as TargetThinkingControl['type'])}</Select.Item>
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
          </article>
        {/each}
      </div>
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

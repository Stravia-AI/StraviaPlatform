<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet, type ColumnFiltersState } from '@tanstack/svelte-table'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import PlusIcon from '@lucide/svelte/icons/plus'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import { tick } from 'svelte'
import SearchIcon from '@lucide/svelte/icons/search'
import { SvelteURLSearchParams } from 'svelte/reactivity'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { modelIdFromCatalogId } from '$lib/catalog-model-id'
import { getDataTableLabels } from '$lib/data-table-labels'
import { formatNumber, formatTime } from '$lib/format'
import { localeState } from '$lib/localization.svelte'
import type {
  Route,
  PreparedProviderModel,
  ProviderModelDetail,
  ProviderModelSelectionPolicy,
  ProviderModelSummary,
  ProviderModelSyncSummary,
} from '$lib/types'
import ModelCombobox from '$lib/components/model-combobox.svelte'
import ProviderModelEditor from '$lib/components/provider-model-editor.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableFilterGroup,
  type DataTableRowPointerEvent,
} from '$lib/components/ui/data-table'
import * as Dialog from '$lib/components/ui/dialog'
import * as DropdownMenu from '$lib/components/ui/dropdown-menu'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { Spinner } from '$lib/components/ui/spinner'

interface Props {
  providerId: string
  routes: Route[]
  routeReferencesReady: boolean
  syncedAt?: Date
  syncedSummary?: ProviderModelSyncSummary
  onSync?: () => Promise<ProviderModelSyncSummary | undefined> | ProviderModelSyncSummary | undefined
}

type AvailabilityFilter = 'all' | 'available' | 'unavailable'
type SourceFilter = 'all' | 'discovered' | 'manual'
type ReferenceFilter = 'all' | 'referenced' | 'unreferenced'

let { providerId, routes, routeReferencesReady, syncedAt, syncedSummary, onSync }: Props = $props()
const queryClient = useQueryClient()
let search = $state('')
let columnFilters = $state<ColumnFiltersState>([
  {
    id: 'availability',
    value: {
      operator: 'and',
      constraints: [{ value: 'available', matchMode: 'equals' }],
    } satisfies DataTableFilterGroup,
  },
])
let filtersOpen = $state(false)
let selectedDetail = $state<ProviderModelDetail>()
let draft = $state(false)
let drawerOpen = $state(false)
let loadingDetail = $state(false)
let detailError = $state<unknown>()
let saving = $state(false)
let dirty = $state(false)
let pendingAction = $state<() => void | Promise<void>>()
let discardOpen = $state(false)
let discarding = $state(false)
let closingDrawer = $state(false)
let manualOpen = $state(false)
let manualTemplateId = $state('')
let preparingManual = $state(false)
let deleteOpen = $state(false)
let syncing = $state(false)
let syncSummary = $state<ProviderModelSyncSummary>()
let lastSyncedAt = $state<Date>()
let loadedQueryModel = $state('')
let editor = $state<{ submit: () => void }>()
let addingRouteModelId = $state('')

const modelsQuery = createQuery(() => ({
  queryKey: ['provider-models', providerId],
  queryFn: () => admin.providers.models(providerId),
}))
const canonicalModelsQuery = createQuery(() => ({
  queryKey: ['catalog', 'canonical-models'],
  queryFn: () => admin.catalog.canonicalModels(),
}))
const canonicalModels = $derived(canonicalModelsQuery.data?.models ?? [])
const displayedSyncedAt = $derived(syncedAt ?? lastSyncedAt)
const displayedSyncSummary = $derived(syncedSummary ?? syncSummary)
const models = $derived(modelsQuery.data?.models ?? [])
const requestedModelId = $derived(page.url.searchParams.get('model') ?? '')
const availabilityFilter = $derived(catalogFilterValue<AvailabilityFilter>('availability', 'all'))
const sourceFilter = $derived(catalogFilterValue<SourceFilter>('source_kind', 'all'))
const referenceFilter = $derived(catalogFilterValue<ReferenceFilter>('usage', 'all'))
const filteredModels = $derived.by(() => {
  const query = search.trim().toLocaleLowerCase(localeState.current)
  return models.filter((model) => {
    const references = modelReferences(model.id)
    return (
      (!query || `${model.name} ${model.id}`.toLocaleLowerCase(localeState.current).includes(query)) &&
      (availabilityFilter === 'all' || model.available === (availabilityFilter === 'available')) &&
      (sourceFilter === 'all' || model.source_kind === sourceFilter) &&
      (referenceFilter === 'all' || references.length > 0 === (referenceFilter === 'referenced'))
    )
  })
})
const activeFilterCount = $derived(
  Number(availabilityFilter !== 'all') + Number(sourceFilter !== 'all') + Number(referenceFilter !== 'all'),
)
const hasActiveFilters = $derived(Boolean(search.trim()) || activeFilterCount > 0)
const selectedReferences = $derived(selectedDetail ? modelReferences(selectedDetail.id) : [])
const tableLabels = $derived(getDataTableLabels())
const providerModelColumnHelper = createDataTableColumnHelper<ProviderModelSummary>()
const providerModelColumns = providerModelColumnHelper.columns([
  providerModelColumnHelper.accessor((model) => `${model.name} ${model.id}`, {
    id: 'model',
    header: () => m.common_model(),
    cell: (context) => renderSnippet(providerModelIdentityCell, context),
    enableSorting: false,
    enableGlobalFilter: true,
    meta: { label: () => m.common_model(), cellClass: 'whitespace-normal py-4' },
    size: 350,
  }),
  providerModelColumnHelper.accessor((model) => (model.available ? 'available' : 'unavailable'), {
    id: 'availability',
    header: () => m.provider_model_catalog_model_availability(),
    cell: (context) => renderSnippet(providerModelAvailabilityCell, context),
    enableSorting: false,
    enableGlobalFilter: false,
    meta: {
      label: () => m.provider_model_catalog_model_availability(),
      cellClass: 'whitespace-normal',
      filter: {
        variant: 'select',
        allLabel: m.common_all_models(),
        options: [
          { value: 'available', label: m.common_used() },
          { value: 'unavailable', label: m.common_unavailable() },
        ],
      },
    },
    size: 160,
  }),
  providerModelColumnHelper.accessor('source_kind', {
    header: () => m.provider_model_catalog_how_models_were_added(),
    cell: (context) => renderSnippet(providerModelSourceCell, context),
    enableSorting: false,
    enableGlobalFilter: false,
    meta: {
      label: () => m.provider_model_catalog_how_models_were_added(),
      filter: {
        variant: 'select',
        allLabel: m.provider_model_catalog_all_sources(),
        options: [
          { value: 'discovered', label: m.common_synced() },
          { value: 'manual', label: m.common_added_manually() },
        ],
      },
    },
    size: 160,
  }),
  providerModelColumnHelper.accessor((model) =>
    modelReferences(model.id).length > 0 ? 'referenced' : 'unreferenced', {
    id: 'usage',
    header: () => m.provider_model_catalog_model_usage(),
    cell: (context) => renderSnippet(providerModelUsageCell, context),
    enableSorting: false,
    enableGlobalFilter: false,
    meta: {
      label: () => m.provider_model_catalog_model_usage(),
      cellClass: 'whitespace-normal',
      filter: {
        variant: 'select',
        allLabel: m.provider_model_catalog_all_usage(),
        options: [
          { value: 'referenced', label: m.provider_model_catalog_use() },
          { value: 'unreferenced', label: m.provider_model_catalog_not_use() },
        ],
      },
    },
    size: 190,
  }),
])

function getProviderModelRowId(model: ProviderModelSummary): string {
  return model.id
}

$effect(() => {
  if (
    requestedModelId &&
    requestedModelId !== loadedQueryModel &&
    models.some((model) => model.id === requestedModelId)
  ) {
    void loadDetail(requestedModelId)
  }
})

function modelReferences(modelId: string): Array<{ route: Route; target: Route['targets'][number] }> {
  return routes.flatMap((route) =>
    route.targets
      .filter((target) => target.provider_id === providerId && target.model === modelId)
      .map((target) => ({ route, target })),
  )
}

function routeForModel(modelId: string): Route | undefined {
  return routes.find((route) => route.name === modelId)
}

function catalogFilterValue<TValue extends string>(columnId: string, fallback: TValue): TValue {
  const value = columnFilters.find((filter) => filter.id === columnId)?.value
  const candidate =
    value && typeof value === 'object' && 'constraints' in value && Array.isArray(value.constraints)
      ? (value as DataTableFilterGroup).constraints[0]?.value
      : value

  return typeof candidate === 'string' ? (candidate as TValue) : fallback
}

function setCatalogFilter(columnId: string, value: string, emptyValue = 'all'): void {
  const remaining = columnFilters.filter((filter) => filter.id !== columnId)
  columnFilters =
    value === emptyValue
      ? remaining
      : [
          ...remaining,
          {
            id: columnId,
            value: {
              operator: 'and',
              constraints: [{ value, matchMode: 'equals' }],
            } satisfies DataTableFilterGroup,
          },
        ]
}

function clearFilters(): void {
  search = ''
  columnFilters = []
}

function modelEditorSearch(modelId: string): string {
  const search = new SvelteURLSearchParams(page.url.searchParams)
  search.set('view', 'models')
  search.set('model', modelId)
  return search.toString()
}

function openProviderModel(model: ProviderModelSummary, event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  void goto(resolve(`/providers/${encodeURIComponent(providerId)}?${modelEditorSearch(model.id)}`))
}

function handleProviderModelTableRowClick({ event, original }: DataTableRowPointerEvent<ProviderModelSummary>): void {
  openProviderModel(original, event)
}

function handleProviderModelRowKeydown(event: KeyboardEvent, model: ProviderModelSummary): void {
  if (event.key !== 'Enter' || event.target !== event.currentTarget) return
  event.preventDefault()
  void goto(resolve(`/providers/${encodeURIComponent(providerId)}?${modelEditorSearch(model.id)}`))
}

async function addModelToRoute(model: ProviderModelSummary): Promise<void> {
  if (!routeReferencesReady || addingRouteModelId) return

  addingRouteModelId = model.id
  try {
    const existingRoute = routeForModel(model.id)
    await admin.models.bind({ provider_id: providerId, provider_model_id: model.id })
    if (existingRoute) {
      toast.success(m.provider_model_catalog_model_target_added({ id: model.id }))
    } else {
      toast.success(m.provider_model_catalog_model_route_created({ id: model.id }))
    }
    await queryClient.invalidateQueries({ queryKey: ['models'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    addingRouteModelId = ''
  }
}

function availabilityReason(model: ProviderModelSummary): string | null {
  if (model.available) return null
  if (model.selection_policy === 'force_disabled') return m.provider_model_catalog_hidden_adding_models()
  return m.provider_model_catalog_service_no_longer_offers_model()
}

function requestGuard(action: () => void | Promise<void>): void {
  if (!dirty) {
    void action()
    return
  }
  pendingAction = action
  discardOpen = true
}

async function confirmDiscard(): Promise<void> {
  const action = pendingAction
  pendingAction = undefined
  discardOpen = false
  discarding = true
  dirty = false
  await tick()
  if (action) await action()
}

async function updateModelQuery(modelId?: string): Promise<void> {
  const search = new SvelteURLSearchParams(page.url.searchParams)
  if (modelId) search.set('model', modelId)
  else search.delete('model')
  await goto(resolve(`/providers/${encodeURIComponent(providerId)}?${search}`), {
    replaceState: true,
    noScroll: true,
    keepFocus: true,
  })
}

async function loadDetail(modelId: string): Promise<void> {
  loadingDetail = true
  discarding = false
  detailError = undefined
  loadedQueryModel = modelId
  try {
    selectedDetail = await admin.providers.model(providerId, modelId)
    draft = false
    dirty = false
    drawerOpen = false
  } catch (error) {
    detailError = error
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    loadingDetail = false
  }
}

function requestClose(): void {
  requestGuard(async () => {
    closingDrawer = true
    selectedDetail = undefined
    draft = false
    detailError = undefined
    await updateModelQuery()
    loadedQueryModel = ''
    drawerOpen = false
    await tick()
    closingDrawer = false
    discarding = false
  })
}

function handleDrawerOpen(nextOpen: boolean): void {
  if (nextOpen) {
    drawerOpen = true
  } else if (closingDrawer) {
    drawerOpen = false
  } else if (dirty) {
    drawerOpen = true
    requestClose()
  } else {
    requestClose()
  }
}
async function syncModels(): Promise<void> {
  if (onSync) {
    syncing = true
    try {
      const summary = await onSync()
      if (!summary) return
      syncSummary = summary
      await modelsQuery.refetch()
      lastSyncedAt = new Date()
    } finally {
      syncing = false
    }
    return
  }
  syncing = true
  try {
    syncSummary = await admin.providers.syncModels(providerId)
    await modelsQuery.refetch()
    lastSyncedAt = new Date()
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    syncing = false
  }
}

function requestSync(): void {
  requestGuard(syncModels)
}

async function prepareManualModel(): Promise<void> {
  const templateId = manualTemplateId.trim()
  if (!templateId) return
  preparingManual = true
  try {
    selectedDetail = preparedDetail(
      await admin.providers.prepareModel(providerId, modelIdFromCatalogId(templateId), templateId),
    )
    draft = !models.some((model) => model.id === selectedDetail?.id)
    dirty = false
    drawerOpen = draft
    manualOpen = false
    manualTemplateId = ''
    loadedQueryModel = selectedDetail.id
    await updateModelQuery(selectedDetail.id)
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    preparingManual = false
  }
}

function selectManualTemplate(templateId: string): void {
  manualTemplateId = templateId
}

function clearManualTemplate(): void {
  manualTemplateId = ''
}

function preparedDetail(prepared: PreparedProviderModel): ProviderModelDetail {
  return {
    ...prepared,
    available: true,
    source_kind: 'manual',
    can_reimport: false,
    selection_policy: 'auto',
    revision: 0,
    created_at: '',
    updated_at: '',
  }
}

async function saveModel(metadataJson: string): Promise<void> {
  if (!selectedDetail) return
  const wasDraft = draft
  saving = true
  try {
    const saved = draft
      ? await admin.providers.createManualModel(providerId, selectedDetail.id, metadataJson)
      : await admin.providers.updateModel(providerId, selectedDetail.id, metadataJson, selectedDetail.revision)
    selectedDetail = saved
    draft = false
    if (wasDraft) drawerOpen = false
    dirty = false
    await Promise.all([
      modelsQuery.refetch(),
      queryClient.invalidateQueries({ queryKey: ['models'] }),
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
    ])
    toast.success(m.provider_model_catalog_model_details_saved())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}

async function updateSelection(policy: ProviderModelSelectionPolicy): Promise<void> {
  if (!selectedDetail || selectedDetail.selection_policy === policy) return
  saving = true
  try {
    const updated = await admin.providers.updateModelSelection(
      providerId,
      selectedDetail.id,
      policy,
      selectedDetail.revision,
    )
    selectedDetail.selection_policy = updated.selection_policy
    selectedDetail.available = updated.available
    selectedDetail.revision = updated.revision
    await Promise.all([modelsQuery.refetch(), queryClient.invalidateQueries({ queryKey: ['models'] })])
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}

async function reimportModel(): Promise<void> {
  if (!selectedDetail) return
  saving = true
  try {
    selectedDetail = await admin.providers.reimportModel(providerId, selectedDetail.id, selectedDetail.revision)
    dirty = false
    await modelsQuery.refetch()
    toast.success(m.provider_model_catalog_model_details_restored_service())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}

function requestReimport(): void {
  requestGuard(reimportModel)
}

function requestDelete(): void {
  requestGuard(() => {
    deleteOpen = true
  })
}

async function deleteManualModel(): Promise<void> {
  if (!selectedDetail || selectedDetail.source_kind !== 'manual') return
  saving = true
  try {
    await admin.providers.deleteManualModel(providerId, selectedDetail.id)
    deleteOpen = false
    drawerOpen = false
    selectedDetail = undefined
    loadedQueryModel = ''
    await Promise.all([modelsQuery.refetch(), queryClient.invalidateQueries({ queryKey: ['models'] })])
    await updateModelQuery()
    toast.success(m.provider_model_catalog_manually_added_model_removed())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}
</script>

{#snippet availabilitySelect(id: string)}
  <Select.Root
    type="single"
    bind:value={() => availabilityFilter, (value) => setCatalogFilter('availability', value ?? 'all')}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_model_availability()}>
      {availabilityFilter === 'all'
        ? m.common_all_models()
        : availabilityFilter === 'available'
          ? m.common_used()
          : m.common_unavailable()}
    </Select.Trigger>
    <Select.Content>
      <Select.Item value="all">{m.common_all_models()}</Select.Item>
      <Select.Item value="available">{m.common_used()}</Select.Item>
      <Select.Item value="unavailable">{m.common_unavailable()}</Select.Item>
    </Select.Content>
  </Select.Root>
{/snippet}

{#snippet sourceSelect(id: string)}
  <Select.Root
    type="single"
    bind:value={() => sourceFilter, (value) => setCatalogFilter('source_kind', value ?? 'all')}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_how_models_were_added()}>
      {sourceFilter === 'all'
        ? m.provider_model_catalog_all_sources()
        : sourceFilter === 'manual'
          ? m.common_added_manually()
          : m.common_synced()}
    </Select.Trigger>
    <Select.Content>
      <Select.Item value="all">{m.provider_model_catalog_all_sources()}</Select.Item>
      <Select.Item value="discovered">{m.common_synced()}</Select.Item>
      <Select.Item value="manual">{m.common_added_manually()}</Select.Item>
    </Select.Content>
  </Select.Root>
{/snippet}

{#snippet referenceSelect(id: string)}
  <Select.Root type="single" bind:value={() => referenceFilter, (value) => setCatalogFilter('usage', value ?? 'all')}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_model_usage()}>
      {referenceFilter === 'all'
        ? m.provider_model_catalog_all_usage()
        : referenceFilter === 'referenced'
          ? m.provider_model_catalog_use()
          : m.provider_model_catalog_not_use()}
    </Select.Trigger>
    <Select.Content>
      <Select.Item value="all">{m.provider_model_catalog_all_usage()}</Select.Item>
      <Select.Item value="referenced">{m.provider_model_catalog_use()}</Select.Item>
      <Select.Item value="unreferenced">{m.provider_model_catalog_not_use()}</Select.Item>
    </Select.Content>
  </Select.Root>
{/snippet}

{#snippet providerModelIdentityCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@const model = context.row.original}
  <div class="min-h-10 w-full min-w-0 text-left" aria-label={`${model.name} ${model.id}`}>
    <span class="block truncate font-medium">{model.name}</span>
    <span class="block truncate font-technical text-xs text-muted-foreground">{model.id}</span>
    <span class="mt-1 flex flex-wrap gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
      {#if model.capabilities.context}<span>{formatNumber(model.capabilities.context)} {m.common_context()}</span>{/if}
      {#if model.capabilities.reasoning}<span>{m.common_reasoning()}</span>{/if}
      {#if model.capabilities.tool_call}<span>{m.common_tool_calls()}</span>{/if}
      {#if model.capabilities.attachment}<span>{m.common_attachments()}</span>{/if}
    </span>
  </div>
{/snippet}

{#snippet providerModelAvailabilityCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@const model = context.row.original}
  {@const reason = availabilityReason(model)}
  <Badge variant={model.available ? 'secondary' : 'outline'}>
    {model.available ? m.common_used() : m.common_unavailable()}
  </Badge>
  {#if reason}<p class="mt-1 text-xs text-muted-foreground">{reason}</p>{/if}
{/snippet}

{#snippet providerModelSourceCell(context: DataTableCellContext<ProviderModelSummary>)}
  <Badge variant="outline">
    {context.row.original.source_kind === 'manual' ? m.common_added_manually() : m.common_synced()}
  </Badge>
{/snippet}

{#snippet providerModelUsageCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@const model = context.row.original}
  {@const references = modelReferences(model.id)}
  {@const matchingRoute = routeForModel(model.id)}
  {#if references.length > 0}
    <a
      class="inline-flex min-h-10 w-fit items-center rounded-md px-2 text-sm font-medium hover:bg-muted"
      href={resolve(`/providers/${encodeURIComponent(providerId)}?view=routes`)}>
      {m.provider_model_catalog_used_by_models({ count: references.length })}
    </a>
  {:else if routeReferencesReady}
    <button
      type="button"
      class="group inline-flex min-h-10 w-fit items-center rounded-md px-2 text-sm text-muted-foreground hover:bg-muted hover:text-foreground disabled:pointer-events-none disabled:opacity-70"
      aria-label={m.provider_model_catalog_add_model_to_route({ id: model.id })}
      disabled={Boolean(addingRouteModelId)}
      onclick={() => void addModelToRoute(model)}>
      {#if addingRouteModelId === model.id}<Spinner data-icon="inline-start" />{/if}
      <span class="group-hover:hidden group-focus-visible:hidden">{m.provider_model_catalog_not_used()}</span>
      <span class="hidden group-hover:inline group-focus-visible:inline">
        {matchingRoute ? m.provider_model_catalog_add_destination() : m.provider_model_catalog_create_model()}
      </span>
    </button>
  {:else}
    <span class="px-2 text-sm text-muted-foreground">{m.provider_model_catalog_not_used()}</span>
  {/if}
{/snippet}

{#snippet providerModelsLoading()}
  <div class="grid min-h-56 place-items-center"><Spinner /></div>
{/snippet}

{#snippet providerModelsEmpty()}
  <div class="py-6">
    {#if modelsQuery.isError}
      <p class="text-sm text-destructive">{localizeBackendErrorMessage(modelsQuery.error)}</p>
      <Button class="mt-3" variant="outline" onclick={() => void modelsQuery.refetch()}>{m.common_retry()}</Button>
    {:else}
      <p class="text-sm text-muted-foreground">{m.provider_model_catalog_no_models_match_filters()}</p>
      {#if hasActiveFilters}
        <Button class="mt-3" size="sm" variant="outline" onclick={clearFilters}>
          {m.provider_model_catalog_clear_filters()}
        </Button>
      {/if}
    {/if}
  </div>
{/snippet}

{#if requestedModelId && !draft}
  <section class="route-section" aria-labelledby="provider-model-editor-title">
    {#if loadingDetail || modelsQuery.isPending}
      <div class="grid min-h-72 place-items-center"><Spinner /></div>
    {:else if detailError || !selectedDetail}
      <div class="py-8">
        <p class="text-sm text-destructive">
          {detailError ? localizeBackendErrorMessage(detailError) : m.backend_error_catalog_model_not_found()}
        </p>
        <Button class="mt-3" variant="outline" onclick={requestClose}>{m.common_cancel()}</Button>
      </div>
    {:else}
      <div class="route-section-header">
        <div class="min-w-0">
          <h2 id="provider-model-editor-title" class="route-section-title truncate">
            {selectedDetail.metadata.name || selectedDetail.id}
          </h2>
          <p class="route-section-description break-all font-technical">{selectedDetail.id}</p>
        </div>
        <DropdownMenu.Root>
          <DropdownMenu.Trigger>
            {#snippet child({ props })}
              <Button
                {...props}
                variant="ghost"
                size="icon"
                class="size-10"
                aria-label={m.provider_model_catalog_model_actions()}><MoreHorizontalIcon /></Button>
            {/snippet}
          </DropdownMenu.Trigger>
          <DropdownMenu.Content align="end">
            <DropdownMenu.Item
              onSelect={() =>
                void goto(
                  resolve(
                    `/models/new?provider=${encodeURIComponent(providerId)}&model=${encodeURIComponent(selectedDetail!.id)}`,
                  ),
                )}>
              {m.provider_model_catalog_use_new_model()}
            </DropdownMenu.Item>
            {#if selectedDetail.can_reimport}
              <DropdownMenu.Item onSelect={requestReimport}
                >{m.provider_model_catalog_restore_details_service()}</DropdownMenu.Item>
            {:else}
              <DropdownMenu.Separator />
              <DropdownMenu.Item variant="destructive" onSelect={requestDelete}
                >{m.provider_model_catalog_remove_manually_added_model()}</DropdownMenu.Item>
            {/if}
          </DropdownMenu.Content>
        </DropdownMenu.Root>
      </div>
      <div>
        <ProviderModelEditor
          bind:this={editor}
          detail={selectedDetail}
          {draft}
          onSave={(metadataJson) => void saveModel(metadataJson)}
          onSelectionChange={(policy) => void updateSelection(policy)}
          onDirtyChange={(value) => {
            if (!discarding) dirty = value
          }} />
      </div>
      <div
        class="sticky bottom-0 z-20 mt-2 flex translate-y-2 justify-end gap-2 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
        <Button variant="outline" onclick={requestClose}>{m.common_cancel()}</Button>
        <Button onclick={() => editor?.submit()} disabled={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_model()}
        </Button>
      </div>
    {/if}
  </section>
{:else}
  <section class="route-section" aria-labelledby="provider-model-inventory-title">
    <div class="route-section-header">
      <div>
        <h2 id="provider-model-inventory-title" class="route-section-title">
          {m.provider_model_catalog_models_service()}
        </h2>
        <p class="route-section-description">
          {m.provider_model_catalog_page_summary()}
        </p>
        <p class="mt-2 text-xs text-muted-foreground">
          {#if displayedSyncedAt}
            {m.provider_model_catalog_last_checked()}
            {formatTime(displayedSyncedAt)}
            {#if displayedSyncSummary}
              · {displayedSyncSummary.added}
              {m.provider_model_catalog_new()} · {displayedSyncSummary.missing}
              {m.provider_model_catalog_no_longer_offered()} · {displayedSyncSummary.restored}
              {m.provider_model_catalog_available_again()}
            {/if}
          {:else}
            {m.provider_model_catalog_sync_list_check_model_updates()}
          {/if}
        </p>
      </div>
      <div class="flex flex-wrap gap-2">
        <Button variant="outline" onclick={() => (manualOpen = true)}
          ><PlusIcon data-icon="inline-start" />{m.provider_model_catalog_add_model_manually()}</Button>
        <Button variant="outline" onclick={requestSync} disabled={syncing}>
          {#if syncing}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon data-icon="inline-start" />{/if}
          {m.provider_model_catalog_sync_models()}
        </Button>
      </div>
    </div>

    <div class="route-desktop-table">
      <DataTable
        data={modelsQuery.isError ? [] : models}
        columns={providerModelColumns}
        labels={tableLabels}
        getRowId={getProviderModelRowId}
        ariaLabel={m.provider_model_catalog_models_service()}
        empty={providerModelsEmpty}
        loading={modelsQuery.isPending}
        loadingContent={providerModelsLoading}
        filterDisplay="menu"
        globalFilterEnabled
        globalFilterId="provider-model-search-desktop"
        globalFilterPlaceholder={m.provider_model_catalog_search_name_model_id()}
        bind:globalFilter={search}
        bind:columnFilters
        stripedRows
        onRowClick={handleProviderModelTableRowClick} />
    </div>

    <div class="route-mobile-list">
      <div class="border-y py-3">
        <div class="relative">
          <SearchIcon
            class="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            id="provider-model-search-mobile"
            aria-label={m.provider_model_catalog_search_models()}
            class="h-10 pl-9"
            bind:value={search}
            placeholder={m.provider_model_catalog_search_name_model_id()} />
        </div>
        <div class="mt-2 flex items-center justify-between gap-2">
          <Button variant="outline" onclick={() => (filtersOpen = true)}>
            <SlidersHorizontalIcon data-icon="inline-start" />
            {m.provider_model_catalog_filter_models()}
            {#if activeFilterCount > 0}<span class="font-technical">· {activeFilterCount}</span>{/if}
          </Button>
          {#if hasActiveFilters}
            <Button size="sm" variant="ghost" onclick={clearFilters}>{m.provider_model_catalog_clear_filters()}</Button>
          {/if}
        </div>
      </div>

      {#if modelsQuery.isPending}
        <div class="grid min-h-56 place-items-center"><Spinner /></div>
      {:else if modelsQuery.isError}
        <div class="border-b py-8">
          <p class="text-sm text-destructive">
            {localizeBackendErrorMessage(modelsQuery.error)}
          </p>
          <Button class="mt-3" variant="outline" onclick={() => void modelsQuery.refetch()}>{m.common_retry()}</Button>
        </div>
      {:else if filteredModels.length === 0}
        <div class="border-b py-8">
          <p class="text-sm text-muted-foreground">
            {m.provider_model_catalog_no_models_match_filters()}
          </p>
          {#if hasActiveFilters}
            <Button class="mt-3" size="sm" variant="outline" onclick={clearFilters}>
              {m.provider_model_catalog_clear_filters()}
            </Button>
          {/if}
        </div>
      {:else}
        {#each filteredModels as model (model.id)}
          {@const references = modelReferences(model.id)}
          {@const reason = availabilityReason(model)}
          {@const matchingRoute = routeForModel(model.id)}
          <div
            class="route-mobile-row cursor-pointer"
            role="link"
            tabindex="0"
            onclick={(event) => openProviderModel(model, event)}
            onkeydown={(event) => handleProviderModelRowKeydown(event, model)}>
            <div class="col-span-2 min-h-10 min-w-0 text-left" aria-label={`${model.name} ${model.id}`}>
              <span class="block truncate font-medium">{model.name}</span>
              <span class="block truncate font-technical text-xs text-muted-foreground">{model.id}</span>
              <span class="mt-1 flex flex-wrap gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
                {#if model.capabilities.context}<span
                    >{formatNumber(model.capabilities.context)} {m.common_context()}</span
                  >{/if}
                {#if model.capabilities.reasoning}<span>{m.common_reasoning()}</span>{/if}
                {#if model.capabilities.tool_call}<span>{m.common_tool_calls()}</span>{/if}
                {#if model.capabilities.attachment}<span>{m.common_attachments()}</span>{/if}
              </span>
            </div>
            <div class="col-span-2 flex min-w-0 flex-wrap items-center gap-2">
              <Badge variant={model.available ? 'secondary' : 'outline'}
                >{model.available ? m.common_used() : m.common_unavailable()}</Badge>
              <Badge variant="outline"
                >{model.source_kind === 'manual' ? m.common_added_manually() : m.common_synced()}</Badge>
              {#if references.length > 0}
                <a
                  class="inline-flex min-h-10 items-center rounded-md px-2 text-sm font-medium hover:bg-muted"
                  href={resolve(`/providers/${encodeURIComponent(providerId)}?view=routes`)}>
                  {m.provider_model_catalog_used_by_models({ count: references.length })}
                </a>
              {:else if routeReferencesReady}
                <button
                  type="button"
                  class="group inline-flex min-h-10 items-center rounded-md px-2 text-sm text-muted-foreground hover:bg-muted hover:text-foreground disabled:pointer-events-none disabled:opacity-70"
                  aria-label={m.provider_model_catalog_add_model_to_route({ id: model.id })}
                  disabled={Boolean(addingRouteModelId)}
                  onclick={() => void addModelToRoute(model)}>
                  {#if addingRouteModelId === model.id}<Spinner data-icon="inline-start" />{/if}
                  <span class="group-hover:hidden group-focus-visible:hidden">
                    {m.provider_model_catalog_not_used()}
                  </span>
                  <span class="hidden group-hover:inline group-focus-visible:inline">
                    {matchingRoute
                      ? m.provider_model_catalog_add_destination()
                      : m.provider_model_catalog_create_model()}
                  </span>
                </button>
              {:else}
                <span class="px-2 text-sm text-muted-foreground">{m.provider_model_catalog_not_used()}</span>
              {/if}
              {#if reason}<span class="text-xs text-muted-foreground">{reason}</span>{/if}
            </div>
          </div>
        {/each}
      {/if}
    </div>
  </section>
{/if}

<Sheet.Root bind:open={filtersOpen}>
  <Sheet.Content
    side="right"
    class="w-full! max-w-none! gap-0 p-0 sm:max-w-sm!"
    closeLabel={m.provider_model_catalog_close_model_filters()}>
    <Sheet.Header class="border-b">
      <Sheet.Title>{m.provider_model_catalog_filter_models()}</Sheet.Title>
      <Sheet.Description>{m.provider_model_catalog_filter_models_description()}</Sheet.Description>
    </Sheet.Header>
    <div class="route-overlay-body">
      <Field.FieldGroup>
        <Field.Field>
          <Field.FieldLabel for="provider-model-availability-mobile">
            {m.provider_model_catalog_model_availability()}
          </Field.FieldLabel>
          {@render availabilitySelect('provider-model-availability-mobile')}
        </Field.Field>
        <Field.Field>
          <Field.FieldLabel for="provider-model-source-mobile">
            {m.provider_model_catalog_how_models_were_added()}
          </Field.FieldLabel>
          {@render sourceSelect('provider-model-source-mobile')}
        </Field.Field>
        <Field.Field>
          <Field.FieldLabel for="provider-model-reference-mobile">
            {m.provider_model_catalog_model_usage()}
          </Field.FieldLabel>
          {@render referenceSelect('provider-model-reference-mobile')}
        </Field.Field>
      </Field.FieldGroup>
    </div>
    <Sheet.Footer class="route-overlay-footer">
      <Button variant="outline" onclick={clearFilters}>{m.provider_model_catalog_clear_filters()}</Button>
      <Sheet.Close class="h-10 rounded-md bg-primary px-3 text-primary-foreground">
        {m.provider_model_catalog_show_models()}
      </Sheet.Close>
    </Sheet.Footer>
  </Sheet.Content>
</Sheet.Root>

<Dialog.Root bind:open={manualOpen}>
  <Dialog.Content>
    <Dialog.Header>
      <Dialog.Title>{m.provider_model_catalog_add_model_manually_label()}</Dialog.Title>
    </Dialog.Header>
    <Field.Field>
      <Field.Label>{m.provider_model_catalog_search_model()}</Field.Label>
      <ModelCombobox
        id="manual-provider-model-search"
        value={manualTemplateId}
        models={canonicalModels}
        placeholder={m.provider_model_catalog_search_model()}
        searchPlaceholder={m.provider_model_catalog_search_model()}
        emptyText={m.provider_model_catalog_no_models_found()}
        ariaLabel={m.provider_model_catalog_search_model()}
        searchAriaLabel={m.provider_model_catalog_search_model()}
        clearAriaLabel={m.provider_model_catalog_clear_selected_model()}
        disabled={canonicalModelsQuery.isPending}
        onSelect={selectManualTemplate}
        onClear={clearManualTemplate} />
    </Field.Field>
    <Dialog.Footer>
      <Button variant="outline" onclick={() => (manualOpen = false)}>{m.common_cancel()}</Button>
      <Button onclick={() => void prepareManualModel()} disabled={preparingManual || !manualTemplateId.trim()}>
        {#if preparingManual}<Spinner data-icon="inline-start" />{/if}{m.provider_model_catalog_continue()}
      </Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>

<Sheet.Root bind:open={drawerOpen} onOpenChange={handleDrawerOpen}>
  <Sheet.Content
    side="right"
    class="provider-model-drawer w-full! max-w-none! gap-0 overflow-hidden p-0 sm:max-w-[960px]!"
    closeLabel={m.provider_model_catalog_close_model_editor()}>
    {#if selectedDetail}
      <Sheet.Header class="border-b pr-14">
        <Sheet.Title class="truncate">{selectedDetail.metadata.name || selectedDetail.id}</Sheet.Title>
        <Sheet.Description class="break-all font-technical">{selectedDetail.id}</Sheet.Description>
      </Sheet.Header>
      <div class="route-overlay-body" data-provider-model-scroll-owner>
        {#if loadingDetail}
          <div class="grid min-h-72 place-items-center"><Spinner /></div>
        {:else}
          <ProviderModelEditor
            bind:this={editor}
            detail={selectedDetail}
            {draft}
            onSave={(metadataJson) => void saveModel(metadataJson)}
            onSelectionChange={(policy) => void updateSelection(policy)}
            onDirtyChange={(value) => {
              if (!discarding) dirty = value
            }} />
        {/if}
      </div>
      <Sheet.Footer class="route-overlay-footer justify-between sm:justify-between">
        <Button variant="outline" onclick={requestClose}>{m.common_cancel()}</Button>
        <Button onclick={() => editor?.submit()} disabled={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}
          {m.common_add_model()}
        </Button>
      </Sheet.Footer>
    {/if}
  </Sheet.Content>
</Sheet.Root>

<AlertDialog.Root bind:open={discardOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.provider_model_catalog_discard_unsaved_model_changes()}</AlertDialog.Title>
      <AlertDialog.Description>{m.provider_model_catalog_unsaved_changes_warning()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={() => (pendingAction = undefined)}
        >{m.provider_model_catalog_keep_editing()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" onclick={() => void confirmDiscard()}
        >{m.provider_model_catalog_discard_changes()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title
        >{m.provider_model_catalog_delete_value({
          id: selectedDetail?.metadata.name || selectedDetail?.id || m.common_model(),
        })}</AlertDialog.Title>
      <AlertDialog.Description>
        {#if routeReferencesReady}
          {m.provider_model_catalog_remove_manual_references({ count: selectedReferences.length })}
        {:else}
          {m.provider_model_catalog_usage_check_error()}
        {/if}
      </AlertDialog.Description>
    </AlertDialog.Header>
    {#if selectedReferences.length > 0}
      <div class="flex flex-col gap-1 rounded-lg border p-3">
        {#each selectedReferences as reference (reference.target.id)}
          <a
            class="flex min-h-10 items-center rounded-md px-2 font-medium hover:bg-muted"
            href={resolve('/models/[id]', { id: reference.route.name })}
            >{reference.route.name} · {reference.target.model}</a>
        {/each}
      </div>
    {/if}
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action
        variant="destructive"
        disabled={saving || !routeReferencesReady}
        onclick={() => void deleteManualModel()}>{m.provider_model_catalog_remove_list()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

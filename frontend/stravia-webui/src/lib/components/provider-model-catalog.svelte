<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { beforeNavigate, goto } from '$app/navigation'
import { resolve } from '$app/paths'
import type { Path } from '$app/types'
import { page } from '$app/state'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet, type ColumnFiltersState } from '@tanstack/svelte-table'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import PlusIcon from '@lucide/svelte/icons/plus'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import { onDestroy, tick } from 'svelte'
import SearchIcon from '@lucide/svelte/icons/search'
import { SvelteURLSearchParams } from 'svelte/reactivity'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'

import {
  getModelChanges,
  summaryDescription,
  sourceLabel,
  syncToast,
  syncErrorToast,
  modelChangeReason,
} from '#lib/model-changes.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { ProviderModelEditing } from '#lib/provider-model-editing.svelte.js'
import type { ProviderModelEditingOperation } from '#lib/provider-model-editing.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import { formatLogTime } from '#lib/format.js'
import { formatSpecificationTokens } from '#lib/model-specification.js'
import FormSection from '#lib/components/form-section.svelte'
import ModalityIcons from '#lib/components/modality-icons.svelte'
import { localeState } from '#lib/localization.svelte.js'
import {
  emptySpecificationFilter,
  matchesSpecification,
  specificationFilterCount,
  type SpecificationFilter,
} from '#lib/model-specification-filter.js'
import ModelSpecificationFilter from '#lib/components/model-specification-filter.svelte'
import type { Route, ProviderModelSelectionPolicy, ProviderModelSummary, ProviderModelSyncSummary } from '#lib/types.js'
import ProviderModelEditor from '#lib/components/provider-model-editor.svelte'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableFilterGroup,
  type DataTableRowPointerEvent,
} from '#lib/components/ui/data-table/index.js'

import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import { allCatalogFilterValue, catalogFilterOptions } from './provider-model-catalog/filter-options'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import CatalogConfirmations from './provider-model-catalog/confirmations.svelte'
import CatalogEditorDrawer from './provider-model-catalog/editor-drawer.svelte'
import CatalogFilterSheet from './provider-model-catalog/filter-sheet.svelte'
import ManualModelDialog from './provider-model-catalog/manual-model-dialog.svelte'
import RpmLimit from './provider-model-catalog/rpm-limit.svelte'
import RpmForm from './provider-model-catalog/rpm-form.svelte'
import ModelCellEditor from './provider-model-catalog/model-cell-editor.svelte'

interface Props {
  providerId: string
  routes: Route[]
  routeReferencesReady: boolean
  onSync?: (providerId: string) => Promise<ProviderModelSyncSummary | undefined> | ProviderModelSyncSummary | undefined
}

type AvailabilityFilter = 'all' | 'available' | 'unavailable'
type SourceFilter = 'all' | 'discovered' | 'manual'
type ReferenceFilter = 'all' | 'referenced' | 'unreferenced'

let { providerId, routes, routeReferencesReady, onSync }: Props = $props()
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
let manualOpen = $state(false)
let manualTemplateId = $state('')
let syncing = $state(false)
const changesSheet = getModelChanges()
let preferenceSaving = $state(false)
let preferenceError = $state('')
let preferenceEpoch = 0
let editor = $state<{ submit: () => void }>()
let rpmForm = $state<{ save: () => Promise<void> }>()
// 详情页底部只有一个「保存模型」：模型规格与 RPM 限额是两个独立资源，按各自是否有改动分别提交。
let specificationDirty = $state(false)

let rpmDirty = $state(false)
let rpmSaving = $state(false)
let addingRouteModelId = $state('')
let internalNavigation = false

const modelsQuery = createQuery(() => ({
  queryKey: ['provider-models', providerId],
  queryFn: () => admin.providers.models(providerId),
  refetchInterval: 15_000,
}))
const syncStateQuery = createQuery(() => ({
  queryKey: ['provider-model-sync-state', providerId],
  queryFn: () => admin.providers.syncState(providerId),
  refetchInterval: 5_000,
}))
async function saveAutomaticSync(enabled: boolean): Promise<void> {
  const id = providerId
  const epoch = ++preferenceEpoch
  preferenceSaving = true
  preferenceError = ''
  try {
    const state = await admin.providers.updateSyncState(id, { automatic_sync: enabled })
    if (id !== providerId || epoch !== preferenceEpoch) return
    queryClient.setQueryData(['provider-model-sync-state', id], state)
  } catch (error) {
    if (id === providerId && epoch === preferenceEpoch) preferenceError = localizeBackendErrorMessage(error)
  } finally {
    if (id === providerId && epoch === preferenceEpoch) preferenceSaving = false
  }
}
$effect(() => {
  if (providerId) {
    preferenceEpoch += 1
    preferenceSaving = false
    preferenceError = ''
    syncing = false
  }
})
const canonicalModelsQuery = createQuery(() => ({
  queryKey: ['catalog', 'canonical-models'],
  queryFn: () => admin.catalog.canonicalModels(),
}))
const canonicalModels = $derived(canonicalModelsQuery.data?.models ?? [])
const editing = new ProviderModelEditing({
  api: admin.providers,
  navigate: updateModelQuery,
  refresh: refreshModelData,
  settleEditor: tick,
  onSuccess: (operation: ProviderModelEditingOperation) => {
    if (operation === 'save') toast.success(m.provider_model_catalog_model_details_saved())
    else if (operation === 'reimport') toast.success(m.provider_model_catalog_model_details_restored_service())
    else if (operation === 'delete') toast.success(m.provider_model_catalog_manually_added_model_removed())
  },
  onError: (error: unknown, phase: string) => {
    if (phase !== 'refresh') toast.error(localizeBackendErrorMessage(error))
  },
})

const modelEditing = $derived(editing.state)
const displayedSyncSummary = $derived(syncStateQuery.data?.last_check)
const models = $derived(modelsQuery.data?.models ?? [])
const requestedModelId = $derived(page.url.searchParams.get('model') ?? '')
const availabilityFilter = $derived(catalogFilterValue<AvailabilityFilter>('availability', 'all'))
const sourceFilter = $derived(catalogFilterValue<SourceFilter>('source_kind', 'all'))
const referenceFilter = $derived(catalogFilterValue<ReferenceFilter>('usage', 'all'))
const specificationFilter = $derived(
  (columnFilters.find((filter) => filter.id === 'specification')?.value as SpecificationFilter | undefined) ??
    emptySpecificationFilter,
)

const filteredModels = $derived.by(() => {
  const query = search.trim().toLocaleLowerCase(localeState.current)
  return models.filter((model) => {
    const references = modelReferences(model.id)

    return (
      (!query || `${model.name} ${model.id}`.toLocaleLowerCase(localeState.current).includes(query)) &&
      (availabilityFilter === 'all' || model.available === (availabilityFilter === 'available')) &&
      (sourceFilter === 'all' || model.source_kind === sourceFilter) &&
      (referenceFilter === 'all' || references.length > 0 === (referenceFilter === 'referenced')) &&
      matchesSpecification(model.specification, specificationFilter)
    )
  })
})

const activeFilterCount = $derived(
  Number(availabilityFilter !== 'all') +
    Number(sourceFilter !== 'all') +
    Number(referenceFilter !== 'all') +
    specificationFilterCount(specificationFilter),
)
const hasActiveFilters = $derived(Boolean(search.trim()) || activeFilterCount > 0)
const selectedReferences = $derived(modelEditing.detail ? modelReferences(modelEditing.detail.id) : [])
const tableLabels = $derived(getDataTableLabels())
const providerModelColumnHelper = createDataTableColumnHelper<ProviderModelSummary>()
const filterOptions = $derived(catalogFilterOptions())

const providerModelColumns = $derived(
  providerModelColumnHelper.columns([
    providerModelColumnHelper.accessor((model) => `${model.name} ${model.id}`, {
      id: 'model',
      header: () => m.common_model(),
      cell: (context) => renderSnippet(providerModelIdentityCell, context),
      enableSorting: false,
      enableGlobalFilter: true,
      meta: { label: () => m.common_model(), cellClass: 'whitespace-normal py-2', width: 'fill' },
      size: 260,
    }),
    providerModelColumnHelper.accessor('specification', {
      header: () => m.model_specification_context(),
      cell: (context) => renderSnippet(providerModelSpecificationCell, context),
      filterFn: (row, _columnId, value) =>
        matchesSpecification(row.original.specification, value as SpecificationFilter),
      enableSorting: false,
      enableGlobalFilter: false,
      meta: {
        label: () => m.model_specification_title(),
        cellClass: 'whitespace-normal py-2',
        exportable: false,
        filter: { variant: 'custom', content: providerModelSpecificationFilter },
      },
      size: 150,
    }),
    providerModelColumnHelper.display({
      id: 'modalities',
      header: () => m.model_specification_modalities(),
      cell: (context) => renderSnippet(providerModelModalitiesCell, context),
      meta: { label: () => m.model_specification_modalities(), cellClass: 'whitespace-normal', exportable: false },
      size: 230,
    }),
    providerModelColumnHelper.display({
      id: 'efforts',
      header: () => m.model_specification_reasoning_efforts(),
      cell: (context) => renderSnippet(providerModelEffortsCell, context),
      meta: {
        label: () => m.model_specification_reasoning_efforts(),
        cellClass: 'whitespace-normal',
        width: 'fill',
        exportable: false,
      },
      size: 170,
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
        filter: { variant: 'select', ...filterOptions.availability },
      },
      size: 160,
    }),
    providerModelColumnHelper.display({
      id: 'rpm',
      header: () => m.rpm_column(),
      cell: (context) => renderSnippet(providerModelRpmCell, context),
      meta: { label: () => m.rpm_column(), cellClass: 'whitespace-normal', exportable: false },
      size: 160,
    }),
    providerModelColumnHelper.accessor('source_kind', {
      header: () => m.provider_model_catalog_how_models_were_added(),
      cell: (context) => renderSnippet(providerModelSourceCell, context),
      enableSorting: false,
      enableGlobalFilter: false,
      meta: {
        label: () => m.provider_model_catalog_how_models_were_added(),
        filter: { variant: 'select', ...filterOptions.source },
      },
      size: 160,
    }),

    providerModelColumnHelper.accessor(
      (model) => (modelReferences(model.id).length > 0 ? 'referenced' : 'unreferenced'),
      {
        id: 'usage',
        header: () => m.provider_model_catalog_model_usage(),
        cell: (context) => renderSnippet(providerModelUsageCell, context),
        enableSorting: false,
        enableGlobalFilter: false,
        meta: {
          label: () => m.provider_model_catalog_model_usage(),
          cellClass: 'whitespace-normal',
          filter: { variant: 'select', ...filterOptions.reference },
        },
        size: 190,
      },
    ),
  ]),
)

function getProviderModelRowId(model: ProviderModelSummary): string {
  return model.id
}

$effect(() => {
  void editing.select(providerId, requestedModelId)
})

beforeNavigate((navigation) => {
  if (navigation.shallow && navigation.type === 'goto') return
  if (internalNavigation) return
  if (!modelEditing.busy && !modelEditing.dirty) return

  navigation.cancel()

  // SvelteKit 对卸载取消使用原生提示；页内导航复用同一丢弃确认。
  if (navigation.willUnload || modelEditing.busy || !navigation.to) return
  const { pathname, search, hash } = navigation.to.url
  const destination = resolve(`${pathname.slice(resolve('').length)}${search}${hash}` as Path)
  const delta = navigation.type === 'popstate' ? navigation.delta : null
  editing.requestLeave(() => {
    if (delta != null) window.history.go(delta)
    else return goto(destination)
  })
})

onDestroy(() => {
  preferenceEpoch += 1
  editing.dispose()
})

function modelReferences(modelId: string): Array<{ route: Route; target: Route['targets'][number] }> {
  return routes.flatMap((route) =>
    route.targets
      .filter((target) => target.provider_id === providerId && target.model === modelId)
      .map((target) => ({ route, target })),
  )
}

function routeForModel(modelId: string): Route | undefined {
  return routes.find((route) => route.model_id === modelId)
}

function catalogFilterValue<TValue extends string>(columnId: string, fallback: TValue): TValue {
  const value = columnFilters.find((filter) => filter.id === columnId)?.value
  const candidate =
    value && typeof value === 'object' && 'constraints' in value && Array.isArray(value.constraints)
      ? (value as DataTableFilterGroup).constraints[0]?.value
      : value

  return typeof candidate === 'string' ? (candidate as TValue) : fallback
}

function setCatalogFilter(columnId: string, value: string, emptyValue = allCatalogFilterValue): void {
  const remaining = columnFilters.filter((filter) => filter.id !== columnId)
  columnFilters =
    value === emptyValue
      ? remaining
      : [
          ...remaining,
          {
            id: columnId,
            value: { operator: 'and', constraints: [{ value, matchMode: 'equals' }] } satisfies DataTableFilterGroup,
          },
        ]
}

function clearFilters(): void {
  search = ''
  columnFilters = []
}

function setSpecificationFilter(value: SpecificationFilter): void {
  const remaining = columnFilters.filter((filter) => filter.id !== 'specification')
  columnFilters = specificationFilterCount(value) ? [...remaining, { id: 'specification', value }] : remaining
}

function modelEditorSearch(modelId: string): string {
  const search = new SvelteURLSearchParams(page.url.search)
  search.set('view', 'models')
  search.set('model', modelId)
  return search.toString()
}

function openProviderModel(model: ProviderModelSummary, event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  void goto(resolve(`providers/${encodeURIComponent(providerId)}?${modelEditorSearch(model.id)}`))
}

function handleProviderModelTableRowClick({ event, original }: DataTableRowPointerEvent<ProviderModelSummary>): void {
  openProviderModel(original, event)
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

async function updateModelQuery(modelId?: string): Promise<void> {
  const search = new SvelteURLSearchParams(page.url.search)
  if (modelId) search.set('model', modelId)
  else search.delete('model')
  internalNavigation = true
  try {
    await goto(resolve(`providers/${encodeURIComponent(providerId)}?${search}`), { replace: true, reset: false })
  } finally {
    internalNavigation = false
  }
}

async function refreshModelData(operation: ProviderModelEditingOperation): Promise<void> {
  const requests: Array<Promise<unknown>> = [modelsQuery.refetch({ throwOnError: true })]
  if (operation !== 'reimport') {
    requests.push(queryClient.invalidateQueries({ queryKey: ['models'] }, { throwOnError: true }))
  }
  if (operation === 'save') {
    requests.push(queryClient.invalidateQueries({ queryKey: ['providers'] }, { throwOnError: true }))
  }
  // 等所有关联读取收口再解锁，首个失败不能留下未观察的刷新任务。
  const results = await Promise.allSettled(requests)
  const failure = results.find((result) => result.status === 'rejected')
  if (failure?.status === 'rejected') throw failure.reason
}

async function syncModels(id = providerId): Promise<void> {
  const sync = onSync
  if (sync) {
    if (id === providerId) syncing = true
    try {
      const summary = await sync(id)
      if (!summary) return
      if (id === providerId) {
        await modelsQuery.refetch()
        await syncStateQuery.refetch()
      }
    } finally {
      if (id === providerId) syncing = false
    }
    return
  }
  if (id === providerId) syncing = true
  const retry = () => {
    if (id === providerId) editing.requestLeave(() => syncModels(id))
    else void syncModels(id)
  }
  try {
    const summary = await admin.providers.syncModels(id)
    syncToast(summary, changesSheet.reveal, retry)
    await queryClient.invalidateQueries({ queryKey: ['provider-models', id] })
    await queryClient.invalidateQueries({ queryKey: ['provider-model-sync-state', id] })
  } catch (error) {
    syncErrorToast(error, changesSheet.reveal, retry)
    await queryClient.invalidateQueries({ queryKey: ['provider-model-sync-state', id] })
  } finally {
    if (id === providerId) syncing = false
  }
}

function requestSync(): void {
  const id = providerId
  editing.requestLeave(() => syncModels(id))
}

async function prepareManualModel(): Promise<void> {
  const templateId = manualTemplateId.trim()
  if (!templateId) return
  if (
    await editing.prepareManual(
      providerId,
      templateId,
      models.map((model) => model.id),
    )
  ) {
    manualOpen = false
    manualTemplateId = ''
  }
}

function selectManualTemplate(templateId: string): void {
  manualTemplateId = templateId
}

async function saveModelDetail(): Promise<void> {
  // 只改了 RPM 时不重写模型规格，避免无意义地推进规格修订号。
  if (specificationDirty || !rpmDirty) editor?.submit()
  if (!rpmDirty || !rpmForm) return
  rpmSaving = true
  try {
    await rpmForm.save()
  } finally {
    rpmSaving = false
  }
}

function clearManualTemplate(): void {
  manualTemplateId = ''
}
</script>

{#snippet providerModelIdentityCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@const model = context.row.original}
  <div class="w-full min-w-0 text-left" aria-label={`${model.name} ${model.id}`}>
    <span class="block truncate font-medium">{model.name}</span>
    <span class="block truncate font-technical text-xs text-muted-foreground">{model.id}</span>
  </div>
{/snippet}

{#snippet providerModelSpecificationFilter(value: unknown, onChange: (value: unknown) => void)}
  <ModelSpecificationFilter
    value={(value as SpecificationFilter | undefined) ?? emptySpecificationFilter}
    onChange={(next: SpecificationFilter) => onChange(specificationFilterCount(next) ? next : undefined)} />
{/snippet}

{#snippet providerModelSpecificationCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@render modelContext(context.row.original)}
{/snippet}

{#snippet modelContext(model: ProviderModelSummary, compact = false)}
  {@const value = model.specification.limit?.context}

  <ModelCellEditor {providerId} {model} field="context" {compact}>
    <span class="font-technical text-sm">
      {value == null ? m.model_specification_not_registered() : formatSpecificationTokens(value)}
    </span>
  </ModelCellEditor>
{/snippet}

{#snippet providerModelModalitiesCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@render modelModalities(context.row.original)}
{/snippet}

{#snippet modelModalities(model: ProviderModelSummary, compact = false)}
  {@const modalities = model.specification.modalities}

  <ModelCellEditor {providerId} {model} field="modalities" {compact}>
    <span class={['flex gap-0.5 text-xs', compact ? 'flex-wrap gap-x-3' : 'flex-col']}>
      <span role="group" aria-label={m.model_specification_input()} class="flex items-center gap-1.5">
        <span class="text-muted-foreground">{m.model_specification_input()}</span>
        <ModalityIcons values={modalities?.input} tooltip={false} />
      </span>
      <span role="group" aria-label={m.model_specification_output()} class="flex items-center gap-1.5">
        <span class="text-muted-foreground">{m.model_specification_output()}</span>
        <ModalityIcons values={modalities?.output} tooltip={false} />
      </span>
    </span>
  </ModelCellEditor>
{/snippet}

{#snippet providerModelEffortsCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@render modelEfforts(context.row.original)}
{/snippet}

{#snippet modelEfforts(model: ProviderModelSummary, compact = false)}
  {@const efforts = model.specification.reasoning_efforts}

  <ModelCellEditor {providerId} {model} field="efforts" {compact}>
    <span class="break-words font-technical text-xs">
      {efforts?.length ? efforts.join(', ') : m.model_specification_not_registered()}
    </span>
  </ModelCellEditor>
{/snippet}

{#snippet providerModelAvailabilityCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@render modelAvailability(context.row.original)}
{/snippet}

{#snippet modelAvailability(model: ProviderModelSummary, compact = false)}
  {@const reason = availabilityReason(model)}

  <ModelCellEditor {providerId} {model} field="availability" {compact}>
    <Badge variant={model.available ? 'secondary' : 'outline'}>
      {model.available ? m.model_specification_available() : m.common_unavailable()}
    </Badge>
  </ModelCellEditor>
  {#if reason}<p class="mt-1 text-xs text-muted-foreground">{reason}</p>{/if}
{/snippet}

{#snippet providerModelRpmCell(context: DataTableCellContext<ProviderModelSummary>)}
  <RpmLimit {providerId} modelId={context.row.original.id} modelName={context.row.original.name} />
{/snippet}

{#snippet providerModelSourceCell(context: DataTableCellContext<ProviderModelSummary>)}
  <Badge variant="outline">
    {context.row.original.source_kind === 'manual' ? m.common_added_manually() : m.common_synced()}
  </Badge>
  {#if context.row.original.snapshot_state.type === 'unregistered'}
    <p class="mt-1 text-xs text-muted-foreground">{m.model_specification_not_registered()}</p>
  {/if}
{/snippet}

{#snippet providerModelUsageCell(context: DataTableCellContext<ProviderModelSummary>)}
  {@const model = context.row.original}
  {@const references = modelReferences(model.id)}
  {@const matchingRoute = routeForModel(model.id)}
  {#if references.length > 0}
    <a
      class="inline-flex w-fit items-center rounded-md px-2 py-1 text-sm font-medium hover:bg-muted"
      href={resolve(`providers/${encodeURIComponent(providerId)}?view=routes`)}
      >{m.provider_model_catalog_used_by_models({ count: references.length })}</a>
  {:else if routeReferencesReady}
    <button
      type="button"
      class="inline-flex w-fit items-center rounded-md px-2 py-1 text-sm font-medium text-foreground hover:bg-muted disabled:pointer-events-none disabled:opacity-70"
      aria-label={m.provider_model_catalog_add_model_to_route({ id: model.id })}
      disabled={!model.available || Boolean(addingRouteModelId)}
      onclick={() => void addModelToRoute(model)}>
      {#if addingRouteModelId === model.id}<Spinner data-icon="inline-start" />{/if}
      {matchingRoute ? m.provider_model_catalog_add_destination() : m.provider_model_catalog_create_model()}
    </button>
  {:else}
    <span class="px-2 text-sm text-muted-foreground">{m.provider_model_catalog_not_used()}</span>
  {/if}
{/snippet}

{#snippet providerModelsLoading()}
  <div class="grid min-h-56 place-items-center"><Spinner /></div>
{/snippet}

{#snippet providerModelsEmpty()}
  {#if modelsQuery.isError && !modelsQuery.data}
    <RequestFailure
      message={localizeBackendErrorMessage(modelsQuery.error)}
      retry={() => modelsQuery.refetch()}
      retrying={modelsQuery.isFetching} />
  {:else}
    <Empty.Root>
      <Empty.Header>
        <Empty.Description
          >{models.length === 0
            ? m.provider_model_catalog_no_models_available()
            : m.provider_model_catalog_no_models_match_filters()}</Empty.Description>
      </Empty.Header>
      <Empty.Content>
        {#if models.length === 0}
          <Button variant="outline" onclick={() => (manualOpen = true)}
            >{m.provider_model_catalog_add_model_manually()}</Button>
          <Button variant="outline" onclick={requestSync} disabled={syncing}
            >{m.provider_model_catalog_sync_models()}</Button>
        {:else if hasActiveFilters}
          <Button size="sm" variant="outline" onclick={clearFilters}>{m.provider_model_catalog_clear_filters()}</Button>
        {/if}
      </Empty.Content>
    </Empty.Root>
  {/if}
{/snippet}

{#if modelEditing.refreshError}
  <RequestFailure
    message={m.provider_model_catalog_saved_refresh_failed()}
    retry={() => editing.refresh()}
    retrying={modelEditing.busy}>
    <p class="text-sm text-muted-foreground">{localizeBackendErrorMessage(modelEditing.refreshError)}</p>
  </RequestFailure>
{/if}

{#if (modelEditing.modelId || requestedModelId) && !modelEditing.draft}
  <section class="route-section" aria-labelledby="provider-model-editor-title">
    {#if modelEditing.loading || modelEditing.preparing || modelsQuery.isPending}
      <div class="grid min-h-72 place-items-center"><Spinner /></div>
    {:else if modelEditing.readError || !modelEditing.detail}
      <RequestFailure
        message={modelEditing.readError
          ? localizeBackendErrorMessage(modelEditing.readError)
          : m.backend_error_catalog_model_not_found()}
        retry={modelEditing.readError ? () => editing.retry() : undefined}
        retrying={modelEditing.loading}>
        <Button variant="outline" onclick={() => editing.close()} disabled={modelEditing.busy}
          >{m.common_cancel()}</Button>
      </RequestFailure>
    {:else}
      <div class="route-section-header">
        <div class="min-w-0">
          <div class="flex flex-wrap items-center gap-2">
            <h2 id="provider-model-editor-title" class="route-section-title break-all text-balance">
              {modelEditing.detail.metadata.name || modelEditing.detail.id}
            </h2>
            <Badge variant={modelEditing.detail.available ? 'secondary' : 'outline'}>
              {modelEditing.detail.available ? m.model_specification_available() : m.common_unavailable()}
            </Badge>
            <Badge variant="outline">
              {modelEditing.detail.source_kind === 'manual' ? m.common_added_manually() : m.common_synced()}
            </Badge>
          </div>
          <TechnicalValue
            value={modelEditing.detail.id}
            copyable
            class="max-w-[min(40rem,100%)] text-[0.8125rem] text-muted-foreground" />
        </div>
        <DropdownMenu.Root>
          <DropdownMenu.Trigger>
            {#snippet child({ props })}
              <Button
                {...props}
                variant="ghost"
                size="icon"
                disabled={modelEditing.busy}
                aria-label={m.provider_model_catalog_model_actions()}><MoreHorizontalIcon /></Button>
            {/snippet}
          </DropdownMenu.Trigger>
          <DropdownMenu.Content align="end" class="w-max min-w-48 max-w-[calc(100vw-2rem)]">
            <DropdownMenu.Group>
              <DropdownMenu.Item
                onSelect={() =>
                  void goto(
                    resolve(
                      `models/new?provider=${encodeURIComponent(providerId)}&model=${encodeURIComponent(modelEditing.detail!.id)}`,
                    ),
                  )}>{m.provider_model_catalog_use_new_model()}</DropdownMenu.Item>

              {#if modelEditing.detail.can_reimport}
                <DropdownMenu.Item onSelect={() => editing.requestReimport()}
                  >{m.provider_model_catalog_restore_details_service()}</DropdownMenu.Item>
              {:else}
                <DropdownMenu.Separator />
                <DropdownMenu.Item variant="destructive" onSelect={() => editing.requestDelete()}
                  >{m.provider_model_catalog_remove_manually_added_model()}</DropdownMenu.Item>
              {/if}
            </DropdownMenu.Group>
          </DropdownMenu.Content>
        </DropdownMenu.Root>
      </div>
      <div>
        <ProviderModelEditor
          bind:this={editor}
          detail={modelEditing.detail}
          draft={modelEditing.draft}
          disabled={modelEditing.busy}
          onSave={(metadataJson: string) => void editing.save(metadataJson)}
          onSelectionChange={(policy: ProviderModelSelectionPolicy) => void editing.changeSelection(policy)}
          onDirtyChange={(value: boolean) => {
            specificationDirty = value
            editing.markDirty(value || rpmDirty)
          }} />
      </div>
      {#key modelEditing.detail.id}
        <FormSection title={m.rpm_column()} class="mt-6">
          <RpmForm
            bind:this={rpmForm}
            {providerId}
            modelId={modelEditing.detail.id}
            embedded
            onDirtyChange={(value: boolean) => {
              rpmDirty = value
              editing.markDirty(value || specificationDirty)
            }} />
        </FormSection>
      {/key}
      <div
        class="sticky bottom-0 z-20 mt-6 flex translate-y-2 flex-wrap items-center justify-end gap-x-4 gap-y-2 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
        {#if modelEditing.dirty}
          <StatusIndicator tone="warning" label={m.provider_model_editor_unsaved()} class="me-auto" />
        {/if}
        <div class="flex gap-2">
          <Button variant="outline" onclick={() => editing.close()} disabled={modelEditing.busy || rpmSaving}
            >{m.common_cancel()}</Button>
          <Button
            onclick={saveModelDetail}
            disabled={modelEditing.busy || rpmSaving}
            aria-busy={modelEditing.busy || rpmSaving}>
            {#if modelEditing.busy || rpmSaving}<Spinner data-icon="inline-start" />{/if}{m.common_save_model()}
          </Button>
        </div>
      </div>
    {/if}
  </section>
{:else}
  <section class="route-section" aria-labelledby="provider-model-inventory-title">
    <h2 id="provider-model-inventory-title" class="sr-only">{m.provider_model_catalog_models_service()}</h2>
    <!-- 工具栏固定两行：第一行是操作与偏好（自动同步、变更记录、手动添加、同步），第二行是上次检查结果与进行中／暂停状态。 -->
    <div class="mb-3 flex flex-col gap-2" data-testid="model-inventory-toolbar">
      <div class="flex flex-wrap items-center gap-x-4 gap-y-2" data-toolbar-row="actions">
        {#if syncStateQuery.data}
          <div class="flex items-center gap-3 text-sm">
            <Switch
              id="automatic-model-sync"
              checked={syncStateQuery.data.automatic_sync}
              disabled={preferenceSaving}
              onCheckedChange={(value: boolean) => void saveAutomaticSync(value)} />
            <label for="automatic-model-sync" class="font-medium">{m.model_changes_automatic()}</label>
            {#if preferenceSaving}<Spinner />{/if}
          </div>
        {/if}
        <div class="ms-auto flex flex-wrap gap-2">
          <Button data-model-changes-trigger variant="outline" onclick={() => changesSheet.show(providerId)}
            >{m.model_changes_title()}</Button>

          <Button variant="outline" onclick={() => (manualOpen = true)}>
            <PlusIcon data-icon="inline-start" />
            {m.provider_model_catalog_add_model_manually()}
          </Button>

          <Button variant="outline" onclick={requestSync} disabled={syncing}>
            {#if syncing}
              <Spinner data-icon="inline-start" />
            {:else}
              <RefreshCwIcon data-icon="inline-start" />
            {/if}

            {m.provider_model_catalog_sync_models()}
          </Button>
        </div>
      </div>
      {#if displayedSyncSummary || syncStateQuery.isPending || syncStateQuery.data?.in_progress || (syncStateQuery.data?.automatic_sync && syncStateQuery.data.pause_reason)}
        <div class="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm" data-toolbar-row="status">
          {#if displayedSyncSummary}
            <p class="min-w-0 text-muted-foreground">
              {m.provider_model_catalog_last_checked()}
              <time class="font-technical text-xs" datetime={displayedSyncSummary.checked_at}
                >{formatLogTime(displayedSyncSummary.checked_at)}</time>
              · {sourceLabel(displayedSyncSummary.source)}
              {#if summaryDescription(displayedSyncSummary)}
                · <span class="text-foreground">{summaryDescription(displayedSyncSummary)}</span>{/if}
              {#if displayedSyncSummary.error}
                · <span class="text-destructive">{modelChangeReason(displayedSyncSummary.error)}</span>{/if}
            </p>
          {:else if syncStateQuery.isPending}
            <Spinner />
          {/if}
          {#if syncStateQuery.data?.in_progress}<p role="status" class="flex items-center gap-2 text-muted-foreground">
              <Spinner />{m.model_changes_in_progress()}
            </p>{/if}
          {#if syncStateQuery.data?.automatic_sync && syncStateQuery.data.pause_reason}<p class="text-warning">
              {modelChangeReason(syncStateQuery.data.pause_reason)}
            </p>{/if}
        </div>
      {/if}
    </div>

    {#if preferenceError}<RequestFailure
        message={preferenceError}
        retry={() => saveAutomaticSync(!syncStateQuery.data?.automatic_sync)} />{/if}
    {#if syncStateQuery.isError}<RequestFailure
        message={localizeBackendErrorMessage(syncStateQuery.error)}
        retry={() => syncStateQuery.refetch()} />{/if}

    {#if modelsQuery.isError && modelsQuery.data}
      <RequestFailure
        message={localizeBackendErrorMessage(modelsQuery.error)}
        retry={() => modelsQuery.refetch()}
        retrying={modelsQuery.isFetching} />
    {/if}

    <div class="route-desktop-table">
      <DataTable
        data={models}
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
        <InputGroup.Root>
          <InputGroup.Input
            id="provider-model-search-mobile"
            aria-label={m.provider_model_catalog_search_models()}
            bind:value={search}
            placeholder={m.provider_model_catalog_search_name_model_id()} />
          <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
        </InputGroup.Root>

        <div class="mt-2 flex flex-wrap items-center justify-between gap-2">
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
      {:else if (modelsQuery.isError && !modelsQuery.data) || filteredModels.length === 0}
        {@render providerModelsEmpty()}
      {:else}
        {#each filteredModels as model (model.id)}
          {@const references = modelReferences(model.id)}
          {@const matchingRoute = routeForModel(model.id)}
          <!-- 紧凑行：身份与「添加／使用」动作成组靠前，其余规格用带标签的 dl，每项保留可见的编辑入口。 -->
          <article class="grid min-w-0 gap-2 border-b py-3 first:border-t" aria-label={`${model.name} ${model.id}`}>
            <div class="flex min-w-0 items-start justify-between gap-3">
              <a
                class="min-w-0 text-left"
                href={resolve(`providers/${encodeURIComponent(providerId)}?${modelEditorSearch(model.id)}`)}
                aria-label={`${model.name} ${model.id}`}>
                <span class="block truncate font-medium">{model.name}</span>
                <span class="block truncate font-technical text-xs text-muted-foreground">{model.id}</span>
              </a>
              <div class="flex shrink-0 items-center">
                {#if references.length > 0}
                  <a
                    class="inline-flex items-center rounded-md px-2 py-1 text-sm font-medium hover:bg-muted"
                    href={resolve(`providers/${encodeURIComponent(providerId)}?view=routes`)}
                    >{m.provider_model_catalog_used_by_models({ count: references.length })}</a>
                {:else if routeReferencesReady}
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    aria-label={m.provider_model_catalog_add_model_to_route({ id: model.id })}
                    disabled={!model.available || Boolean(addingRouteModelId)}
                    onclick={() => void addModelToRoute(model)}>
                    {#if addingRouteModelId === model.id}<Spinner data-icon="inline-start" />{/if}
                    {matchingRoute
                      ? m.provider_model_catalog_add_destination()
                      : m.provider_model_catalog_create_model()}
                  </Button>
                {:else}
                  <span class="px-2 text-sm text-muted-foreground">{m.provider_model_catalog_not_used()}</span>
                {/if}
              </div>
            </div>
            <dl class="grid min-w-0 grid-cols-2 gap-x-3 gap-y-1 text-sm">
              <div class="flex min-w-0 items-center gap-1.5">
                <dt class="shrink-0 text-xs text-muted-foreground">{m.model_specification_context()}</dt>
                <dd class="min-w-0 flex-1">{@render modelContext(model, true)}</dd>
              </div>
              <div class="flex min-w-0 items-center gap-1.5">
                <dt class="shrink-0 text-xs text-muted-foreground">{m.rpm_column()}</dt>
                <dd class="min-w-0 flex-1">
                  <RpmLimit {providerId} modelId={model.id} modelName={model.name} compact />
                </dd>
              </div>
              <div class="col-span-2 flex min-w-0 items-center gap-1.5">
                <dt class="shrink-0 text-xs text-muted-foreground">{m.model_specification_modalities()}</dt>
                <dd class="min-w-0 flex-1">{@render modelModalities(model, true)}</dd>
              </div>
              <div class="col-span-2 flex min-w-0 items-center gap-1.5">
                <dt class="shrink-0 text-xs text-muted-foreground">{m.model_specification_reasoning_efforts()}</dt>
                <dd class="min-w-0 flex-1">{@render modelEfforts(model, true)}</dd>
              </div>
              <div class="col-span-2 flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
                <div class="flex min-w-0 items-center gap-1.5">
                  <dt class="shrink-0 text-xs text-muted-foreground">
                    {m.provider_model_catalog_model_availability()}
                  </dt>
                  <dd class="min-w-0">{@render modelAvailability(model, true)}</dd>
                </div>
                <div class="min-w-0">
                  <dt class="sr-only">{m.provider_model_catalog_how_models_were_added()}</dt>
                  <dd>
                    <Badge variant="outline"
                      >{model.source_kind === 'manual' ? m.common_added_manually() : m.common_synced()}</Badge>
                  </dd>
                </div>
              </div>
            </dl>
          </article>
        {/each}
      {/if}
    </div>
  </section>
{/if}

<CatalogFilterSheet
  bind:open={filtersOpen}
  availability={availabilityFilter}
  source={sourceFilter}
  reference={referenceFilter}
  specification={specificationFilter}
  onSpecificationChange={setSpecificationFilter}
  onFilterChange={setCatalogFilter}
  onClear={clearFilters} />

<ManualModelDialog
  open={manualOpen}
  onOpenChange={(open: boolean) => {
    manualOpen = open
    if (!open && modelEditing.preparing) editing.close()
  }}
  bind:templateId={manualTemplateId}
  models={canonicalModels}
  modelsPending={canonicalModelsQuery.isPending}
  preparing={modelEditing.preparing}
  onSelect={selectManualTemplate}
  onClear={clearManualTemplate}
  onContinue={() => void prepareManualModel()} />

<CatalogEditorDrawer
  open={modelEditing.drawerOpen}
  detail={modelEditing.detail}
  draft={modelEditing.draft}
  loading={modelEditing.loading}
  saving={modelEditing.busy}
  onOpenChange={(open: boolean) => editing.setDrawerOpen(open)}
  onClose={() => editing.close()}
  onSave={(metadataJson: string) => void editing.save(metadataJson)}
  onSelectionChange={(policy: ProviderModelSelectionPolicy) => void editing.changeSelection(policy)}
  onDirtyChange={(value: boolean) => editing.markDirty(value)} />

<CatalogConfirmations
  discardOpen={modelEditing.discardOpen}
  deleteOpen={modelEditing.deleteOpen}
  detail={modelEditing.detail}
  references={selectedReferences}
  {routeReferencesReady}
  saving={modelEditing.busy}
  onKeepEditing={() => editing.keepEditing()}
  onDiscard={() => void editing.confirmDiscard()}
  onDeleteOpenChange={(open: boolean) => editing.setDeleteOpen(open)}
  onDelete={() => void editing.delete()} />

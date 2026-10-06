<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet } from '@tanstack/svelte-table'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import PlusIcon from '@lucide/svelte/icons/plus'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { getDataTableLabels } from '$lib/data-table-labels'
import { effectiveModelDisplayName, sortLogicalModels } from '$lib/logical-model'
import { formatSpecificationTokens } from '$lib/model-specification'
import {
  formatRouteDestinations,
  routeDestinationSeverity,
  summarizeRouteDestinations,
  type RouteDestinationSummary,
} from '$lib/route-destinations'
import { cn } from '$lib/utils'
import type { Route } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import TechnicalValue from '$lib/components/technical-value.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Button } from '$lib/components/ui/button'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRowPointerEvent,
} from '$lib/components/ui/data-table'
import * as DropdownMenu from '$lib/components/ui/dropdown-menu'
import * as Empty from '$lib/components/ui/empty'
import { Skeleton } from '$lib/components/ui/skeleton'

const queryClient = useQueryClient()
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const apiKeysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const webSearchQuery = createQuery(() => ({ queryKey: ['web-search-config'], queryFn: admin.webSearch.config.get }))
const mediaUnderstandingQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))
let deleteTarget = $state<Route>()
let deleteOpen = $state(false)
let actingModelId = $state<string>()

const models = $derived(sortLogicalModels(modelsQuery.data ?? []))
const providers = $derived(providersQuery.data ?? [])
// 先读取全部查询状态，避免短路跳过订阅后错过先完成的依赖。
const resourcesPending = $derived.by(() => {
  const models = modelsQuery.isPending
  const providers = providersQuery.isPending
  return models || providers
})
const resourcesFetching = $derived.by(() => {
  const models = modelsQuery.isFetching
  const providers = providersQuery.isFetching
  return models || providers
})
const resourceError = $derived.by(() => {
  const models = modelsQuery.error
  const providers = providersQuery.error
  return models ?? providers
})
const apiKeys = $derived(apiKeysQuery.data ?? [])
const tableLabels = $derived(getDataTableLabels())
const modelColumnHelper = createDataTableColumnHelper<Route>()

const modelColumns = modelColumnHelper.columns([
  modelColumnHelper.accessor((model) => effectiveModelDisplayName(model), {
    id: 'display-name',
    header: () => m.models_display_name(),
    cell: (context) => renderSnippet(modelDisplayNameCell, context),
    meta: { label: () => m.models_display_name() },
    size: 180,
  }),
  modelColumnHelper.accessor('model_id', {
    header: () => m.models_client_model_id(),
    cell: (context) => renderSnippet(modelIdCell, context),
    meta: { label: () => m.models_client_model_id() },
    size: 190,
  }),
  modelColumnHelper.accessor('context_window', {
    header: () => m.model_specification_context(),
    cell: (context) => renderSnippet(modelContextCell, context),
    meta: { label: () => m.model_specification_context(), align: 'end' },
    size: 130,
  }),
  // 按严重度排序：表格默认顺序已按名称排好，稳定排序使同级保持名称顺序。
  modelColumnHelper.accessor((model) => routeDestinationSeverity(destinationSummary(model)), {
    id: 'destinations',
    header: () => m.models_destinations(),
    cell: (context) => renderSnippet(modelDestinationsCell, context),
    meta: { label: () => m.models_destinations() },
    size: 280,
  }),
  modelColumnHelper.accessor('is_enabled', {
    header: () => m.common_status(),
    cell: (context) => renderSnippet(modelStatusCell, context),
    meta: { label: () => m.common_status() },
    size: 130,
  }),
  modelColumnHelper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(modelActionsCell, context),
    enableHiding: false,
    enableSorting: false,
    meta: { label: () => m.common_actions(), align: 'end', exportable: false },
    size: 64,
  }),
])

function getModelRowId(model: Route): string {
  return model.id
}
const routeDependenciesUnavailable = $derived(
  apiKeysQuery.isPending ||
    apiKeysQuery.isError ||
    webSearchQuery.isPending ||
    webSearchQuery.isError ||
    mediaUnderstandingQuery.isPending ||
    mediaUnderstandingQuery.isError,
)
const deleteApiKeyReferences = $derived(
  deleteTarget ? apiKeys.filter((apiKey) => apiKey.model_ids.includes(deleteTarget!.id)) : [],
)
const deletesWebSearchRoute = $derived(
  Boolean(
    deleteTarget &&
    webSearchQuery.data?.backend?.kind === 'local' &&
    webSearchQuery.data.backend.model_id === deleteTarget.id,
  ),
)
const deletesMediaUnderstandingRoute = $derived(
  Boolean(deleteTarget && mediaUnderstandingQuery.data?.model_id === deleteTarget.id),
)

function destinationSummary(model: Route): RouteDestinationSummary {
  return summarizeRouteDestinations(model, providers)
}

function openModel(model: Route, event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  void goto(resolve(`/models/${encodeURIComponent(model.model_id)}`))
}

function handleModelTableRowClick({ event, original }: DataTableRowPointerEvent<Route>): void {
  openModel(original, event)
}

function askDelete(model: Route): void {
  deleteTarget = model
  deleteOpen = true
}

async function toggleModel(model: Route): Promise<void> {
  actingModelId = model.id
  try {
    await admin.models.update(model.model_id, { is_enabled: !model.is_enabled })
    await queryClient.invalidateQueries({ queryKey: ['models'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingModelId = undefined
  }
}

async function deleteModel(): Promise<void> {
  if (!deleteTarget) return
  actingModelId = deleteTarget.id
  try {
    await admin.models.delete(deleteTarget.model_id)
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['models'] }),
      queryClient.invalidateQueries({ queryKey: ['api-keys'] }),
      queryClient.invalidateQueries({ queryKey: ['web-search-config'] }),
      queryClient.invalidateQueries({ queryKey: ['media-understanding-config'] }),
    ])
    deleteOpen = false
    deleteTarget = undefined
    toast.success(m.models_model_deleted())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingModelId = undefined
  }
}
</script>

<svelte:head><title>{m.common_models()} · Stravia</title></svelte:head>

{#snippet addModelAction()}
  <Button href="/models/new" disabled={providers.length === 0 || providersQuery.isPending}>
    <PlusIcon data-icon="inline-start" />{m.common_add_model()}
  </Button>
{/snippet}

{#snippet modelActions(model: Route)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger>
      {#snippet child({ props })}
        <Button
          {...props}
          size="icon"
          class="size-10"
          variant="ghost"
          aria-label={m.models_more_actions_value({ name: effectiveModelDisplayName(model) })}>
          <MoreHorizontalIcon />
        </Button>
      {/snippet}
    </DropdownMenu.Trigger>
    <DropdownMenu.Content class="w-48" align="end">
      <DropdownMenu.Group>
        <DropdownMenu.Item onSelect={() => void toggleModel(model)} disabled={actingModelId === model.id}>
          {model.is_enabled ? m.models_disable_model() : m.models_enable_model()}
        </DropdownMenu.Item>
      </DropdownMenu.Group>
      <DropdownMenu.Separator />
      <DropdownMenu.Group
        ><DropdownMenu.Item variant="destructive" onSelect={() => askDelete(model)}
          >{m.models_delete_model()}</DropdownMenu.Item
        ></DropdownMenu.Group>
    </DropdownMenu.Content>
  </DropdownMenu.Root>
{/snippet}

{#snippet modelDisplayNameCell(context: DataTableCellContext<Route>)}
  <span class="block truncate font-medium">{effectiveModelDisplayName(context.row.original)}</span>
{/snippet}

{#snippet modelIdCell(context: DataTableCellContext<Route>)}
  <TechnicalValue value={context.row.original.model_id} copyable />
{/snippet}

{#snippet modelContextCell(context: DataTableCellContext<Route>)}
  {@const value = context.row.original.context_window}
  <span class="font-technical tabular-nums">
    {value == null ? m.model_specification_not_registered() : formatSpecificationTokens(value)}
  </span>
{/snippet}

{#snippet modelDestinations(model: Route)}
  {@const summary = destinationSummary(model)}
  {@const text = formatRouteDestinations(summary)}
  <div class="min-w-0">
    <span
      class={cn('block truncate', !summary.preferred && 'text-warning', !model.is_enabled && 'text-muted-foreground')}
      >{text.primary}</span>
    {#if text.details.length > 0}
      <span class="block truncate text-xs text-muted-foreground">
        {#each text.details as detail, index (detail.text)}
          {#if index > 0}<span aria-hidden="true"> · </span>{/if}<span
            class={cn(detail.tone === 'warning' && 'text-warning')}>{detail.text}</span>
        {/each}
      </span>
    {/if}
  </div>
{/snippet}

{#snippet modelDestinationsCell(context: DataTableCellContext<Route>)}
  {@render modelDestinations(context.row.original)}
{/snippet}

{#snippet modelStatusCell(context: DataTableCellContext<Route>)}
  {@const model = context.row.original}
  <StatusIndicator
    compact
    label={model.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
    tone={model.is_enabled ? 'healthy' : 'neutral'} />
{/snippet}

{#snippet modelActionsCell(context: DataTableCellContext<Route>)}
  <div class="flex justify-end gap-1">{@render modelActions(context.row.original)}</div>
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.common_setup()}
    title={m.common_models()}
    description={m.models_give_apps_stable_model_names_choose_which_connected()}
    actions={models.length > 0 ? addModelAction : undefined} />

  <section class="route-section" aria-labelledby="model-route-table-title">
    <h2 id="model-route-table-title" class="sr-only">{m.models_configured_models()}</h2>

    {#if resourceError}
      <RequestFailure
        title={m.models_models_not_loaded()}
        message={localizeBackendErrorMessage(resourceError)}
        retry={() => Promise.all([modelsQuery.refetch(), providersQuery.refetch()])}
        retrying={resourcesFetching} />
    {/if}

    {#if resourcesPending}
      <div class="flex flex-col border-y" aria-label={m.models_loading_models()}>
        {#each Array(5) as _, index (index)}<div
            class="grid grid-cols-[2fr_1fr_3fr_1fr] gap-4 border-b p-3 last:border-b-0">
            <Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" />
          </div>{/each}
      </div>
    {:else if modelsQuery.data !== undefined && providersQuery.data !== undefined && models.length === 0}
      <Empty.Root class="border-y py-10">
        <Empty.Header
          ><Empty.Title
            >{providers.length === 0
              ? m.models_connect_model_service_first()
              : m.models_no_models_on_service({ name: providers[0]?.name ?? m.common_model_service() })}</Empty.Title
          ><Empty.Description
            >{providers.length === 0
              ? m.models_connect_ai_service_adding_model()
              : m.models_add_model_name_apps_request_choose_where_requests()}</Empty.Description
          ></Empty.Header>
        <Empty.Content
          >{#if providers.length === 0}<Button href="/providers">{m.models_go_model_services()}</Button>{:else}<Button
              href="/models/new">{m.models_add_first_model()}</Button
            >{/if}</Empty.Content>
      </Empty.Root>
    {:else if modelsQuery.data !== undefined && providersQuery.data !== undefined}
      <div class="route-desktop-table">
        <DataTable
          data={models}
          columns={modelColumns}
          labels={tableLabels}
          getRowId={getModelRowId}
          ariaLabel={m.models_configured_models()}
          size="small"
          stripedRows
          sortMode="multiple"
          resizableColumns
          onRowClick={handleModelTableRowClick} />
      </div>
      <div class="route-mobile-list">
        {#each models as model (model.id)}
          <div class="route-mobile-row">
            <div class="min-w-0">
              <a class="block truncate font-medium" href={resolve(`/models/${encodeURIComponent(model.model_id)}`)}>
                {effectiveModelDisplayName(model)}
              </a>
              <TechnicalValue value={model.model_id} copyable />
              <p class="mt-1 text-xs text-muted-foreground">
                {m.model_specification_context()}:
                <span class="font-technical tabular-nums">
                  {model.context_window == null
                    ? m.model_specification_not_registered()
                    : formatSpecificationTokens(model.context_window)}
                </span>
              </p>
              <div class="mt-1 text-sm">{@render modelDestinations(model)}</div>
              <StatusIndicator
                class="mt-1"
                compact
                label={model.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
                tone={model.is_enabled ? 'healthy' : 'neutral'} />
            </div>
            <div class="flex items-start gap-1">{@render modelActions(model)}</div>
          </div>
        {/each}
      </div>
    {/if}
  </section>
</div>

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>
        {m.models_delete_named_model({
          name: deleteTarget ? effectiveModelDisplayName(deleteTarget) : m.common_model(),
        })}
      </AlertDialog.Title>
      <AlertDialog.Description>
        {m.models_removed_service_destinations({ count: deleteTarget?.targets.length ?? 0 })}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <div class="flex flex-col gap-3 rounded-lg border p-3">
      {#if routeDependenciesUnavailable}
        <p class="text-sm text-destructive">
          {m.models_stravia_not_check_everything_uses_model_try_again()}
        </p>
      {/if}
      <div>
        <p class="font-medium">
          {m.models_removed_api_key_permissions({ count: deleteApiKeyReferences.length })}
        </p>
        {#if deleteApiKeyReferences.length > 0}
          <ul class="mt-1 list-disc pl-5 text-sm text-muted-foreground">
            {#each deleteApiKeyReferences as apiKey (apiKey.id)}<li>{apiKey.name}</li>{/each}
          </ul>
        {/if}
      </div>
      {#if deletesWebSearchRoute}
        <p class="text-sm text-warning">
          {m.models_web_search_no_longer_has_model()}
        </p>
      {/if}
      {#if deletesMediaUnderstandingRoute}
        <p class="text-sm text-warning">
          {m.models_image_understanding_no_longer_have_model_use()}
        </p>
      {/if}
    </div>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action
        variant="destructive"
        disabled={routeDependenciesUnavailable}
        onclick={() => void deleteModel()}>{m.models_delete_model_label()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

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
import { effectiveModelDisplayName, logicalModelSecondaryId, sortLogicalModels } from '$lib/logical-model'
import type { Route, RouteSelectionStrategy } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import TechnicalValue from '$lib/components/technical-value.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
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
const apiKeys = $derived(apiKeysQuery.data ?? [])
const tableLabels = $derived(getDataTableLabels())
const modelColumnHelper = createDataTableColumnHelper<Route>()

function strategyLabel(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'weighted':
      return m.models_split_share()
    case 'priority':
      return m.common_try_order()
    case 'cooldown':
      return m.model_editor_rotate_destinations()
    case 'latency':
      return m.model_editor_prefer_low_latency()
  }
}

const modelColumns = modelColumnHelper.columns([
  modelColumnHelper.accessor((model) => `${effectiveModelDisplayName(model)} ${model.model_id}`, {
    id: 'model',
    header: () => m.common_model(),
    cell: (context) => renderSnippet(modelIdentityCell, context),
    meta: { label: () => m.common_model() },
    size: 200,
  }),
  modelColumnHelper.accessor('balance', {
    header: () => m.models_request_handling(),
    cell: (context) => renderSnippet(modelBalanceCell, context),
    meta: { label: () => m.models_request_handling() },
    size: 170,
  }),
  modelColumnHelper.accessor((model) => targetsLabel(model), {
    id: 'destinations',
    header: () => m.models_destinations(),
    cell: (context) => renderSnippet(modelTargetsCell, context),
    meta: { label: () => m.models_destinations() },
    size: 420,
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

function targetsLabel(model: Route): string {
  return model.targets
    .map(
      (target) =>
        `${providers.find((provider) => provider.id === target.provider_id)?.name ?? target.provider_id}: ${target.model}`,
    )
    .join(', ')
}

function openModel(model: Route, event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  void goto(resolve(`/models/${encodeURIComponent(model.model_id)}`))
}

function handleModelTableRowClick({ event, original }: DataTableRowPointerEvent<Route>): void {
  openModel(original, event)
}

function handleModelRowKeydown(event: KeyboardEvent, model: Route): void {
  if (event.key !== 'Enter' || event.target !== event.currentTarget) return
  event.preventDefault()
  void goto(resolve(`/models/${encodeURIComponent(model.model_id)}`))
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

{#snippet modelIdentityCell(context: DataTableCellContext<Route>)}
  {@const model = context.row.original}
  <div class="min-w-0">
    <span class="block truncate font-medium">{effectiveModelDisplayName(model)}</span>
    {#if logicalModelSecondaryId(model)}
      <span class="block truncate font-technical text-xs text-muted-foreground">{model.model_id}</span>
    {/if}
  </div>
{/snippet}

{#snippet modelBalanceCell(context: DataTableCellContext<Route>)}
  <Badge variant="outline">
    {strategyLabel(context.row.original.balance)}
  </Badge>
{/snippet}

{#snippet modelTargetsCell(context: DataTableCellContext<Route>)}
  <TechnicalValue value={targetsLabel(context.row.original)} copyable />
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
    <div class="route-section-header">
      <div>
        <h2 id="model-route-table-title" class="route-section-title">{m.models_configured_models()}</h2>
        <p class="route-section-description">
          {m.models_split_traffic_across_services_try_them_order_one()}
        </p>
      </div>
    </div>

    {#if modelsQuery.isPending || providersQuery.isPending}
      <div class="flex flex-col border-y" aria-label={m.models_loading_models()}>
        {#each Array(5) as _, index (index)}<div
            class="grid grid-cols-[2fr_1fr_3fr_1fr] gap-4 border-b p-3 last:border-b-0">
            <Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" />
          </div>{/each}
      </div>
    {:else if modelsQuery.isError || providersQuery.isError}
      <div class="border-y py-6">
        <p class="text-sm font-medium text-destructive">
          {m.models_models_not_loaded()}
        </p>
        <p class="mt-1 text-sm text-muted-foreground">
          {localizeBackendErrorMessage(modelsQuery.error ?? providersQuery.error)}
        </p>
        <Button
          class="mt-3"
          variant="outline"
          onclick={() => void Promise.all([modelsQuery.refetch(), providersQuery.refetch()])}
          >{m.common_retry()}</Button>
      </div>
    {:else if models.length === 0}
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
    {:else}
      <div class="route-desktop-table">
        <DataTable
          data={models}
          columns={modelColumns}
          labels={tableLabels}
          getRowId={getModelRowId}
          ariaLabel={m.models_configured_models()}
          stripedRows
          sortMode="multiple"
          resizableColumns
          onRowClick={handleModelTableRowClick} />
      </div>
      <div class="route-mobile-list">
        {#each models as model (model.id)}
          <div
            class="route-mobile-row cursor-pointer"
            role="link"
            tabindex="0"
            onclick={(event) => openModel(model, event)}
            onkeydown={(event) => handleModelRowKeydown(event, model)}>
            <div class="min-w-0">
              <p class="truncate font-medium">{effectiveModelDisplayName(model)}</p>
              {#if logicalModelSecondaryId(model)}
                <p class="truncate font-technical text-xs text-muted-foreground">{model.model_id}</p>
              {/if}
              <p class="mt-1 text-xs text-muted-foreground">
                {strategyLabel(model.balance)} · {model.targets.length === 1
                  ? m.common_1_destination()
                  : m.models_value_destinations({ target_count: model.targets.length })}
              </p>
              <TechnicalValue class="mt-1 text-muted-foreground" value={targetsLabel(model)} /><StatusIndicator
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

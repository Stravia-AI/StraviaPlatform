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
import { formatDuration } from '$lib/format'
import { effectiveModelDisplayName, logicalModelSecondaryId } from '$lib/logical-model'
import type { ImageCapabilityDrift, Provider, Route } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import ProviderEditor from '$lib/components/provider-editor.svelte'
import ProviderMark from '$lib/components/provider-mark.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import TechnicalValue from '$lib/components/technical-value.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import { Checkbox } from '$lib/components/ui/checkbox'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRowPointerEvent,
} from '$lib/components/ui/data-table'
import * as DropdownMenu from '$lib/components/ui/dropdown-menu'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import { Skeleton } from '$lib/components/ui/skeleton'

const queryClient = useQueryClient()
const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const presetsQuery = createQuery(() => ({ queryKey: ['catalog-providers'], queryFn: admin.catalog.providers }))
const capabilityDriftsQuery = createQuery(() => ({
  queryKey: ['image-capability-drifts'],
  queryFn: admin.providers.capabilityDrifts,
  refetchInterval: 60_000,
}))

let editorOpen = $state(false)
let deleteTarget = $state<Provider>()
let deleteOpen = $state(false)
let copyTarget = $state<Provider>()
let copyOpen = $state(false)
let appendTargets = $state(false)
let actingProviderId = $state<string>()

const providers = $derived(providersQuery.data ?? [])
const models = $derived(modelsQuery.data ?? [])
const providerDependenciesUnavailable = $derived(modelsQuery.isPending || modelsQuery.isError)
const presets = $derived(presetsQuery.data ?? [])
const driftsByProvider = $derived.by(() => {
  const grouped: Record<string, ImageCapabilityDrift[]> = {}
  for (const drift of capabilityDriftsQuery.data ?? []) {
    const existing = grouped[drift.provider_id]
    if (existing) existing.push(drift)
    else grouped[drift.provider_id] = [drift]
  }
  return grouped
})
const providerColumnHelper = createDataTableColumnHelper<Provider>()
const providerColumns = providerColumnHelper.columns([
  providerColumnHelper.accessor('name', {
    header: () => m.common_model_service(),
    cell: (context) => renderSnippet(providerIdentityCell, context),
    meta: { label: () => m.common_model_service() },
    size: 280,
  }),
  providerColumnHelper.accessor('protocol', {
    header: () => m.common_protocol(),
    cell: (context) => renderSnippet(providerProtocolCell, context),
    meta: { label: () => m.common_protocol() },
    size: 180,
  }),
  providerColumnHelper.accessor('base_url', {
    header: () => m.common_base_url(),
    cell: (context) => renderSnippet(providerBaseUrlCell, context),
    meta: { label: () => m.common_base_url() },
    size: 320,
  }),
  providerColumnHelper.accessor((provider) => provider.auth_mode ?? 'apikey', {
    id: 'authentication',
    header: () => m.providers_authentication(),
    cell: (context) => renderSnippet(providerAuthenticationCell, context),
    meta: { label: () => m.providers_authentication() },
    size: 160,
  }),
  providerColumnHelper.accessor('is_enabled', {
    header: () => m.common_status(),
    cell: (context) => renderSnippet(providerStatusCell, context),
    meta: { label: () => m.common_status() },
    size: 170,
  }),
  providerColumnHelper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(providerActionsCell, context),
    enableHiding: false,
    enableSorting: false,
    meta: { label: () => m.common_actions(), align: 'end', exportable: false },
    size: 88,
  }),
])
const providerTableLabels = $derived({
  ...getDataTableLabels(),
  search: m.providers_table_search(),
  noResults: m.providers_table_no_results(),
})

function providerIcon(provider: Provider): string {
  return provider.preset_key ?? 'custom'
}

function openCreate(): void {
  editorOpen = true
}

function openProvider(provider: Provider, event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  void goto(resolve(`/providers/${encodeURIComponent(provider.id)}?view=connection`))
}

function getProviderRowId(provider: Provider): string {
  return provider.id
}

function handleProviderTableRowClick({ event, original }: DataTableRowPointerEvent<Provider>): void {
  openProvider(original, event)
}

function providerReferences(provider: Provider): Array<{ route: Route; target: Route['targets'][number] }> {
  return models.flatMap((route) =>
    route.targets.filter((target) => target.provider_id === provider.id).map((target) => ({ route, target })),
  )
}

function providerSaved(provider: Provider): void {
  void goto(resolve(`/providers/${encodeURIComponent(provider.id)}?view=models&sync=created`))
}

function askDelete(provider: Provider): void {
  deleteTarget = provider
  deleteOpen = true
}

function askCopy(provider: Provider): void {
  copyTarget = provider
  appendTargets = false
  copyOpen = true
}

async function toggleProvider(provider: Provider): Promise<void> {
  actingProviderId = provider.id
  try {
    await admin.providers.update(provider.id, { is_enabled: !provider.is_enabled })
    await queryClient.invalidateQueries({ queryKey: ['providers'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingProviderId = undefined
  }
}

async function testProvider(provider: Provider): Promise<void> {
  actingProviderId = provider.id
  try {
    const result = await admin.providers.test(provider.id)
    if (result.success) {
      toast.success(m.common_service_response_time({ duration: formatDuration(result.latency_ms) }))
    } else {
      toast.error(result.error || m.common_connection_test_failed())
    }
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingProviderId = undefined
  }
}

async function deleteProvider(): Promise<void> {
  if (!deleteTarget) return
  actingProviderId = deleteTarget.id
  try {
    await admin.providers.delete(deleteTarget.id)
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['models'] }),
      queryClient.invalidateQueries({ queryKey: ['api-keys'] }),
    ])
    toast.success(m.providers_model_service_deleted())
    deleteOpen = false
    deleteTarget = undefined
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingProviderId = undefined
  }
}

async function copyProvider(): Promise<void> {
  if (!copyTarget) return
  actingProviderId = copyTarget.id
  try {
    await admin.providers.copy(copyTarget.id, { append_targets: appendTargets })
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['models'] }),
    ])
    toast.success(m.providers_model_service_duplicated())
    copyOpen = false
    copyTarget = undefined
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingProviderId = undefined
  }
}
</script>

<svelte:head><title>{m.common_model_services()} · Stravia</title></svelte:head>

{#snippet providerPageActions()}
  <Button onclick={openCreate}><PlusIcon data-icon="inline-start" />{m.common_connect_service()}</Button>
{/snippet}

{#snippet providerActions(provider: Provider)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger>
      {#snippet child({ props })}
        <Button
          {...props}
          size="icon"
          class="size-10"
          variant="ghost"
          aria-label={m.providers_more_actions_value({ name: provider.name })}>
          <MoreHorizontalIcon />
        </Button>
      {/snippet}
    </DropdownMenu.Trigger>
    <DropdownMenu.Content class="w-52" align="end">
      <DropdownMenu.Group>
        <DropdownMenu.Item onSelect={() => void testProvider(provider)} disabled={actingProviderId === provider.id}>
          {m.providers_test_connection()}
        </DropdownMenu.Item>
        <DropdownMenu.Item onSelect={() => void toggleProvider(provider)} disabled={actingProviderId === provider.id}>
          {provider.is_enabled ? m.providers_disable_service() : m.providers_enable_service()}
        </DropdownMenu.Item>
        <DropdownMenu.Item onSelect={() => askCopy(provider)}
          >{m.providers_duplicate_service_action()}</DropdownMenu.Item>
      </DropdownMenu.Group>
      <DropdownMenu.Separator />
      <DropdownMenu.Group>
        <DropdownMenu.Item variant="destructive" onSelect={() => askDelete(provider)}
          >{m.providers_delete_service()}</DropdownMenu.Item>
      </DropdownMenu.Group>
    </DropdownMenu.Content>
  </DropdownMenu.Root>
{/snippet}

{#snippet providerIdentityCell(context: DataTableCellContext<Provider>)}
  {@const provider = context.row.original}
  {@const drifts = driftsByProvider[provider.id] ?? []}
  <div class="flex items-center gap-3">
    <ProviderMark
      icon={providerIcon(provider)}
      name={provider.name}
      catalog={Boolean(provider.preset_key)}
      endpoint={provider.base_url} />
    <div class="min-w-0">
      <p class="truncate font-medium">{provider.name}</p>
      {#if provider.vendor}
        <p class="font-technical truncate text-[0.7rem] text-muted-foreground">{provider.vendor}</p>
      {/if}
      {#if drifts.length > 0}
        <Badge
          class="mt-1"
          variant="destructive"
          title={drifts.map((drift) => `${drift.upstream_model}: ${drift.safe_message}`).join('\n')}>
          {m.providers_model_compatibility_warnings({ count: drifts.length })}
        </Badge>
      {/if}
    </div>
  </div>
{/snippet}

{#snippet providerProtocolCell(context: DataTableCellContext<Provider>)}
  <Badge variant="outline">{context.row.original.protocol}</Badge>
{/snippet}

{#snippet providerBaseUrlCell(context: DataTableCellContext<Provider>)}
  <TechnicalValue value={context.row.original.base_url} copyable />
{/snippet}

{#snippet providerAuthenticationCell(context: DataTableCellContext<Provider>)}
  {context.row.original.auth_mode === 'oauth' ? 'OAuth' : m.common_api_key()}
{/snippet}

{#snippet providerStatusCell(context: DataTableCellContext<Provider>)}
  {@const provider = context.row.original}
  <StatusIndicator
    compact
    label={provider.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
    tone={provider.is_enabled ? 'healthy' : 'neutral'} />
  {#if provider.oauth_status}
    <p class="mt-1 text-xs text-muted-foreground">OAuth · {provider.oauth_status}</p>
  {/if}
{/snippet}

{#snippet providerActionsCell(context: DataTableCellContext<Provider>)}
  <div class="flex justify-end gap-1">
    {@render providerActions(context.row.original)}
  </div>
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.common_setup()}
    title={m.common_model_services()}
    description={m.providers_connect_ai_services_handle_requests_models()}
    actions={providers.length > 0 ? providerPageActions : undefined} />

  <section class="route-section" aria-labelledby="provider-table-title">
    <div class="route-section-header">
      <div>
        <h2 id="provider-table-title" class="route-section-title">
          {m.providers_connected_services()}
        </h2>
        <p class="route-section-description">
          {m.providers_catalog_summary()}
        </p>
      </div>
    </div>

    {#if providersQuery.isPending}
      <div class="flex flex-col gap-0 border-y" aria-label={m.providers_loading_model_services()}>
        {#each Array(5) as _, index (index)}
          <div class="grid grid-cols-[2fr_1fr_3fr_1fr] gap-4 border-b p-3 last:border-b-0">
            <Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" />
          </div>
        {/each}
      </div>
    {:else if providersQuery.isError}
      <div class="border-y py-6">
        <p class="text-sm font-medium text-destructive">
          {m.providers_model_services_not_loaded()}
        </p>
        <p class="mt-1 text-sm text-muted-foreground">
          {localizeBackendErrorMessage(providersQuery.error)}
        </p>
        <Button class="mt-3" variant="outline" onclick={() => void providersQuery.refetch()}>{m.common_retry()}</Button>
      </div>
    {:else if providers.length === 0}
      <Empty.Root class="border-y py-10">
        <Empty.Header
          ><Empty.Title>{m.providers_no_model_services_connected()}</Empty.Title><Empty.Description
            >{m.providers_model_nowhere_without_provider()}</Empty.Description
          ></Empty.Header>
        <Empty.Content><Button onclick={openCreate}>{m.common_connect_first_service()}</Button></Empty.Content>
      </Empty.Root>
    {:else}
      <div class="route-desktop-table">
        <DataTable
          data={providers}
          columns={providerColumns}
          labels={providerTableLabels}
          getRowId={getProviderRowId}
          ariaLabel={m.providers_table_aria_label()}
          globalFilterEnabled
          globalFilterPlaceholder={m.providers_table_search()}
          columnToggle
          exportable
          exportFilename="stravia-model-services.csv"
          stripedRows
          scrollHeight="32rem"
          stickyHeader
          sortMode="multiple"
          resizableColumns
          reorderableColumns
          stateKey="providers-data-table"
          onRowClick={handleProviderTableRowClick} />
      </div>

      <div class="route-mobile-list">
        {#each providers as provider (provider.id)}
          {@const drifts = driftsByProvider[provider.id] ?? []}
          <div class="route-mobile-row">
            <div class="min-w-0">
              <div class="flex items-center gap-3">
                <ProviderMark
                  icon={providerIcon(provider)}
                  name={provider.name}
                  catalog={Boolean(provider.preset_key)}
                  endpoint={provider.base_url} />
                <div class="min-w-0">
                  <p class="truncate font-medium">{provider.name}</p>
                  <p class="mt-1 text-xs text-muted-foreground">
                    {provider.protocol} · {provider.auth_mode === 'oauth' ? 'OAuth' : m.common_api_key()}
                  </p>
                </div>
              </div>
              <TechnicalValue class="mt-2 max-w-full text-muted-foreground" value={provider.base_url} />
              <StatusIndicator
                class="mt-1"
                compact
                label={provider.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
                tone={provider.is_enabled ? 'healthy' : 'neutral'} />
              {#if drifts.length > 0}
                <Badge
                  class="mt-2"
                  variant="destructive"
                  title={drifts.map((drift) => `${drift.upstream_model}: ${drift.safe_message}`).join('\n')}>
                  {m.providers_model_compatibility_warnings({ count: drifts.length })}
                </Badge>
              {/if}
            </div>
            <div class="flex items-start gap-1">
              <Button variant="ghost" size="sm" href={`/providers/${encodeURIComponent(provider.id)}?view=connection`}
                >{m.providers_open()}</Button>
              {@render providerActions(provider)}
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </section>
</div>

<ProviderEditor bind:open={editorOpen} {presets} onSaved={providerSaved} />

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    {@const references = deleteTarget ? providerReferences(deleteTarget) : []}
    <AlertDialog.Header>
      <AlertDialog.Title>
        {deleteTarget && providerDependenciesUnavailable
          ? m.providers_not_check_where_value_used({ name: deleteTarget.name })
          : deleteTarget && references.length > 0
            ? m.providers_cannot_delete_value({ name: deleteTarget.name })
            : m.providers_delete_named_service({ name: deleteTarget?.name ?? m.common_model_service() })}
      </AlertDialog.Title>
      <AlertDialog.Description>
        {providerDependenciesUnavailable
          ? m.providers_stravia_not_check_which_models_use_service_try()
          : references.length > 0
            ? m.providers_model_destinations_in_use({ count: references.length })
            : m.providers_no_models_use_service_deleting_cannot_undone()}
      </AlertDialog.Description>
    </AlertDialog.Header>
    {#if references.length > 0}
      <div class="flex flex-col gap-2 rounded-lg border p-3">
        {#each references as reference (reference.target.id)}
          <a
            class="flex min-h-10 items-center justify-between gap-3 rounded-md px-2 hover:bg-muted"
            href={resolve('/models/[id]', { id: reference.route.model_id })}>
            <span class="font-medium">
              {effectiveModelDisplayName(reference.route)}
              {#if logicalModelSecondaryId(reference.route)}
                · {reference.route.model_id}{/if}
              · {reference.target.model}
            </span>
            <span class="text-xs text-muted-foreground">{m.providers_change_service()}</span>
          </a>
        {/each}
      </div>
    {/if}
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      {#if deleteTarget && references.length > 0 && deleteTarget.is_enabled}
        <Button variant="outline" onclick={() => deleteTarget && void toggleProvider(deleteTarget)}>
          {m.providers_disable_service_instead()}
        </Button>
      {/if}
      <AlertDialog.Action
        variant="destructive"
        disabled={providerDependenciesUnavailable || references.length > 0}
        onclick={() => void deleteProvider()}>{m.providers_delete_service_label()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<AlertDialog.Root bind:open={copyOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header
      ><AlertDialog.Title>{m.providers_duplicate_model_service()}</AlertDialog.Title><AlertDialog.Description
        >{copyTarget
          ? m.providers_create_disabled_copy_value_so_review_use({ name: copyTarget.name })
          : ''}</AlertDialog.Description
      ></AlertDialog.Header>
    <Field.Field orientation="horizontal"
      ><Checkbox id="append-model-targets" bind:checked={appendTargets} /><Field.Label for="append-model-targets"
        >{m.providers_add_copy_same_models()}</Field.Label
      ></Field.Field>
    <AlertDialog.Footer
      ><AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel><AlertDialog.Action
        onclick={() => void copyProvider()}>{m.providers_duplicate_service_short_action()}</AlertDialog.Action
      ></AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

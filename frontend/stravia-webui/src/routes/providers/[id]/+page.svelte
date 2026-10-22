<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left'
import PlusIcon from '@lucide/svelte/icons/plus'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'

import { getModelChanges, syncToast, syncErrorToast, modelChangeReason } from '#lib/model-changes.js'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { effectiveModelDisplayName, logicalModelSecondaryId } from '#lib/logical-model.js'
import type { Provider, ProviderModelSyncSummary } from '#lib/types.js'
import PageHeader from '#lib/components/page-header.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import ProviderConnectionView from '#lib/components/provider-connection-view.svelte'
import ProviderMark from '#lib/components/provider-mark.svelte'
import ProviderModelCatalog from '#lib/components/provider-model-catalog.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import AllowanceSuspension from '#lib/components/allowance-suspension.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { tabsListVariants, tabsTriggerVariants } from '#lib/components/ui/tabs/index.js'

const providerId = $derived(page.params.id ?? '')

type ProviderDetailView = 'connection' | 'models' | 'routes'

const view = $derived(parseProviderDetailView(page.url.searchParams.get('view')))
const queryClient = useQueryClient()
const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))

const providerDescriptorsQuery = createQuery(() => ({
  queryKey: ['provider-descriptors'],
  queryFn: admin.providers.descriptors,
}))

const routesQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
let syncStatus = $state<'idle' | 'syncing' | 'success' | 'error'>('idle')
let syncProviderId = $state('')
const changesSheet = getModelChanges()
let syncError = $state('')
let autoSyncStarted = $state(false)
let savedProvider = $state<Provider>()
const provider = $derived(savedProvider ?? providersQuery.data?.find((item) => item.id === providerId))
const descriptor = $derived(providerDescriptorsQuery.data?.find((item) => item.provider_id === provider?.vendor))
const channel = $derived(descriptor?.channels.find((item) => item.id === provider?.channel))
const canDiscoverModels = $derived(channel?.capabilities.includes('model_discovery') ?? false)
const usesProviderModels = $derived(
  Boolean(
    channel?.capabilities.includes('infer') ||
    channel?.capabilities.includes('media_image') ||
    channel?.capabilities.includes('search'),
  ),
)

const detailTabs = $derived([
  { id: 'connection', label: m.provider_detail_tab_connection },
  ...(usesProviderModels ? [{ id: 'models', label: m.provider_detail_tab_models }] : []),
  { id: 'routes', label: m.provider_detail_tab_routes },
])

const routeReferences = $derived(
  (routesQuery.data ?? []).flatMap((route) =>
    route.targets.filter((target) => target.provider_id === providerId).map((target) => ({ route, target })),
  ),
)

$effect(() => {
  if (page.url.searchParams.get('sync') === 'created' && provider && canDiscoverModels && !autoSyncStarted) {
    autoSyncStarted = true
    void syncModels()
  }
})

function parseProviderDetailView(value: string | null): ProviderDetailView {
  return value === 'models' || value === 'routes' ? value : 'connection'
}

async function syncModels(id = providerId): Promise<ProviderModelSyncSummary | undefined> {
  if (id === providerId && !canDiscoverModels) return undefined
  if (id === providerId) {
    syncProviderId = id
    syncStatus = 'syncing'
    syncError = ''
  }
  try {
    const summary = await admin.providers.syncModels(id)
    if (id === providerId) syncStatus = 'success'
    await queryClient.invalidateQueries({ queryKey: ['provider-models', id] })
    await queryClient.invalidateQueries({ queryKey: ['provider-model-sync-state', id] })
    if (page.params.id === id && page.url.searchParams.get('sync') === 'created') {
      const url = new URL(page.url.href)

      url.searchParams.delete('sync')
      await goto(resolve(`providers/${encodeURIComponent(id)}${url.search}`), { replace: true, reset: false })
    }
    syncToast(summary, changesSheet.reveal, () => void syncModels(id))
    return summary
  } catch (error) {
    if (id === providerId) {
      syncStatus = 'error'
      syncError = modelChangeReason(localizeBackendErrorMessage(error))
    }
    syncErrorToast(error, changesSheet.reveal, () => void syncModels(id))
    await queryClient.invalidateQueries({ queryKey: ['provider-model-sync-state', id] })
    return undefined
  }
}
</script>

<svelte:head
  ><title>{provider ? provider.name.trim() || provider.id : m.common_model_service()} · Stravia</title></svelte:head>

{#if providersQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if providersQuery.isError && !provider}
  <div class="route-page">
    <PageHeader title={m.common_model_service_details()} />
    <RequestFailure
      title={m.providers_model_services_not_loaded()}
      message={localizeBackendErrorMessage(providersQuery.error)}
      retry={() => providersQuery.refetch()}
      retrying={providersQuery.isFetching} />
    <Button href="/providers" variant="outline"
      ><ArrowLeftIcon data-icon="inline-start" />{m.providers_back_model_services()}</Button>
  </div>
{:else if !provider}
  <div class="route-page">
    <PageHeader title={m.providers_model_service_not_found()} />
    <Button href="/providers" variant="outline"
      ><ArrowLeftIcon data-icon="inline-start" />{m.providers_back_model_services()}</Button>
  </div>
{:else}
  <div class="route-page">
    <PageHeader title={provider.name.trim() || provider.id}>
      {#snippet meta()}
        {@const credentialInvalid = provider.credential_status === 'invalid'}
        <div class="flex flex-wrap items-center gap-2">
          <ProviderMark
            icon={descriptor?.catalog_id ?? provider.preset_key ?? provider.vendor ?? 'custom'}
            name={provider.name}
            logo={provider.id} />
          {#if credentialInvalid || !provider.is_enabled || !provider.allowance_suspension}
            <StatusIndicator
              compact
              label={credentialInvalid && provider.is_enabled
                ? m.providers_credential_invalid()
                : provider.is_enabled
                  ? m.common_enabled_status()
                  : m.common_inactive_status()}
              tone={credentialInvalid && provider.is_enabled ? 'error' : provider.is_enabled ? 'healthy' : 'neutral'} />
          {/if}
          {#if provider.allowance_suspension}<AllowanceSuspension suspension={provider.allowance_suspension} />{/if}
          {#if credentialInvalid && !provider.is_enabled}
            <Badge variant="destructive">{m.providers_credential_invalid()}</Badge>
          {/if}
        </div>
      {/snippet}
      {#snippet actions()}
        {#if usesProviderModels}
          <Button variant="outline" href={`/models/new?provider=${encodeURIComponent(provider.id)}`}
            ><PlusIcon data-icon="inline-start" />{m.providers_use_model()}</Button>
        {/if}
      {/snippet}
    </PageHeader>
    <nav class={tabsListVariants()} data-variant="default" aria-label={m.common_model_service_details()}>
      {#each detailTabs as item (item.id)}
        <a
          class={tabsTriggerVariants()}
          data-state={view === item.id ? 'active' : 'inactive'}
          aria-current={view === item.id ? 'page' : undefined}
          href={resolve(`providers/${encodeURIComponent(providerId)}?view=${encodeURIComponent(item.id)}`)}
          >{item.label()}</a>
      {/each}
    </nav>

    {#if view === 'connection' || (view === 'models' && !usesProviderModels)}
      <ProviderConnectionView {provider} onSaved={(saved: Provider) => (savedProvider = saved)} />
    {:else if view === 'models'}
      {#if syncProviderId === providerId && (syncStatus === 'syncing' || syncStatus === 'error')}
        <section class="rounded-xl border p-4" aria-live="polite">
          {#if syncStatus === 'syncing'}
            <div class="flex items-center gap-3">
              <Spinner />
              <div>
                <p class="font-medium">{m.providers_syncing_models()}</p>
                <p class="text-sm text-muted-foreground">
                  {m.providers_checking_service_latest_models_keep_using_page()}
                </p>
              </div>
            </div>
          {:else if syncStatus === 'error'}
            <div class="flex flex-wrap items-center justify-between gap-3">
              <div>
                <p class="font-medium text-destructive">
                  {page.url.searchParams.get('sync') === 'created'
                    ? m.providers_connection_saved_but_models_couldn_t_synced()
                    : m.model_changes_failed()}
                </p>
                <p class="mt-1 text-sm text-muted-foreground">{syncError}</p>
              </div>
              <Button variant="outline" onclick={() => void syncModels()}>
                <RefreshCwIcon data-icon="inline-start" />{m.common_try_again()}
              </Button>
            </div>
          {/if}
        </section>
      {/if}
      <ProviderModelCatalog
        providerId={provider.id}
        routes={routesQuery.data ?? []}
        routeReferencesReady={!routesQuery.isPending && !routesQuery.isError}
        onSync={canDiscoverModels ? syncModels : undefined} />
    {:else if view === 'routes'}
      <section class="route-section" aria-labelledby="provider-route-references-title">
        <h2 id="provider-route-references-title" class="sr-only">{m.providers_used_models()}</h2>
        {#if routesQuery.isPending}
          <div class="grid min-h-32 place-items-center"><Spinner /></div>
        {:else if routesQuery.isError}
          <div class="flex flex-wrap items-center justify-between gap-3 border-y py-4">
            <p class="text-sm text-destructive">
              {localizeBackendErrorMessage(routesQuery.error)}
            </p>
            <Button variant="outline" onclick={() => void routesQuery.refetch()}>
              <RefreshCwIcon data-icon="inline-start" />{m.common_retry()}
            </Button>
          </div>
        {:else if routeReferences.length === 0}
          <div class="flex flex-col items-center gap-3 rounded-xl border border-dashed px-4 py-10 text-center">
            <p class="text-sm text-muted-foreground">{m.providers_no_models_use_model_service_yet()}</p>
            {#if usesProviderModels}
              <Button variant="outline" href={resolve(`providers/${encodeURIComponent(providerId)}?view=models`)}
                >{m.providers_browse_service_models()}</Button>
            {/if}
          </div>
        {:else}
          <!-- 带列名的紧凑列：宽时表头 + 三列靠左，窄时每行用标签区分三个值。 -->
          {@const columns = '@min-[40rem]/route-references:grid-cols-[minmax(10rem,22rem)_minmax(10rem,26rem)_5rem]'}
          <div role="table" aria-label={m.providers_used_models()} class="@container/route-references border-y">
            <div
              role="row"
              class={[
                'hidden gap-x-8 border-b py-2 text-xs font-medium text-muted-foreground @min-[40rem]/route-references:grid',
                columns,
              ]}>
              <div role="columnheader">{m.providers_route_client_model()}</div>
              <div role="columnheader">{m.providers_route_upstream_model()}</div>
              <div role="columnheader">{m.providers_route_priority()}</div>
            </div>
            {#each routeReferences as reference (reference.target.id)}
              {@const sameId = reference.target.model === reference.route.model_id}
              <div
                role="row"
                class={[
                  'grid gap-x-8 gap-y-2 py-3 not-first:border-t @min-[40rem]/route-references:items-start',
                  columns,
                ]}>
                <div role="cell" class="min-w-0">
                  <span class="block text-xs text-muted-foreground @min-[40rem]/route-references:hidden"
                    >{m.providers_route_client_model()}</span>
                  <a
                    class="block font-medium hover:underline"
                    href={resolve('/models/[id]', { id: reference.route.model_id })}>
                    <span class="block break-words">{effectiveModelDisplayName(reference.route)}</span>
                    {#if logicalModelSecondaryId(reference.route)}
                      <span class="block break-all font-technical text-xs font-normal text-muted-foreground"
                        >{reference.route.model_id}</span>
                    {/if}
                  </a>
                </div>
                <div role="cell" class="min-w-0">
                  <span class="block text-xs text-muted-foreground @min-[40rem]/route-references:hidden"
                    >{m.providers_route_upstream_model()}</span>
                  {#if sameId}
                    <span class="text-sm text-muted-foreground">{m.providers_route_same_id()}</span>
                  {:else}
                    <TechnicalValue value={reference.target.model} copyable />
                  {/if}
                </div>
                <div role="cell" class="min-w-0">
                  <span class="block text-xs text-muted-foreground @min-[40rem]/route-references:hidden"
                    >{m.providers_route_priority()}</span>
                  <Badge variant="outline" class="font-technical">{reference.target.priority}</Badge>
                </div>
              </div>
            {/each}
          </div>
        {/if}
      </section>
    {/if}
  </div>
{/if}

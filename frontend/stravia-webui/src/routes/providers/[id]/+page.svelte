<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { page } from '$app/state'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left'
import PlusIcon from '@lucide/svelte/icons/plus'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import type { Provider, ProviderModelSyncSummary } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import ProviderConnectionView from '$lib/components/provider-connection-view.svelte'
import ProviderMark from '$lib/components/provider-mark.svelte'
import ProviderModelCatalog from '$lib/components/provider-model-catalog.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import TechnicalValue from '$lib/components/technical-value.svelte'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'

const providerId = $derived(page.params.id ?? '')
type ProviderDetailView = 'connection' | 'models' | 'routes'
const view = $derived(parseProviderDetailView(page.url.searchParams.get('view')))
const queryClient = useQueryClient()
const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const routesQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
let syncStatus = $state<'idle' | 'syncing' | 'success' | 'error'>('idle')
let syncSummary = $state<ProviderModelSyncSummary>()
let syncCompletedAt = $state<Date>()
let syncError = $state('')
let autoSyncStarted = $state(false)
let savedProvider = $state<Provider>()

const provider = $derived(savedProvider ?? providersQuery.data?.find((item) => item.id === providerId))
const routeReferences = $derived(
  (routesQuery.data ?? []).flatMap((route) =>
    route.targets.filter((target) => target.provider_id === providerId).map((target) => ({ route, target })),
  ),
)

$effect(() => {
  if (page.url.searchParams.get('sync') === 'created' && provider && !autoSyncStarted) {
    autoSyncStarted = true
    void syncModels()
  }
})

function parseProviderDetailView(value: string | null): ProviderDetailView {
  return value === 'models' || value === 'routes' ? value : 'connection'
}

async function syncModels(): Promise<ProviderModelSyncSummary | undefined> {
  syncStatus = 'syncing'
  syncError = ''
  try {
    const summary = await admin.providers.syncModels(providerId)
    syncSummary = summary
    syncCompletedAt = new Date()
    syncStatus = 'success'
    await queryClient.invalidateQueries({ queryKey: ['provider-models', providerId] })
    const url = new URL(page.url)
    url.searchParams.delete('sync')
    await goto(resolve(`/providers/${encodeURIComponent(providerId)}${url.search}`), {
      replaceState: true,
      noScroll: true,
      keepFocus: true,
    })
    return summary
  } catch (error) {
    syncStatus = 'error'
    syncError = localizeBackendErrorMessage(error)
    return undefined
  }
}
</script>

<svelte:head><title>{provider?.name ?? m.common_model_service()} · Stravia</title></svelte:head>

{#if providersQuery.isPending}
  <div class="grid min-h-72 place-items-center"><Spinner /></div>
{:else if !provider}
  <div class="route-page">
    <PageHeader
      eyebrow={m.common_model_service()}
      title={m.providers_model_service_not_found()}
      description={m.providers_saved_model_service_no_longer_available()} />
    <Button href="/providers" variant="outline"
      ><ArrowLeftIcon data-icon="inline-start" />{m.providers_back_model_services()}</Button>
  </div>
{:else}
  <div class="route-page">
    <PageHeader
      eyebrow={m.common_model_service_details()}
      title={provider.name}
      description={m.providers_page_summary()}>
      {#snippet meta()}
        <div class="flex flex-wrap items-center gap-2">
          <ProviderMark
            icon={provider.preset_key ?? 'custom'}
            name={provider.name}
            catalog={Boolean(provider.preset_key)}
            endpoint={provider.base_url} />
          <Badge variant="outline">{provider.protocol}</Badge>
          <StatusIndicator
            compact
            label={provider.is_enabled ? m.common_enabled_status() : m.common_inactive_status()}
            tone={provider.is_enabled ? 'healthy' : 'neutral'} />
        </div>
      {/snippet}
      {#snippet actions()}
        <Button href={`/models/new?provider=${encodeURIComponent(provider.id)}`}
          ><PlusIcon data-icon="inline-start" />{m.providers_use_model()}</Button>
      {/snippet}
    </PageHeader>

    <nav class="flex flex-wrap gap-2" aria-label={m.common_model_service_details()}>
      {#each [{ id: 'connection', label: m.provider_detail_tab_connection }, { id: 'models', label: m.provider_detail_tab_models }, { id: 'routes', label: m.provider_detail_tab_routes }] as item (item.id)}
        <a
          class="inline-flex min-h-10 items-center rounded-md px-3 text-sm font-medium hover:bg-muted"
          class:bg-muted={view === item.id}
          aria-current={view === item.id ? 'page' : undefined}
          href={resolve(`/providers/${encodeURIComponent(providerId)}?view=${encodeURIComponent(item.id)}`)}
          >{item.label()}</a>
      {/each}
    </nav>

    {#if view === 'connection'}
      <ProviderConnectionView {provider} onSaved={(saved) => (savedProvider = saved)} />
    {:else if view === 'models'}
      {#if syncStatus !== 'idle'}
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
          {:else if syncStatus === 'success' && syncSummary}
            <div>
              <div>
                <p class="font-medium">{m.providers_model_list_updated()}</p>
                <p class="mt-1 text-sm text-muted-foreground">
                  {m.providers_value_new_value_no_longer_offered_value_available({
                    added: syncSummary.added,
                    missing: syncSummary.missing,
                    restored: syncSummary.restored,
                  })}
                </p>
              </div>
            </div>
          {:else if syncStatus === 'error'}
            <div class="flex flex-wrap items-center justify-between gap-3">
              <div>
                <p class="font-medium text-destructive">
                  {m.providers_connection_saved_but_models_couldn_t_synced()}
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
        syncedAt={syncCompletedAt}
        syncedSummary={syncSummary}
        onSync={syncModels} />
    {:else if view === 'routes'}
      <section class="route-section" aria-labelledby="provider-route-references-title">
        <div class="route-section-header">
          <div>
            <h2 id="provider-route-references-title" class="route-section-title">
              {m.providers_used_models()}
            </h2>
            <p class="route-section-description">
              {m.providers_model_usage_summary()}
            </p>
          </div>
          <Badge variant="outline">{routeReferences.length}</Badge>
        </div>
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
          <p class="border-y py-8 text-sm text-muted-foreground">
            {m.providers_no_models_use_model_service_yet()}
          </p>
        {:else}
          <div class="divide-y border-y">
            {#each routeReferences as reference (reference.target.id)}
              <div class="grid gap-3 py-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] sm:items-center">
                <a
                  class="min-h-10 content-center font-medium hover:underline"
                  href={resolve('/models/[id]', { id: reference.route.id })}>{reference.route.name}</a>
                <TechnicalValue value={reference.target.model} copyable />
                <Badge variant="outline">{m.providers_order()} {reference.target.priority}</Badge>
              </div>
            {/each}
          </div>
        {/if}
      </section>
    {/if}
  </div>
{/if}

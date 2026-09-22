<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import RequestFailure from '$lib/components/request-failure.svelte'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { tick } from 'svelte'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { logicalModelSecondaryId, sortLogicalModels } from '$lib/logical-model'
import type { WebSearchBackend, WebSearchConfig } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import WebAccessConfiguration from '$lib/components/web-access-configuration.svelte'
import { Button, buttonVariants } from '$lib/components/ui/button'
import * as Alert from '$lib/components/ui/alert'
import * as Collapsible from '$lib/components/ui/collapsible'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

const queryClient = useQueryClient()
const configQuery = createQuery(() => ({ queryKey: ['web-search-config'], queryFn: admin.webSearch.config.get }))
const eligibleModelsQuery = createQuery(() => ({
  queryKey: ['web-search-eligible-models'],
  queryFn: admin.webSearch.eligibleModels,
}))
const externalRoutesQuery = createQuery(() => ({
  queryKey: ['web-search-external-routes'],
  queryFn: admin.webSearch.externalRoutes,
}))

let initialized = $state(false)
let backendKind = $state<'local' | 'external'>('local')

const sourceSettingsQuery = createQuery(() => ({
  queryKey: ['web-access-settings'],
  queryFn: admin.webAccess.settings.get,
  enabled: configQuery.data?.backend?.kind === 'local' || (initialized && backendKind === 'local'),
}))
const sourceProvidersQuery = createQuery(() => ({
  queryKey: ['web-providers'],
  queryFn: admin.webAccess.providers.list,
  enabled: configQuery.data?.backend?.kind === 'local' || (initialized && backendKind === 'local'),
}))

let toggleSaving = $state(false)
let toggleError = $state('')
let localModelId = $state('')
let externalRouteId = $state('')
let maxTurns = $state('')
let totalSeconds = $state('')
let advancedOpen = $state(false)
let saving = $state(false)
let saveError = $state('')
const hasChanges = $derived.by(() => {
  const config = configQuery.data
  if (!config) return false
  const backend = config.backend
  return (
    backendKind !== (backend?.kind ?? 'local') ||
    (backendKind === 'local'
      ? localModelId !== (backend?.kind === 'local' ? (backend.model_id ?? '') : '')
      : externalRouteId !== (backend?.kind === 'external' ? (backend.route_id ?? '') : '')) ||
    Number(maxTurns) !== config.max_turns ||
    Number(totalSeconds) !== config.total_time_seconds
  )
})

const externalRoutes = $derived.by(() => {
  const routes = externalRoutesQuery.data ?? []
  if (!externalRouteId || routes.some((route) => route.model_id === externalRouteId)) return routes
  return [
    ...routes,
    { id: externalRouteId, model_id: externalRouteId, display_name: externalRouteId, available: false },
  ]
})
const eligibleModels = $derived(sortLogicalModels(eligibleModelsQuery.data ?? []))
const limits = $derived(configQuery.data?.limits)
const selectedExternalRoute = $derived(externalRoutes.find((route) => route.model_id === externalRouteId))
const bindingReady = $derived(
  backendKind === 'local'
    ? Boolean(localModelId)
    : Boolean(externalRouteId) && Boolean(selectedExternalRoute?.available),
)
const localLimitsReady = $derived(
  backendKind !== 'local' ||
    Boolean(
      limits &&
      Number(maxTurns) >= limits.min_turns &&
      Number(maxTurns) <= limits.max_turns &&
      Number(totalSeconds) >= limits.min_total_time_seconds &&
      Number(totalSeconds) <= limits.max_total_time_seconds,
    ),
)
const savedBindingReady = $derived.by(() => {
  const backend = configQuery.data?.backend
  if (backend?.kind === 'local') {
    return (
      !eligibleModelsQuery.isError && Boolean(eligibleModelsQuery.data?.some((model) => model.id === backend.model_id))
    )
  }
  if (backend?.kind === 'external') {
    return (
      !externalRoutesQuery.isError &&
      Boolean(externalRoutesQuery.data?.some((route) => route.model_id === backend.route_id && route.available))
    )
  }
  return false
})
// Availability hints use the server's capability metadata; core validates activation.
const savedSourceIssue = $derived.by(() => {
  if (configQuery.data?.backend?.kind !== 'local') return undefined
  // 在提前返回前订阅全部依赖，避免某项加载失败时其余查询停留在旧快照。
  const settingsStatus = sourceSettingsQuery.status
  const providersStatus = sourceProvidersQuery.status
  const settings = sourceSettingsQuery.data
  const providers = sourceProvidersQuery.data ?? []
  if (settingsStatus === 'error' || providersStatus === 'error') return 'unavailable'
  if (settingsStatus === 'pending' || providersStatus === 'pending') return 'loading'
  const hasSource = (capability: 'search' | 'fetch', ids: string[]) =>
    providers.some((provider) => ids.includes(provider.id) && provider.capabilities[capability])
  if (
    settings &&
    hasSource('search', settings.search_provider_ids) &&
    hasSource('fetch', settings.fetch_provider_ids)
  ) {
    return undefined
  }
  return 'missing'
})
const canEnable = $derived(savedBindingReady && !savedSourceIssue)
const canSave = $derived(
  Boolean(configQuery.data) &&
    hasChanges &&
    !saving &&
    !toggleSaving &&
    localLimitsReady &&
    (!configQuery.data?.enabled || bindingReady),
)

$effect(() => {
  const config = configQuery.data
  if (!config || initialized) return
  initialized = true
  loadDraft(config)
})

function loadDraft(config: WebSearchConfig): void {
  backendKind = config.backend?.kind ?? 'local'
  localModelId = config.backend?.kind === 'local' ? (config.backend.model_id ?? '') : ''
  externalRouteId = config.backend?.kind === 'external' ? (config.backend.route_id ?? '') : ''
  maxTurns = String(config.max_turns)
  totalSeconds = String(config.total_time_seconds)
}

$effect(() => {
  if (limits && !localLimitsReady) advancedOpen = true
})

function backendDraft(): WebSearchBackend {
  return backendKind === 'local'
    ? { kind: 'local', model_id: localModelId || null }
    : { kind: 'external', route_id: externalRouteId || null }
}

async function toggleEnabled(enabled: boolean): Promise<void> {
  const current = configQuery.data
  if (!current || saving || toggleSaving || enabled === current.enabled || (enabled && !canEnable)) return
  toggleSaving = true
  toggleError = ''
  try {
    const input: WebSearchConfig = {
      revision: current.revision,
      enabled,
      backend: current.backend,
      max_turns: current.max_turns,
      total_time_seconds: current.total_time_seconds,
      updated_at: current.updated_at,
    }
    const config = await admin.webSearch.config.update(input)
    queryClient.setQueryData(['web-search-config'], config)
  } catch (error) {
    toggleError = localizeBackendErrorMessage(error)
  } finally {
    toggleSaving = false
  }
}

async function save(): Promise<void> {
  const current = configQuery.data
  if (!current || !canSave) return
  saving = true
  saveError = ''
  try {
    const input: WebSearchConfig = {
      revision: current.revision,
      enabled: current.enabled,
      backend: backendDraft(),
      max_turns: Number(maxTurns),
      total_time_seconds: Number(totalSeconds),
      updated_at: current.updated_at,
    }
    const config = await admin.webSearch.config.update(input)
    loadDraft(config)
    queryClient.setQueryData(['web-search-config'], config)
    toast.success(m.web_search_settings_saved())
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
    toast.error(saveError)
  } finally {
    saving = false
  }
}
</script>

<svelte:head><title>{m.web_search_title()} · Stravia</title></svelte:head>

<div class="route-page mx-auto max-w-[64rem]">
  <PageHeader
    eyebrow={m.app_shell_nav_advanced_features()}
    title={m.web_search_title()}
    description={m.web_search_feature_summary()} />

  {#if configQuery.isError}
    <RequestFailure
      title={m.web_search_settings_not_loaded()}
      message={localizeBackendErrorMessage(configQuery.error)}
      retry={() => configQuery.refetch()}
      retrying={configQuery.isFetching} />
  {/if}
  {#if configQuery.isPending}
    <p class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
  {/if}
  {#if configQuery.data}
    <section class="route-section" aria-labelledby="search-gate-title">
      <div class="route-section-header">
        <div class="min-w-0 flex-1 basis-64">
          <h2 id="search-gate-title" class="route-section-title">{m.web_search_enable()}</h2>
          <p id="search-gate-description" class="route-section-description">
            {m.web_search_method_selection_help()}
          </p>
        </div>
        <div class="flex shrink-0 items-center gap-3">
          {#if toggleSaving}<Spinner />{/if}
          <Switch
            bind:checked={() => configQuery.data?.enabled ?? false, (value: boolean) => void toggleEnabled(value)}
            disabled={saving || toggleSaving || (!configQuery.data.enabled && !canEnable)}
            aria-busy={toggleSaving}
            aria-labelledby="search-gate-title"
            aria-describedby="search-gate-description" />
        </div>
      </div>
      <div class="flex flex-col gap-3">
        {#if toggleError}<Alert.Root variant="destructive"
            ><Alert.Description>{toggleError}</Alert.Description></Alert.Root
          >{/if}
        {#if savedSourceIssue}
          <Alert.Root variant="warning" role="status">
            <Alert.Description>
              {savedSourceIssue === 'loading'
                ? m.common_settings_loading()
                : savedSourceIssue === 'unavailable'
                  ? m.web_search_sources_unavailable()
                  : m.web_search_sources_required()}
              <Button
                variant="link"
                size="sm"
                onclick={async () => {
                  backendKind = 'local'
                  await tick()
                  document.getElementById('web-search-sources')?.scrollIntoView({ block: 'start' })
                }}>{m.web_search_configure_sources()}</Button>
            </Alert.Description>
          </Alert.Root>
        {/if}
        {#if !savedBindingReady}
          <Alert.Root variant="warning" role="status">
            <Alert.Description>
              {m.web_search_enable_prerequisite()}
              {#if !configQuery.data.enabled}{m.common_enable_requires_saved_settings()}{/if}
            </Alert.Description>
          </Alert.Root>
        {/if}
      </div>
    </section>

    <section class="route-section" aria-labelledby="search-backend-title">
      <div class="route-section-header">
        <div>
          <h2 id="search-backend-title" class="route-section-title">{m.web_search_method_title()}</h2>
        </div>
        <Button disabled={!canSave} aria-busy={saving} onclick={() => void save()}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_settings()}
        </Button>
      </div>
      <div class="flex flex-col gap-3">
        {#if hasChanges}<p class="text-sm text-muted-foreground" role="status">{m.common_settings_unsaved()}</p>{/if}
        {#if saveError}<Alert.Root variant="destructive"><Alert.Description>{saveError}</Alert.Description></Alert.Root
          >{/if}
        {#if configQuery.data.enabled && !bindingReady}
          <p class="text-sm text-muted-foreground" role="status">{m.web_search_enable_prerequisite()}</p>
        {/if}
      </div>

      <Field.Group>
        <Field.Field size="select">
          <Field.Label for="search-backend">{m.web_search_method()}</Field.Label>
          <Select.Root type="single" bind:value={backendKind} disabled={saving}>
            <Select.Trigger id="search-backend" class="w-full">
              {backendKind === 'local' ? m.web_search_use_stravia_model() : m.web_search_use_external_route()}
            </Select.Trigger>
            <Select.Content
              ><Select.Group>
                <Select.Item value="local" label={m.web_search_use_stravia_model()}>
                  {m.web_search_use_stravia_model()}
                </Select.Item>
                <Select.Item value="external" label={m.web_search_use_external_route()}>
                  {m.web_search_use_external_route()}
                </Select.Item>
              </Select.Group></Select.Content>
          </Select.Root>
        </Field.Field>

        {#if backendKind === 'local'}
          <Field.Field size="select">
            <Field.Label for="search-local-model" hint={m.web_search_eligible_model_help()}>
              {m.web_search_model_used()}
            </Field.Label>
            <Select.Root
              type="single"
              bind:value={localModelId}
              disabled={saving || eligibleModelsQuery.isPending || eligibleModelsQuery.isError}>
              <Select.Trigger id="search-local-model" class="w-full">
                {eligibleModels.find((model) => model.id === localModelId)?.display_name ?? m.common_select_model()}
              </Select.Trigger>
              <Select.Content
                ><Select.Group>
                  {#each eligibleModels as model (model.id)}
                    {@const secondaryId = logicalModelSecondaryId(model)}
                    <Select.Item value={model.id} label={model.display_name}>
                      <span class="min-w-0 flex-1 truncate">{model.display_name}</span>
                      {#if secondaryId}
                        <span class="truncate font-technical text-xs text-muted-foreground">{secondaryId}</span>
                      {/if}
                    </Select.Item>
                  {/each}
                </Select.Group></Select.Content>
            </Select.Root>
          </Field.Field>
        {:else}
          <Field.Field size="select" data-invalid={Boolean(externalRouteId) && !selectedExternalRoute?.available}>
            <Field.Label for="search-external-route">{m.web_search_external_route()}</Field.Label>
            <Select.Root
              type="single"
              bind:value={externalRouteId}
              disabled={saving || externalRoutesQuery.isPending || externalRoutesQuery.isError}>
              <Select.Trigger id="search-external-route" class="w-full">
                {selectedExternalRoute?.display_name ?? m.web_search_select_external_route()}
              </Select.Trigger>
              <Select.Content
                ><Select.Group>
                  {#each externalRoutes as route (route.model_id)}
                    <Select.Item value={route.model_id} label={route.display_name} disabled={!route.available}>
                      <span class="min-w-0 flex-1 truncate">{route.display_name}</span>
                      <span class="truncate font-technical text-xs text-muted-foreground">{route.model_id}</span>
                    </Select.Item>
                  {/each}
                </Select.Group></Select.Content>
            </Select.Root>
            {#if externalRouteId && !selectedExternalRoute?.available}
              <Field.Error>{m.web_search_external_route_unavailable()}</Field.Error>
            {/if}
          </Field.Field>
        {/if}
      </Field.Group>
      {#if backendKind === 'local' && eligibleModelsQuery.isError}
        <RequestFailure
          class="mt-4"
          message={localizeBackendErrorMessage(eligibleModelsQuery.error)}
          retry={() => eligibleModelsQuery.refetch()}
          retrying={eligibleModelsQuery.isFetching} />
      {:else if backendKind === 'external' && externalRoutesQuery.isError}
        <RequestFailure
          class="mt-4"
          title={m.web_search_external_routes_not_loaded()}
          message={localizeBackendErrorMessage(externalRoutesQuery.error)}
          retry={() => externalRoutesQuery.refetch()}
          retrying={externalRoutesQuery.isFetching} />
      {:else if (backendKind === 'local' && eligibleModelsQuery.isPending) || (backendKind === 'external' && externalRoutesQuery.isPending)}
        <p class="mt-4 text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
      {/if}
    </section>

    {#if backendKind === 'local'}
      <Collapsible.Root bind:open={advancedOpen} class="flex flex-col gap-4 border-t pt-4">
        <Collapsible.Trigger class={buttonVariants({ variant: 'outline', class: 'self-start' })}
          >{m.common_advanced()}</Collapsible.Trigger>
        <Collapsible.Content>
          <section id="web-search-advanced-fields" class="route-section" aria-labelledby="search-limits-title">
            <div class="route-section-header">
              <div>
                <h2 id="search-limits-title" class="route-section-title">{m.web_search_limits_title()}</h2>
                <p class="route-section-description">
                  {m.web_search_limits_help()}
                </p>
              </div>
            </div>
            <Field.Group>
              <Field.Field size="number" data-invalid={!localLimitsReady}>
                <Field.Label for="search-max-turns">{m.web_search_maximum_steps()}</Field.Label>
                <Input
                  id="search-max-turns"
                  disabled={saving}
                  type="number"
                  aria-invalid={!localLimitsReady}
                  aria-describedby={!localLimitsReady ? 'search-limits-error' : undefined}
                  min={limits?.min_turns}
                  max={limits?.max_turns}
                  bind:value={maxTurns} />
                <Field.Description>{limits ? `${limits.min_turns}–${limits.max_turns}` : '—'}</Field.Description>
              </Field.Field>
              <Field.Field size="number" data-invalid={!localLimitsReady}>
                <Field.Label for="search-total-seconds">{m.web_search_time_limit_seconds()}</Field.Label>
                <Input
                  id="search-total-seconds"
                  disabled={saving}
                  type="number"
                  aria-invalid={!localLimitsReady}
                  aria-describedby={!localLimitsReady ? 'search-limits-error' : undefined}
                  min={limits?.min_total_time_seconds}
                  max={limits?.max_total_time_seconds}
                  bind:value={totalSeconds} />
                <Field.Description>
                  {limits ? `${limits.min_total_time_seconds}–${limits.max_total_time_seconds}` : '—'}
                </Field.Description>
              </Field.Field>
            </Field.Group>
            {#if !localLimitsReady}
              <p id="search-limits-error" class="mt-4 text-sm font-medium text-destructive">
                {m.web_search_choose_supported_limits()}
              </p>
            {/if}
          </section>
        </Collapsible.Content>
      </Collapsible.Root>

      <WebAccessConfiguration />
    {/if}
  {/if}
</div>

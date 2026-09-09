<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import RequestFailure from '$lib/components/request-failure.svelte'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

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
const codexProvidersQuery = createQuery(() => ({
  queryKey: ['web-search-codex-providers'],
  queryFn: admin.webSearch.compatibleCodexProviders,
}))

let initialized = $state(false)
let toggleSaving = $state(false)
let toggleError = $state('')
let backendKind = $state<'local' | 'codex'>('local')
let localModelId = $state('')
let codexProviderId = $state('')
let codexModelId = $state('')
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
      : codexProviderId !== (backend?.kind === 'codex' ? (backend.provider_id ?? '') : '') ||
        codexModelId !== (backend?.kind === 'codex' ? (backend.upstream_model ?? '') : '')) ||
    Number(maxTurns) !== config.max_turns ||
    Number(totalSeconds) !== config.total_time_seconds
  )
})

const codexProviders = $derived(codexProvidersQuery.data ?? [])
const codexModels = $derived(codexProviders.find((provider) => provider.id === codexProviderId)?.models ?? [])
const eligibleModels = $derived(sortLogicalModels(eligibleModelsQuery.data ?? []))
const limits = $derived(configQuery.data?.limits)
const bindingReady = $derived(
  backendKind === 'local' ? Boolean(localModelId) : Boolean(codexProviderId) && Boolean(codexModelId),
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
  if (backend?.kind === 'codex') {
    return (
      !codexProvidersQuery.isError &&
      Boolean(
        codexProvidersQuery.data?.some(
          (provider) =>
            provider.id === backend.provider_id && provider.models.some((model) => model.id === backend.upstream_model),
        ),
      )
    )
  }
  return false
})
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
  codexProviderId = config.backend?.kind === 'codex' ? (config.backend.provider_id ?? '') : ''
  codexModelId = config.backend?.kind === 'codex' ? (config.backend.upstream_model ?? '') : ''
  maxTurns = String(config.max_turns)
  totalSeconds = String(config.total_time_seconds)
}

$effect(() => {
  if (limits && !localLimitsReady) advancedOpen = true
})

function backendDraft(): WebSearchBackend {
  return backendKind === 'local'
    ? { kind: 'local', model_id: localModelId || null }
    : { kind: 'codex', provider_id: codexProviderId || null, upstream_model: codexModelId || null }
}

async function toggleEnabled(enabled: boolean): Promise<void> {
  const current = configQuery.data
  if (!current || saving || toggleSaving || enabled === current.enabled || (enabled && !savedBindingReady)) return
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
            bind:checked={() => configQuery.data?.enabled ?? false, (value) => void toggleEnabled(value)}
            disabled={saving || toggleSaving || (!configQuery.data.enabled && !savedBindingReady)}
            aria-busy={toggleSaving}
            aria-labelledby="search-gate-title"
            aria-describedby="search-gate-description" />
        </div>
      </div>
      <div class="flex flex-col gap-3">
        {#if toggleError}<Alert.Root variant="destructive"
            ><Alert.Description>{toggleError}</Alert.Description></Alert.Root
          >{/if}
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
              {backendKind === 'local' ? m.web_search_use_stravia_model() : m.web_search_use_codex()}
            </Select.Trigger>
            <Select.Content
              ><Select.Group>
                <Select.Item value="local" label={m.web_search_use_stravia_model()}>
                  {m.web_search_use_stravia_model()}
                </Select.Item>
                <Select.Item value="codex" label={m.web_search_use_codex()}>{m.web_search_use_codex()}</Select.Item>
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
          <Field.Field size="select">
            <Field.Label for="search-codex-provider">{m.web_search_codex_account()}</Field.Label>
            <Select.Root
              type="single"
              bind:value={codexProviderId}
              disabled={saving || codexProvidersQuery.isPending || codexProvidersQuery.isError}
              onValueChange={() => {
                codexModelId = ''
              }}>
              <Select.Trigger id="search-codex-provider" class="w-full">
                {codexProviders.find((provider) => provider.id === codexProviderId)?.name ??
                  m.web_search_select_codex_account()}
              </Select.Trigger>
              <Select.Content
                ><Select.Group>
                  {#each codexProviders as provider (provider.id)}
                    <Select.Item value={provider.id} label={provider.name}>{provider.name}</Select.Item>
                  {/each}
                </Select.Group></Select.Content>
            </Select.Root>
          </Field.Field>
          <Field.Field size="select">
            <Field.Label for="search-codex-model">{m.web_search_codex_model()}</Field.Label>
            <Select.Root
              type="single"
              bind:value={codexModelId}
              disabled={saving || codexProvidersQuery.isPending || codexProvidersQuery.isError || !codexProviderId}>
              <Select.Trigger id="search-codex-model" class="w-full">
                {codexModels.find((model) => model.id === codexModelId)?.id ?? m.web_search_select_codex_model()}
              </Select.Trigger>
              <Select.Content
                ><Select.Group>
                  {#each codexModels as model (model.id)}
                    <Select.Item value={model.id} label={model.id}>{model.id}</Select.Item>
                  {/each}
                </Select.Group></Select.Content>
            </Select.Root>
          </Field.Field>
        {/if}
      </Field.Group>
      {#if backendKind === 'local' && eligibleModelsQuery.isError}
        <RequestFailure
          class="mt-4"
          message={localizeBackendErrorMessage(eligibleModelsQuery.error)}
          retry={() => eligibleModelsQuery.refetch()}
          retrying={eligibleModelsQuery.isFetching} />
      {:else if backendKind === 'codex' && codexProvidersQuery.isError}
        <RequestFailure
          class="mt-4"
          message={localizeBackendErrorMessage(codexProvidersQuery.error)}
          retry={() => codexProvidersQuery.refetch()}
          retrying={codexProvidersQuery.isFetching} />
      {:else if (backendKind === 'local' && eligibleModelsQuery.isPending) || (backendKind === 'codex' && codexProvidersQuery.isPending)}
        <p class="mt-4 text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
      {/if}
      {#if backendKind === 'local'}
        <Alert.Root class="mt-4" role="note"
          ><Alert.Description>{m.web_search_data_disclosure_notice()}</Alert.Description></Alert.Root>
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

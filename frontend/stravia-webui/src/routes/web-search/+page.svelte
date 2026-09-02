<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import SearchCheckIcon from '@lucide/svelte/icons/search-check'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { sortLogicalModels } from '$lib/logical-model'
import type { WebSearchBackend, WebSearchConfig } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import WebAccessConfiguration from '$lib/components/web-access-configuration.svelte'
import { Button } from '$lib/components/ui/button'
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

let initializedRevision = $state<number>()
let enabled = $state(false)
let backendKind = $state<'local' | 'codex'>('local')
let localModelId = $state('')
let codexProviderId = $state('')
let codexModelId = $state('')
let maxTurns = $state('')
let totalSeconds = $state('')
let advancedOpen = $state(false)
let saving = $state(false)

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
const canSave = $derived(!saving && !configQuery.isPending && localLimitsReady && (!enabled || bindingReady))

$effect(() => {
  const config = configQuery.data
  if (!config || initializedRevision === config.revision) return
  initializedRevision = config.revision
  enabled = config.enabled
  backendKind = config.backend?.kind ?? 'local'
  localModelId = config.backend?.kind === 'local' ? (config.backend.model_id ?? '') : ''
  codexProviderId = config.backend?.kind === 'codex' ? (config.backend.provider_id ?? '') : ''
  codexModelId = config.backend?.kind === 'codex' ? (config.backend.upstream_model ?? '') : ''
  maxTurns = String(config.max_turns)
  totalSeconds = String(config.total_time_seconds)
})

function backendDraft(): WebSearchBackend {
  return backendKind === 'local'
    ? { kind: 'local', model_id: localModelId || null }
    : { kind: 'codex', provider_id: codexProviderId || null, upstream_model: codexModelId || null }
}

async function save(): Promise<void> {
  const current = configQuery.data
  if (!current || !canSave) return
  saving = true
  try {
    const input: WebSearchConfig = {
      revision: current.revision,
      enabled,
      backend: backendDraft(),
      max_turns: Number(maxTurns),
      total_time_seconds: Number(totalSeconds),
      updated_at: current.updated_at,
    }
    await admin.webSearch.config.update(input)
    await queryClient.invalidateQueries({ queryKey: ['web-search-config'] })
    toast.success(m.web_search_settings_saved())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}
</script>

<svelte:head><title>{m.web_search_title()} · Stravia</title></svelte:head>

{#snippet pageActions()}
  <Button disabled={!canSave} aria-busy={saving} onclick={() => void save()}>
    {#if saving}<Spinner data-icon="inline-start" />{/if}
    {m.common_save_settings()}
  </Button>
{/snippet}

<div class="route-page mx-auto max-w-[64rem]">
  <PageHeader
    eyebrow={m.app_shell_nav_advanced_features()}
    title={m.web_search_title()}
    description={m.web_search_feature_summary()}
    actions={pageActions} />

  {#if configQuery.isError}
    <section class="route-section">
      <p class="text-sm font-medium text-destructive">
        {m.web_search_settings_not_loaded()}
      </p>
      <Button class="mt-3" variant="outline" onclick={() => void configQuery.refetch()}>{m.common_retry()}</Button>
    </section>
  {:else}
    <section class="route-section" aria-labelledby="search-gate-title">
      <div class="route-section-header">
        <div>
          <h2 id="search-gate-title" class="route-section-title">
            {m.web_search_enable()}
          </h2>
          <p class="route-section-description">
            {m.web_search_method_selection_help()}
          </p>
        </div>
        <Switch bind:checked={enabled} aria-label={m.web_search_enable()} />
      </div>
    </section>

    <section class="route-section" aria-labelledby="search-backend-title">
      <div class="route-section-header">
        <div>
          <h2 id="search-backend-title" class="route-section-title">{m.web_search_method_title()}</h2>
          <p class="route-section-description">
            {m.web_search_task_method_help()}
          </p>
        </div>
      </div>

      <Field.Group>
        <Field.Field size="select">
          <Field.Label for="search-backend">{m.web_search_method()}</Field.Label>
          <Select.Root type="single" bind:value={backendKind}>
            <Select.Trigger id="search-backend" class="w-full">
              {backendKind === 'local' ? m.web_search_use_stravia_model() : m.web_search_use_codex()}
            </Select.Trigger>
            <Select.Content>
              <Select.Item value="local" label={m.web_search_use_stravia_model()}>
                {m.web_search_use_stravia_model()}
              </Select.Item>
              <Select.Item value="codex" label={m.web_search_use_codex()}>{m.web_search_use_codex()}</Select.Item>
            </Select.Content>
          </Select.Root>
        </Field.Field>

        {#if backendKind === 'local'}
          <Field.Field size="select">
            <Field.Label for="search-local-model" hint={m.web_search_eligible_model_help()}>
              {m.web_search_model_used()}
            </Field.Label>
            <Select.Root type="single" bind:value={localModelId}>
              <Select.Trigger id="search-local-model" class="w-full">
                {eligibleModels.find((model) => model.id === localModelId)?.display_name ?? m.common_select_model()}
              </Select.Trigger>
              <Select.Content>
                {#each eligibleModels as model (model.id)}
                  <Select.Item value={model.id} label={model.display_name}>
                    <span class="min-w-0 flex-1 truncate">{model.display_name}</span>
                    {#if model.display_name !== model.model_id}
                      <span class="truncate font-technical text-xs text-muted-foreground">{model.model_id}</span>
                    {/if}
                  </Select.Item>
                {/each}
              </Select.Content>
            </Select.Root>
          </Field.Field>
        {:else}
          <Field.Field size="select">
            <Field.Label for="search-codex-provider">{m.web_search_codex_account()}</Field.Label>
            <Select.Root
              type="single"
              bind:value={codexProviderId}
              onValueChange={() => {
                codexModelId = ''
              }}>
              <Select.Trigger id="search-codex-provider" class="w-full">
                {codexProviders.find((provider) => provider.id === codexProviderId)?.name ??
                  m.web_search_select_codex_account()}
              </Select.Trigger>
              <Select.Content>
                {#each codexProviders as provider (provider.id)}
                  <Select.Item value={provider.id} label={provider.name}>{provider.name}</Select.Item>
                {/each}
              </Select.Content>
            </Select.Root>
          </Field.Field>
          <Field.Field size="select">
            <Field.Label for="search-codex-model">{m.web_search_codex_model()}</Field.Label>
            <Select.Root type="single" bind:value={codexModelId} disabled={!codexProviderId}>
              <Select.Trigger id="search-codex-model" class="w-full">
                {codexModels.find((model) => model.id === codexModelId)?.id ?? m.web_search_select_codex_model()}
              </Select.Trigger>
              <Select.Content>
                {#each codexModels as model (model.id)}
                  <Select.Item value={model.id} label={model.id}>{model.id}</Select.Item>
                {/each}
              </Select.Content>
            </Select.Root>
          </Field.Field>
        {/if}
      </Field.Group>
      {#if backendKind === 'local'}
        <div class="mt-4 rounded-md border border-border bg-muted/40 p-4 text-sm text-muted-foreground">
          {m.web_search_data_disclosure_notice()}
        </div>
      {/if}
    </section>

    {#if backendKind === 'local'}
      <WebAccessConfiguration />

      <div class="border-t pt-4">
        <Button
          type="button"
          variant="outline"
          aria-expanded={advancedOpen}
          aria-controls="web-search-advanced-fields"
          onclick={() => (advancedOpen = !advancedOpen)}>
          {m.common_advanced()}
        </Button>
      </div>

      {#if advancedOpen}
        <section id="web-search-advanced-fields" class="route-section" aria-labelledby="search-limits-title">
          <div class="route-section-header">
            <div>
              <h2 id="search-limits-title" class="route-section-title">{m.web_search_limits_title()}</h2>
              <p class="route-section-description">
                {m.web_search_limits_help()}
              </p>
            </div>
            <SearchCheckIcon class="size-5 text-muted-foreground" />
          </div>
          <div class="grid max-w-md gap-4 sm:grid-cols-2">
            <Field.Field size="number">
              <Field.Label for="search-max-turns">{m.web_search_maximum_steps()}</Field.Label>
              <Input
                id="search-max-turns"
                type="number"
                min={limits?.min_turns}
                max={limits?.max_turns}
                bind:value={maxTurns} />
              <Field.Description>{limits ? `${limits.min_turns}–${limits.max_turns}` : '—'}</Field.Description>
            </Field.Field>
            <Field.Field size="number">
              <Field.Label for="search-total-seconds">{m.web_search_time_limit_seconds()}</Field.Label>
              <Input
                id="search-total-seconds"
                type="number"
                min={limits?.min_total_time_seconds}
                max={limits?.max_total_time_seconds}
                bind:value={totalSeconds} />
              <Field.Description>
                {limits ? `${limits.min_total_time_seconds}–${limits.max_total_time_seconds}` : '—'}
              </Field.Description>
            </Field.Field>
          </div>
          {#if !localLimitsReady}
            <p class="mt-4 text-sm font-medium text-destructive">
              {m.web_search_choose_supported_limits()}
            </p>
          {/if}
        </section>
      {/if}
    {/if}

    {#if enabled && !bindingReady}
      <p class="text-sm font-medium text-destructive">
        {m.web_search_enable_prerequisite()}
      </p>
    {/if}
  {/if}
</div>

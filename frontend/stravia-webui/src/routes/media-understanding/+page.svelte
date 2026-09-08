<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import RequestFailure from '$lib/components/request-failure.svelte'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { logicalModelSecondaryId, sortLogicalModels } from '$lib/logical-model'
import PageHeader from '$lib/components/page-header.svelte'
import * as Alert from '$lib/components/ui/alert'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import * as Empty from '$lib/components/ui/empty'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import type { EligibleMediaModel, ThinkingLevel } from '$lib/types'

const queryClient = useQueryClient()
const configQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))

let initialized = $state(false)
let toggleSaving = $state(false)
let toggleError = $state('')
let modelId = $state('')
let thinkingLevel = $state<ThinkingLevel | ''>('')
let saving = $state(false)
let saveError = $state('')
const hasChanges = $derived(
  configQuery.data !== undefined &&
    (modelId !== (configQuery.data.model_id ?? '') || thinkingLevel !== (configQuery.data.thinking_level ?? '')),
)

const eligibleModels = $derived(sortLogicalModels(configQuery.data?.eligible_models ?? []))
const selectedModel = $derived(configQuery.data?.eligible_models.find((model) => model.id === modelId))
const savedBindingReady = $derived.by(() => {
  const config = configQuery.data
  if (!config) return false
  const model = config.eligible_models.find((candidate) => candidate.id === config.model_id)
  return Boolean(model && config.thinking_level && model.supported_thinking_levels.includes(config.thinking_level))
})
const canSave = $derived(
  Boolean(configQuery.data) &&
    hasChanges &&
    !saving &&
    !toggleSaving &&
    (!configQuery.data?.enabled ||
      Boolean(selectedModel && thinkingLevel && selectedModel.supported_thinking_levels.includes(thinkingLevel))),
)

function supportedThinkingLevel(
  model: EligibleMediaModel | undefined,
  preferred?: ThinkingLevel | null,
): ThinkingLevel | '' {
  if (!model) return ''
  if (preferred && model.supported_thinking_levels.includes(preferred)) return preferred
  if (model.supported_thinking_levels.includes('medium')) return 'medium'
  return model.supported_thinking_levels[0] ?? ''
}

$effect(() => {
  const config = configQuery.data
  if (!config || initialized) return
  initialized = true
  const model = config.eligible_models.find((candidate) => candidate.id === config.model_id)
  modelId = model?.id ?? ''
  thinkingLevel = supportedThinkingLevel(model, config.thinking_level)
})

async function toggleEnabled(enabled: boolean): Promise<void> {
  const current = configQuery.data
  if (!current || saving || toggleSaving || enabled === current.enabled || (enabled && !savedBindingReady)) return
  toggleSaving = true
  toggleError = ''
  try {
    const config = await admin.mediaUnderstanding.update({
      enabled,
      model_id: current.model_id,
      thinking_level: current.thinking_level,
    })
    queryClient.setQueryData(['media-understanding-config'], config)
  } catch (error) {
    toggleError = localizeBackendErrorMessage(error)
  } finally {
    toggleSaving = false
  }
}

async function save(): Promise<void> {
  if (!canSave || !configQuery.data) return
  saving = true
  saveError = ''
  try {
    const config = await admin.mediaUnderstanding.update({
      enabled: configQuery.data.enabled,
      model_id: modelId || null,
      thinking_level: thinkingLevel || null,
    })
    modelId = config.model_id ?? ''
    thinkingLevel = config.thinking_level ?? ''
    queryClient.setQueryData(['media-understanding-config'], config)
    toast.success(m.media_understanding_settings_saved())
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
    toast.error(saveError)
  } finally {
    saving = false
  }
}

function selectModel(value?: string): void {
  modelId = value ?? ''
  const model = configQuery.data?.eligible_models.find((candidate) => candidate.id === modelId)
  thinkingLevel = supportedThinkingLevel(model, thinkingLevel || null)
}

function selectThinkingLevel(value?: string): void {
  thinkingLevel = selectedModel?.supported_thinking_levels.includes(value as ThinkingLevel)
    ? (value as ThinkingLevel)
    : ''
}
</script>

<svelte:head><title>{m.media_understanding_title()} · Stravia</title></svelte:head>

<div class="route-page mx-auto max-w-[64rem]">
  <PageHeader
    eyebrow={m.app_shell_nav_advanced_features()}
    title={m.media_understanding_title()}
    description={m.media_understanding_feature_summary()} />

  {#if configQuery.isError}
    <RequestFailure
      title={m.media_understanding_settings_not_loaded()}
      message={localizeBackendErrorMessage(configQuery.error)}
      retry={() => configQuery.refetch()}
      retrying={configQuery.isFetching} />
  {/if}
  {#if configQuery.isPending}
    <p class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
  {/if}
  {#if configQuery.data}
    <section class="route-section" aria-labelledby="media-service-title">
      <div class="route-section-header">
        <div class="min-w-0 flex-1 basis-64">
          <h2 id="media-service-title" class="route-section-title">{m.media_understanding_enable()}</h2>
          <p id="media-service-description" class="route-section-description">
            {m.media_understanding_model_requirement()}
          </p>
        </div>
        <div class="flex shrink-0 items-center gap-3">
          {#if toggleSaving}<Spinner />{/if}
          <Switch
            bind:checked={() => configQuery.data?.enabled ?? false, (value) => void toggleEnabled(value)}
            disabled={saving || toggleSaving || (!configQuery.data.enabled && !savedBindingReady)}
            aria-busy={toggleSaving}
            aria-labelledby="media-service-title"
            aria-describedby="media-service-description media-immediate-description" />
        </div>
      </div>
      <div class="flex flex-col gap-3">
        <p id="media-immediate-description" class="text-sm text-muted-foreground">{m.common_settings_immediate()}</p>
        {#if !configQuery.data.enabled && !savedBindingReady}
          <p class="text-sm text-muted-foreground" role="status">{m.common_enable_requires_saved_settings()}</p>
        {/if}
        {#if toggleError}<Alert.Root variant="destructive"
            ><Alert.Description>{toggleError}</Alert.Description></Alert.Root
          >{/if}
        {#if configQuery.data.state === 'unavailable'}
          <Alert.Root variant="warning" role="status"
            ><Alert.Description>{m.media_understanding_saved_unavailable()}</Alert.Description></Alert.Root>
        {/if}
      </div>
    </section>

    <section class="route-section" aria-labelledby="media-model-title">
      <div class="route-section-header">
        <div>
          <h2 id="media-model-title" class="route-section-title">
            {m.media_understanding_model_title()}
          </h2>
          <p class="route-section-description">
            {m.media_understanding_model_role_help()}
          </p>
        </div>
        <Button disabled={!canSave} aria-busy={saving} onclick={() => void save()}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_settings()}
        </Button>
      </div>
      <div class="flex flex-col gap-3">
        {#if hasChanges}<p class="text-sm text-muted-foreground" role="status">{m.common_settings_unsaved()}</p>{/if}
        {#if saveError}<Alert.Root variant="destructive"><Alert.Description>{saveError}</Alert.Description></Alert.Root
          >{/if}
      </div>
      {#if configQuery.data?.eligible_models.length === 0}
        <Empty.Root class="border-y py-6"
          ><Empty.Header
            ><Empty.Description>{m.media_understanding_add_logical_model()}</Empty.Description></Empty.Header
          ><Empty.Content><Button href={resolve('/models')}>{m.connect_add_a_model()}</Button></Empty.Content
          ></Empty.Root>
      {:else}
        <Field.Group>
          <Field.Field size="select">
            <Field.Label for="media-model">{m.media_understanding_model_label()}</Field.Label>
            <Select.Root type="single" value={modelId} disabled={saving} onValueChange={selectModel}>
              <Select.Trigger id="media-model" class="w-full">
                {selectedModel?.display_name ?? m.media_understanding_select_model()}
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
          <Field.Field size="select">
            <Field.Label for="media-thinking-level">
              {m.media_understanding_thinking_level_label()}
            </Field.Label>
            <Select.Root
              type="single"
              value={thinkingLevel}
              disabled={saving || !selectedModel}
              onValueChange={selectThinkingLevel}>
              <Select.Trigger id="media-thinking-level" class="w-full">
                {thinkingLevel || m.media_understanding_select_thinking_level()}
              </Select.Trigger>
              <Select.Content
                ><Select.Group>
                  {#each selectedModel?.supported_thinking_levels ?? [] as level (level)}
                    <Select.Item value={level} label={level}>{level}</Select.Item>
                  {/each}
                </Select.Group></Select.Content>
            </Select.Root>
          </Field.Field>
        </Field.Group>
      {/if}
    </section>
  {/if}
</div>

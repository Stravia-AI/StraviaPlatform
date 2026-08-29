<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import PageHeader from '$lib/components/page-header.svelte'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

const queryClient = useQueryClient()
const configQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))

let initializedConfigKey = $state('')
let enabled = $state(false)
let modelId = $state('')
let saving = $state(false)

const canSave = $derived(Boolean(configQuery.data) && !saving && (!enabled || Boolean(modelId)))

$effect(() => {
  const config = configQuery.data
  if (!config) return
  const configKey = `${config.enabled ? '1' : '0'}:${config.model_id ?? ''}`
  if (initializedConfigKey === configKey) return
  initializedConfigKey = configKey
  enabled = config.enabled
  modelId = config.model_id ?? ''
})

function stateLabel(state: 'disabled' | 'unavailable' | 'available'): string {
  if (state === 'available') return m.media_understanding_available()
  if (state === 'unavailable') return m.common_unavailable()
  return m.common_disabled_status()
}

async function save(): Promise<void> {
  if (!canSave || !configQuery.data) return
  saving = true
  try {
    await admin.mediaUnderstanding.update({ enabled, model_id: modelId || null })
    await queryClient.invalidateQueries({ queryKey: ['media-understanding-config'] })
    toast.success(m.media_understanding_settings_saved())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}
</script>

<svelte:head><title>{m.media_understanding_title()} · Stravia</title></svelte:head>

{#snippet pageActions()}
  <Button disabled={!canSave} aria-busy={saving} onclick={() => void save()}>
    {#if saving}<Spinner data-icon="inline-start" />{/if}
    {m.common_save_settings()}
  </Button>
{/snippet}

<div class="route-page mx-auto max-w-[64rem]">
  <PageHeader
    eyebrow={m.app_shell_nav_advanced_features()}
    title={m.media_understanding_title()}
    description={m.media_understanding_feature_summary()}
    actions={pageActions} />

  {#if configQuery.isError}
    <section class="route-section">
      <p class="text-sm font-medium text-destructive">
        {m.media_understanding_settings_not_loaded()}
      </p>
      <Button class="mt-3" variant="outline" onclick={() => void configQuery.refetch()}>
        {m.common_retry()}
      </Button>
    </section>
  {:else}
    <section class="route-section" aria-labelledby="media-service-title">
      <div class="route-section-header">
        <div>
          <div class="flex items-center gap-2">
            <h2 id="media-service-title" class="route-section-title">
              {m.media_understanding_enable()}
            </h2>
            {#if configQuery.data}
              <Badge variant="outline">{stateLabel(configQuery.data.state)}</Badge>
            {/if}
          </div>
          <p class="route-section-description">
            {m.media_understanding_model_requirement()}
          </p>
        </div>
        <Switch bind:checked={enabled} aria-label={m.media_understanding_enable()} />
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
      </div>
      {#if configQuery.data?.eligible_models.length === 0}
        <div class="flex flex-col gap-3 border-y py-6">
          <p class="text-sm text-muted-foreground">{m.media_understanding_add_multimodal_model()}</p>
          <a class="font-medium text-primary underline-offset-4 hover:underline" href={resolve('/models')}>
            {m.connect_add_a_model()}
          </a>
        </div>
      {:else}
        <Field.Group>
          <Field.Field size="select">
            <Field.Label for="media-model">{m.media_understanding_model_label()}</Field.Label>
            <Select.Root type="single" bind:value={modelId}>
              <Select.Trigger id="media-model" class="w-full">
                {configQuery.data?.eligible_models.find((model) => model.id === modelId)?.name ??
                  m.media_understanding_select_model()}
              </Select.Trigger>
              <Select.Content>
                {#each configQuery.data?.eligible_models ?? [] as model (model.id)}
                  <Select.Item value={model.id} label={model.name}>{model.name}</Select.Item>
                {/each}
              </Select.Content>
            </Select.Root>
          </Field.Field>
        </Field.Group>
      {/if}
    </section>
  {/if}
</div>

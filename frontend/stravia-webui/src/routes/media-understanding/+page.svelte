<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import { logicalModelSecondaryId, sortLogicalModels } from '#lib/logical-model.js'
import PageHeader from '#lib/components/page-header.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import type { EligibleMediaModel, ThinkingLevel } from '#lib/types.js'

const queryClient = useQueryClient()

const configQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))

let toggleSaving = $state(false)
let toggleError = $state('')
let editorOpen = $state(false)
let modelId = $state('')
let thinkingLevel = $state<ThinkingLevel | ''>('')
let saving = $state(false)
let saveError = $state('')

// 绑定草稿：打开 Sheet 时的模型与等级是基线；保存中锁定关闭，失败保留草稿。
const baseline = new DraftBaseline(() => ({ modelId, thinkingLevel }))
const guard = new DraftGuard({ dirty: () => editorOpen && baseline.dirty, busy: () => saving })

const eligibleModels = $derived(sortLogicalModels(configQuery.data?.eligible_models ?? []))
const selectedModel = $derived(configQuery.data?.eligible_models.find((model) => model.id === modelId))
const savedModel = $derived(configQuery.data?.eligible_models.find((model) => model.id === configQuery.data?.model_id))
const hasChanges = $derived(
  configQuery.data !== undefined &&
    (modelId !== (configQuery.data.model_id ?? '') || thinkingLevel !== (configQuery.data.thinking_level ?? '')),
)
const draftValid = $derived(
  Boolean(selectedModel && thinkingLevel && selectedModel.supported_thinking_levels.includes(thinkingLevel)),
)
const bindingSummary = $derived.by(() => {
  const config = configQuery.data

  if (!savedModel) return m.media_understanding_binding_empty()

  return config?.thinking_level ? `${savedModel.display_name} · ${config.thinking_level}` : savedModel.display_name
})

const savedBindingReady = $derived.by(() => {
  const config = configQuery.data

  if (!config) return false

  const model = config.eligible_models.find((candidate) => candidate.id === config.model_id)

  return Boolean(model && config.thinking_level && model.supported_thinking_levels.includes(config.thinking_level))
})

function supportedThinkingLevel(
  model: EligibleMediaModel | undefined,
  preferred?: ThinkingLevel | null,
): ThinkingLevel | '' {
  if (!model) return ''
  if (preferred && model.supported_thinking_levels.includes(preferred)) return preferred
  if (model.supported_thinking_levels.includes('medium')) return 'medium'
  return model.supported_thinking_levels[0] ?? ''
}

function openEditor(): void {
  const config = configQuery.data

  if (!config) return

  modelId = savedModel?.id ?? ''
  thinkingLevel = supportedThinkingLevel(savedModel, config.thinking_level)
  saveError = ''
  baseline.reset()
  editorOpen = true
}

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
  const current = configQuery.data

  if (!current || saving || !hasChanges || !draftValid) return
  saving = true
  saveError = ''
  try {
    const config = await admin.mediaUnderstanding.update({
      enabled: current.enabled,
      model_id: modelId,
      thinking_level: thinkingLevel || null,
    })
    queryClient.setQueryData(['media-understanding-config'], config)
    baseline.reset()
    editorOpen = false
    toast.success(m.media_understanding_settings_saved())
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
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

<div class="route-page">
  <PageHeader title={m.media_understanding_title()} />

  {#if configQuery.isError}
    <RequestFailure
      title={m.media_understanding_settings_not_loaded()}
      message={localizeBackendErrorMessage(configQuery.error)}
      retry={() => configQuery.refetch()}
      retrying={configQuery.isFetching} />
  {:else if configQuery.isPending}
    <div class="content-column flex flex-col gap-4" aria-label={m.common_settings_loading()} aria-busy="true">
      <Skeleton class="h-12" />
      <Skeleton class="h-12" />
    </div>
  {:else if configQuery.data}
    {@const config = configQuery.data}
    <div class="content-column">
      <SettingRow
        title={m.media_understanding_enable()}
        hint={m.media_understanding_model_requirement()}
        description={!config.enabled && !savedBindingReady ? m.common_enable_requires_saved_settings() : undefined}
        layout="inline">
        {#snippet children({ titleId, descriptionId })}
          {#if toggleSaving}<Spinner />{/if}
          <Switch
            bind:checked={() => config.enabled, (value: boolean) => void toggleEnabled(value)}
            disabled={saving || toggleSaving || (!config.enabled && !savedBindingReady)}
            aria-busy={toggleSaving}
            aria-labelledby={titleId}
            aria-describedby={descriptionId} />
        {/snippet}
      </SettingRow>
      {#if config.eligible_models.length === 0}
        <SettingRow
          title={m.media_understanding_model_title()}
          hint={m.media_understanding_model_role_help()}
          description={m.media_understanding_add_logical_model()}>
          <Button variant="outline" size="sm" href={resolve('models')}>{m.connect_add_a_model()}</Button>
        </SettingRow>
      {:else}
        <SettingRow
          title={m.media_understanding_model_title()}
          hint={m.media_understanding_model_role_help()}
          value={bindingSummary}
          onedit={openEditor} />
      {/if}
    </div>
    {#if toggleError || config.state === 'unavailable'}
      <div class="content-column flex flex-col gap-3">
        {#if toggleError}
          <Alert.Root variant="destructive"><Alert.Description>{toggleError}</Alert.Description></Alert.Root>
        {/if}
        {#if config.state === 'unavailable'}
          <Alert.Root variant="warning" role="status">
            <Alert.Description>{m.media_understanding_saved_unavailable()}</Alert.Description>
          </Alert.Root>
        {/if}
      </div>
    {/if}
  {/if}
</div>

<SettingsSheet
  bind:open={editorOpen}
  title={m.media_understanding_edit_binding()}
  submitLabel={m.media_understanding_save_binding()}
  submitDisabled={!hasChanges || !draftValid}
  {saving}
  error={saveError}
  {guard}
  onsubmit={save}>
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
</SettingsSheet>

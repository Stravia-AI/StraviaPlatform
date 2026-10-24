<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { logicalModelSecondaryId, sortLogicalModels } from '#lib/logical-model.js'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import type {
  EligibleMediaModel,
  MediaUnderstandingKind,
  MediaUnderstandingKindConfig,
  ThinkingLevel,
} from '#lib/types.js'

interface Props {
  kind: MediaUnderstandingKind
  config: MediaUnderstandingKindConfig
}

let { kind, config }: Props = $props()

const queryClient = useQueryClient()

// 每种模态拥有独立的启用状态与模型绑定，文案按模态区分，页面与 Sheet 共用同一套交互。
const labelTable = {
  image: {
    title: () => m.media_understanding_image_title(),
    enable: () => m.media_understanding_image_enable(),
    requirement: () => m.media_understanding_image_model_requirement(),
    modelTitle: () => m.media_understanding_image_model_title(),
    editBinding: () => m.media_understanding_image_edit_binding(),
    saveBinding: () => m.media_understanding_image_save_binding(),
    addModel: () => m.media_understanding_image_add_logical_model(),
  },
  audio: {
    title: () => m.media_understanding_audio_title(),
    enable: () => m.media_understanding_audio_enable(),
    requirement: () => m.media_understanding_audio_model_requirement(),
    modelTitle: () => m.media_understanding_audio_model_title(),
    editBinding: () => m.media_understanding_audio_edit_binding(),
    saveBinding: () => m.media_understanding_audio_save_binding(),
    addModel: () => m.media_understanding_audio_add_logical_model(),
  },
}
const labels = $derived(labelTable[kind])

let toggleSaving = $state(false)
let toggleError = $state('')
let editorOpen = $state(false)
let modelId = $state('')
let thinkingLevel = $state<ThinkingLevel | ''>('')
let saving = $state(false)
let saveError = $state('')

const eligibleModels = $derived(sortLogicalModels(config.eligible_models))
const selectedModel = $derived(config.eligible_models.find((model) => model.id === modelId))
const savedModel = $derived(config.eligible_models.find((model) => model.id === config.model_id))
const hasChanges = $derived(modelId !== (config.model_id ?? '') || thinkingLevel !== (config.thinking_level ?? ''))
const draftValid = $derived(
  Boolean(selectedModel && thinkingLevel && selectedModel.supported_thinking_levels.includes(thinkingLevel)),
)
const bindingSummary = $derived.by(() => {
  if (!config.model_id) return m.common_not_configured()

  // 已保存但不再合格的绑定仍显示原始技术 ID，不伪装成「未配置」。
  const name = savedModel?.display_name ?? config.model_id

  return config.thinking_level ? `${name} · ${config.thinking_level}` : name
})

// 功能关闭时允许保存空选择以清除失效绑定；开启时必须选到有效模型。
const canSaveDraft = $derived(hasChanges && (!config.enabled || draftValid))

const savedBindingReady = $derived(
  Boolean(savedModel && config.thinking_level && savedModel.supported_thinking_levels.includes(config.thinking_level)),
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

function openEditor(): void {
  modelId = savedModel?.id ?? ''
  thinkingLevel = supportedThinkingLevel(savedModel, config.thinking_level)
  saveError = ''
  editorOpen = true
}

async function toggleEnabled(enabled: boolean): Promise<void> {
  if (saving || toggleSaving || enabled === config.enabled || (enabled && !savedBindingReady)) return
  toggleSaving = true
  toggleError = ''
  try {
    const view = await admin.mediaUnderstanding.update(kind, {
      enabled,
      model_id: config.model_id,
      thinking_level: config.thinking_level,
    })
    queryClient.setQueryData(['media-understanding-config'], view)
  } catch (error) {
    toggleError = localizeBackendErrorMessage(error)
  } finally {
    toggleSaving = false
  }
}

async function save(): Promise<void> {
  if (saving || !canSaveDraft) return
  saving = true
  saveError = ''
  try {
    const view = await admin.mediaUnderstanding.update(kind, {
      enabled: config.enabled,
      model_id: modelId || null,
      thinking_level: thinkingLevel || null,
    })
    queryClient.setQueryData(['media-understanding-config'], view)
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
  const model = config.eligible_models.find((candidate) => candidate.id === modelId)
  thinkingLevel = supportedThinkingLevel(model, thinkingLevel || null)
}

function selectThinkingLevel(value?: string): void {
  thinkingLevel = selectedModel?.supported_thinking_levels.includes(value as ThinkingLevel)
    ? (value as ThinkingLevel)
    : ''
}
</script>

<SettingsSection id="media-understanding-{kind}" title={labels.title()}>
  <SettingRow
    title={labels.enable()}
    hint={labels.requirement()}
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
      title={labels.modelTitle()}
      hint={m.media_understanding_model_role_help()}
      description={labels.addModel()}>
      <Button variant="outline" size="sm" href={resolve('models')}>{m.connect_add_a_model()}</Button>
    </SettingRow>
  {:else}
    <SettingRow
      title={labels.modelTitle()}
      hint={m.media_understanding_model_role_help()}
      value={bindingSummary}
      onedit={openEditor} />
  {/if}
  {#if toggleError || config.state === 'unavailable'}
    <div class="mt-3 flex flex-col gap-3">
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
</SettingsSection>

<SettingsSheet
  bind:open={editorOpen}
  title={labels.editBinding()}
  submitLabel={labels.saveBinding()}
  submitDisabled={!canSaveDraft}
  {saving}
  error={saveError}
  draft={() => ({ modelId, thinkingLevel })}
  onsubmit={save}>
  <Field.Group>
    <Field.Field>
      <Field.Label for="media-model-{kind}">{m.media_understanding_model_label()}</Field.Label>
      <Select.Root type="single" value={modelId} disabled={saving} onValueChange={selectModel}>
        <Select.Trigger id="media-model-{kind}" class="w-full">
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
    <Field.Field>
      <Field.Label for="media-thinking-level-{kind}">
        {m.media_understanding_thinking_level_label()}
      </Field.Label>
      <Select.Root
        type="single"
        value={thinkingLevel}
        disabled={saving || !selectedModel}
        onValueChange={selectThinkingLevel}>
        <Select.Trigger id="media-thinking-level-{kind}" class="w-full">
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

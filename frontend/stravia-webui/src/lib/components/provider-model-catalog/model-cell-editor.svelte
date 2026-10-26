<script lang="ts">
import type { Snippet } from 'svelte'
import { untrack } from 'svelte'
import { useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import PencilIcon from '@lucide/svelte/icons/pencil'
import * as m from '#lib/paraglide/messages.js'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { localeState } from '#lib/localization.svelte.js'
import ModalitySelect from '#lib/components/modality-select.svelte'
import ReasoningEffortSelect from '#lib/components/reasoning-effort-select.svelte'
import { providerModelSelectionPolicyLabel } from '#lib/provider-model-labels.js'
import { inputValue } from '#lib/utils.js'
import type { ProviderModelDetail, ProviderModelSelectionPolicy, ProviderModelSummary } from '#lib/types.js'
import {
  buildProviderModelMetadataJson,
  normalizeProviderModelEfforts,
  providerModelCostFromMetadata,
} from '../provider-model-form.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import * as Alert from '#lib/components/ui/alert/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'
import * as Select from '#lib/components/ui/select/index.js'

interface Props {
  providerId: string
  model: ProviderModelSummary
  field: 'context' | 'modalities' | 'efforts' | 'availability'
  /** 窄屏紧凑行：触发器更矮，并显示编辑图标让入口可见。 */
  compact?: boolean
  children: Snippet
}
let { providerId, model, field, compact = false, children }: Props = $props()
const queryClient = useQueryClient()
const uid = $props.id()
const targets = ['input', 'output'] as const
let open = $state(false)
let loading = $state(false)
let saving = $state(false)
let detail = $state.raw<ProviderModelDetail>()
let error = $state('')
let context = $state('')
let input = $state<string[]>([])
let output = $state<string[]>([])
let efforts = $state<string[]>([])
let policy = $state<ProviderModelSelectionPolicy>('auto')
let generation = 0
const identity = $derived(JSON.stringify([providerId, model.id, field]))
let label = $derived(
  field === 'context'
    ? m.provider_model_editor_context()
    : field === 'modalities'
      ? m.provider_model_editor_modalities()
      : field === 'efforts'
        ? m.provider_model_editor_reasoning_efforts()
        : m.common_availability_adding_models(),
)
let registeredModalities = $derived([
  ...(detail?.metadata.modalities?.input ?? []),
  ...(detail?.metadata.modalities?.output ?? []),
])

$effect(() => {
  const isOpen = open
  // 刷新列表会替换行对象；同一编辑目标不能因此重置草稿或使保存结果失效。
  void identity
  untrack(() => {
    generation++
    detail = undefined
    error = ''
    saving = false
    loading = false
    if (isOpen) void load(providerId, model.id, field)
  })
  return () => {
    generation++
  }
})

async function load(id = providerId, modelId = model.id, currentField = field): Promise<void> {
  const request = ++generation
  loading = true
  error = ''
  detail = undefined
  try {
    const result = await admin.providers.model(id, modelId)
    if (!isCurrent(request, id, modelId, currentField)) return
    detail = result
    context = result.metadata.limit?.context == null ? '' : String(result.metadata.limit.context)
    input = [...(result.metadata.modalities?.input ?? [])]
    output = [...(result.metadata.modalities?.output ?? [])]
    efforts = normalizeProviderModelEfforts(result.metadata.reasoning_efforts ?? [])
    policy = result.selection_policy
  } catch (cause) {
    if (isCurrent(request, id, modelId, currentField)) {
      error = m.model_editor_model_details_load_failed({ error: localizeBackendErrorMessage(cause) })
    }
  } finally {
    if (isCurrent(request, id, modelId, currentField)) loading = false
  }
}

function isCurrent(request: number, id: string, modelId: string, currentField: Props['field']): boolean {
  return open && request === generation && providerId === id && model.id === modelId && field === currentField
}

async function save(selection?: ProviderModelSelectionPolicy): Promise<void> {
  if (!detail || loading || saving) return
  const snapshot = detail
  const id = providerId
  const modelId = model.id
  const currentField = field
  const request = generation
  saving = true
  error = ''
  try {
    if (currentField === 'availability') {
      if (!selection) return
      await admin.providers.updateModelSelection(id, snapshot.id, selection, snapshot.revision)
    } else {
      const metadata = structuredClone(snapshot.metadata)
      const key =
        currentField === 'context' ? 'limit' : currentField === 'modalities' ? 'modalities' : 'reasoning_efforts'
      if (currentField === 'context')
        metadata.limit = { ...metadata.limit, context: context === '' ? null : Number(context) }
      else if (currentField === 'modalities') metadata.modalities = { input: [...input], output: [...output] }
      else metadata.reasoning_efforts = normalizeProviderModelEfforts(efforts)
      const result = buildProviderModelMetadataJson(snapshot.id, metadata, providerModelCostFromMetadata(metadata))
      if (!result.json) {
        error = result.errors.join('\n')
        return
      }
      // 完整编辑器会规范化其他字段；单元格保存只能替换当前编辑字段。
      const edited = JSON.parse(result.json) as Record<string, unknown>
      const preserved = { ...snapshot.metadata, [key]: edited[key] }
      if (currentField === 'context') preserved.limit = metadata.limit
      await admin.providers.updateModel(id, snapshot.id, JSON.stringify(preserved), snapshot.revision)
    }
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['provider-models', id] }),
      queryClient.invalidateQueries({ queryKey: ['models'] }),
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
    ])
    toast.success(m.provider_model_catalog_model_details_saved())
    if (isCurrent(request, id, modelId, currentField)) open = false
  } catch (cause) {
    const message = localizeBackendErrorMessage(cause)
    if (isCurrent(request, id, modelId, currentField)) error = message
    else toast.error(message)
  } finally {
    if (isCurrent(request, id, modelId, currentField)) saving = false
  }
}
</script>

<Popover.Root bind:open>
  <Popover.Trigger>
    {#snippet child({ props })}
      <Button
        {...props}
        type="button"
        variant="ghost"
        class={[
          'w-full min-w-0 justify-start whitespace-normal text-left',
          compact && 'h-auto min-h-7 gap-1.5 px-1.5 py-0.5',
        ]}
        aria-label={`${label} ${model.id}`}>
        {@render children()}
        {#if compact}<PencilIcon class="size-3 shrink-0 text-muted-foreground" aria-hidden="true" />{/if}
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content
    align="start"
    class="flex max-h-[min(80vh,40rem)] w-80 max-w-[calc(100vw-2rem)] flex-col p-0"
    role="dialog">
    <!-- 字段名已由表单标签给出；标题只交代正在编辑哪个模型，避免「字段 · ID」与标签重复。 -->
    <Popover.Header class="border-b border-border/60 px-4 py-3">
      <Popover.Title class="truncate">{model.name || model.id}</Popover.Title>
      {#if model.name && model.name !== model.id}
        <Popover.Description class="truncate font-technical text-xs">{model.id}</Popover.Description>
      {/if}
    </Popover.Header>
    {#if loading}
      <div class="flex min-h-24 items-center justify-center" aria-busy="true"><Spinner /></div>
    {/if}
    {#if error}
      <div class="flex flex-col items-start gap-3 px-4 pt-4" class:pb-4={!detail && !loading}>
        <Alert.Root variant="destructive" role="alert"
          ><Alert.Description class="whitespace-pre-line">{error}</Alert.Description></Alert.Root>
        {#if !detail && !loading}<Button variant="outline" onclick={() => load()}>{m.common_retry()}</Button>{/if}
      </div>
    {/if}
    {#if detail && !loading}
      <form
        class="flex min-h-0 flex-col"
        onsubmit={(event) => {
          event.preventDefault()
          void save(field === 'availability' ? policy : undefined)
        }}>
        <Field.Group class="min-h-0 overflow-y-auto p-4">
          {#if field === 'context'}
            <Field.Field>
              <Field.Label for={`${uid}-context`}>{label}</Field.Label>
              <Input
                id={`${uid}-context`}
                type="number"
                min="0"
                step="1"
                class="font-technical"
                value={context}
                oninput={(event: Event) => {
                  context = inputValue(event)
                }}
                disabled={saving}
                placeholder={m.model_specification_not_registered()} />
            </Field.Field>
          {:else if field === 'modalities'}
            {#each targets as target (target)}
              <Field.Field>
                <Field.Label for={`${uid}-${target}`}
                  >{target === 'input'
                    ? m.provider_model_editor_accepted_input_types()
                    : m.provider_model_editor_generated_output_types()}</Field.Label>
                {#if target === 'input'}
                  <ModalitySelect
                    id={`${uid}-${target}`}
                    {target}
                    registered={registeredModalities}
                    disabled={saving}
                    bind:value={input} />
                {:else}
                  <ModalitySelect
                    id={`${uid}-${target}`}
                    {target}
                    registered={registeredModalities}
                    disabled={saving}
                    bind:value={output} />
                {/if}
              </Field.Field>
            {/each}
          {:else if field === 'efforts'}
            <Field.Field>
              <Field.Label for={`${uid}-efforts`} hint={m.provider_model_editor_effort_help()}>{label}</Field.Label>
              <ReasoningEffortSelect
                id={`${uid}-efforts`}
                disabled={saving}
                registered={detail?.metadata.reasoning_efforts}
                bind:value={efforts} />
            </Field.Field>
          {:else}
            <Field.Field>
              <Field.Label for={`${uid}-availability`} hint={m.provider_model_editor_visibility_help()}
                >{label}</Field.Label>
              <Select.Root
                type="single"
                value={policy}
                disabled={saving}
                onValueChange={(value: string) => {
                  if (value) {
                    policy = value as ProviderModelSelectionPolicy
                    void save(policy)
                  }
                }}>
                <Select.Trigger id={`${uid}-availability`} class="w-full"
                  >{providerModelSelectionPolicyLabel(policy, localeState.current)}</Select.Trigger>
                <Select.Content
                  ><Select.Group>
                    <Select.Item value="auto">{m.common_use_synced_status()}</Select.Item>
                    <Select.Item value="force_enabled">{m.common_always_allow()}</Select.Item>
                    <Select.Item value="force_disabled">{m.common_don_t_allow()}</Select.Item>
                  </Select.Group></Select.Content>
              </Select.Root>
            </Field.Field>
          {/if}
        </Field.Group>
        <div class="flex items-center justify-end gap-2 border-t border-border/60 px-4 py-3">
          <Button
            type="button"
            variant="outline"
            disabled={saving}
            onclick={() => {
              open = false
            }}>{m.common_cancel()}</Button>
          {#if field !== 'availability' || error}
            <Button type="submit" disabled={saving}
              >{#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save()}</Button>
          {:else if saving}<Spinner />{/if}
        </div>
      </form>
    {/if}
  </Popover.Content>
</Popover.Root>

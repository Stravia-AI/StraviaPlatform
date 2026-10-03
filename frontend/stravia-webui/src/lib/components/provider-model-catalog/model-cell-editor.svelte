<script lang="ts">
import type { Snippet } from 'svelte'
import { untrack } from 'svelte'
import { useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import PlusIcon from '@lucide/svelte/icons/plus'
import * as m from '$lib/paraglide/messages.js'
import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { localeState } from '$lib/localization.svelte'
import { specificationModalities, specificationModality, specificationReasoningEfforts } from '$lib/model-specification'
import { providerModelSelectionPolicyLabel } from '$lib/provider-model-labels'
import { inputValue } from '$lib/utils.js'
import type { ProviderModelDetail, ProviderModelSelectionPolicy, ProviderModelSummary } from '$lib/types'
import {
  buildProviderModelMetadataJson,
  normalizeProviderModelEfforts,
  providerModelCostFromMetadata,
} from '../provider-model-form.js'
import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'
import { Spinner } from '$lib/components/ui/spinner'
import * as Alert from '$lib/components/ui/alert'
import * as Field from '$lib/components/ui/field'
import * as Popover from '$lib/components/ui/popover'
import * as Select from '$lib/components/ui/select'

interface Props {
  providerId: string
  model: ProviderModelSummary
  field: 'context' | 'modalities' | 'efforts' | 'availability'
  children: Snippet
}
let { providerId, model, field, children }: Props = $props()
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
let customEffort = $state('')
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
let modalityOptions = $derived([
  ...new Set([
    ...specificationModalities.map(({ key }) => key),
    ...(detail?.metadata.modalities?.input ?? []),
    ...(detail?.metadata.modalities?.output ?? []),
    ...input,
    ...output,
  ]),
])
let effortOptions = $derived([
  ...new Set([
    ...specificationReasoningEfforts,
    ...normalizeProviderModelEfforts(detail?.metadata.reasoning_efforts ?? []),
    ...efforts,
  ]),
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
    customEffort = ''
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

function addCustomEffort(): void {
  const value = customEffort.trim()
  if (!value || ['default', 'null'].includes(value.toLowerCase())) return
  efforts = normalizeProviderModelEfforts([...efforts, value])
  customEffort = ''
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
        class="h-auto min-h-10 w-full min-w-0 justify-start whitespace-normal text-left"
        aria-label={`${label} ${model.id}`}>
        {@render children()}
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content
    align="start"
    class="flex max-h-[min(80vh,40rem)] w-80 max-w-[calc(100vw-2rem)] flex-col gap-4 overflow-y-auto">
    <Popover.Header>
      <Popover.Title class="break-all">{label} · {model.id}</Popover.Title>
    </Popover.Header>
    {#if loading}
      <div class="flex min-h-20 items-center justify-center" aria-busy="true"><Spinner /></div>
    {/if}
    {#if error}
      <Alert.Root variant="destructive" role="alert"
        ><Alert.Description class="whitespace-pre-line">{error}</Alert.Description></Alert.Root>
      {#if !detail && !loading}<Button variant="outline" onclick={() => load()}>{m.common_retry()}</Button>{/if}
    {/if}
    {#if detail && !loading}
      <form
        class="flex flex-col gap-4"
        onsubmit={(event) => {
          event.preventDefault()
          void save(field === 'availability' ? policy : undefined)
        }}>
        <Field.Group>
          {#if field === 'context'}
            <Field.Field>
              <Field.Label for={`${uid}-context`}>{label}</Field.Label>
              <Input
                id={`${uid}-context`}
                type="number"
                min="0"
                step="1"
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
                <Select.Root
                  type="multiple"
                  disabled={saving}
                  bind:value={
                    () => (target === 'input' ? input : output),
                    (values: string[]) => {
                      if (target === 'input') input = values
                      else output = values
                    }
                  }>
                  <Select.Trigger id={`${uid}-${target}`} class="w-full min-w-0">
                    <span class="truncate"
                      >{(target === 'input' ? input : output)
                        .map((value) => specificationModality(value).label())
                        .join(', ') || m.provider_model_editor_select_content_types()}</span>
                  </Select.Trigger>
                  <Select.Content
                    ><Select.Group>
                      {#each modalityOptions as value (value)}
                        {@const modality = specificationModality(value)}
                        <Select.Item {value} label={modality.label()}
                          ><modality.icon aria-hidden="true" />{modality.label()}</Select.Item>
                      {/each}
                    </Select.Group></Select.Content>
                </Select.Root>
              </Field.Field>
            {/each}
          {:else if field === 'efforts'}
            <Field.Field>
              <Field.Label for={`${uid}-efforts`}>{label}</Field.Label>
              <Select.Root type="multiple" bind:value={efforts} disabled={saving}>
                <Select.Trigger id={`${uid}-efforts`} class="w-full min-w-0"
                  ><span class="truncate">{efforts.join(', ') || m.provider_model_editor_select_efforts()}</span
                  ></Select.Trigger>
                <Select.Content
                  ><Select.Group
                    >{#each effortOptions as value (value)}<Select.Item {value}>{value}</Select.Item
                      >{/each}</Select.Group
                  ></Select.Content>
              </Select.Root>
              <Field.Description>{m.provider_model_editor_effort_help()}</Field.Description>
            </Field.Field>
            <Field.Field>
              <Field.Label for={`${uid}-custom`}>{m.provider_model_editor_custom_effort()}</Field.Label>
              <div class="flex gap-2">
                <Input
                  id={`${uid}-custom`}
                  class="min-w-0 flex-1"
                  bind:value={customEffort}
                  disabled={saving}
                  onkeydown={(event: KeyboardEvent) => {
                    if (event.key === 'Enter') {
                      event.preventDefault()
                      addCustomEffort()
                    }
                  }} />
                <Button
                  type="button"
                  variant="outline"
                  onclick={addCustomEffort}
                  disabled={saving ||
                    !customEffort.trim() ||
                    ['default', 'null'].includes(customEffort.trim().toLowerCase())}
                  ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_add_effort()}</Button>
              </div>
            </Field.Field>
          {:else}
            <Field.Field>
              <Field.Label for={`${uid}-availability`}>{label}</Field.Label>
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
              <Field.Description>{m.provider_model_editor_visibility_help()}</Field.Description>
            </Field.Field>
          {/if}
        </Field.Group>
        <div class="flex justify-end gap-2">
          <Button
            type="button"
            variant="outline"
            disabled={saving}
            onclick={() => {
              open = false
            }}>{m.common_cancel()}</Button>
          {#if field !== 'availability' || error}
            <Button type="submit" disabled={saving}
              >{#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_model()}</Button>
          {:else if saving}<Spinner />{/if}
        </div>
      </form>
    {/if}
  </Popover.Content>
</Popover.Root>

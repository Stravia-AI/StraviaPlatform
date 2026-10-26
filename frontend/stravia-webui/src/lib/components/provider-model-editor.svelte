<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import PlusIcon from '@lucide/svelte/icons/plus'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { tick, untrack, type Snippet } from 'svelte'

import FormSection from '#lib/components/form-section.svelte'
import ModalitySelect from '#lib/components/modality-select.svelte'
import ReasoningEffortSelect from '#lib/components/reasoning-effort-select.svelte'
import { localeState } from '#lib/localization.svelte.js'
import { providerModelSelectionPolicyLabel } from '#lib/provider-model-labels.js'
import type { ProviderModelDetail, ProviderModelMetadata, ProviderModelSelectionPolicy } from '#lib/types.js'
import {
  buildProviderModelMetadataJson,
  emptyProviderModelCost,
  emptyProviderModelPrices,
  normalizeProviderModelEfforts,
  providerModelCostFromMetadata,
  providerModelFormFingerprint,
  type ProviderModelCostForm,
  type ProviderModelPriceForm,
} from './provider-model-form.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Alert from '#lib/components/ui/alert/index.js'
import * as Collapsible from '#lib/components/ui/collapsible/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Textarea } from '#lib/components/ui/textarea/index.js'
import { inputValue } from '#lib/utils.js'

interface Props {
  detail: ProviderModelDetail
  draft?: boolean
  disabled?: boolean
  onSave: (metadataJson: string) => void
  onSelectionChange: (policy: ProviderModelSelectionPolicy) => void
  onDirtyChange?: (dirty: boolean) => void
  /** 上游模型的 RPM 限额；只有已存在的模型才有，由使用方提供并随页面一起保存。 */
  rpm?: Snippet
}

type StringField = 'name' | 'description'
type PriceField = keyof ProviderModelPriceForm
const modalityTargets = ['input', 'output'] as const

// 宽布局下的“添加可选字段”按钮贴设置列右缘，与控件右边缘对齐。
const alignedActions = 'flex flex-wrap gap-2 empty:hidden'
// 设置列够宽时短字段两列并排；窄时退回单列。
const pairGrid =
  'grid gap-x-6 gap-y-4 grid-cols-[repeat(auto-fill,minmax(max(min(100%,20rem),calc((100%-1.5rem)/2)),1fr))]'
const priceGrid = 'grid gap-3 grid-cols-[repeat(auto-fill,minmax(max(min(100%,10rem),calc((100%-2.25rem)/4)),1fr))]'
const stringFields: Array<{ key: StringField; label: () => string; multiline?: boolean }> = [
  { key: 'name', label: m.provider_model_field_name },
  { key: 'description', label: m.provider_model_field_description, multiline: true },
]
const priceFields: Array<{ key: PriceField; label: () => string }> = [
  { key: 'input', label: m.provider_model_field_input },
  { key: 'output', label: m.provider_model_field_output },
  { key: 'cache_read', label: m.provider_model_field_cache_read },
  { key: 'cache_write', label: m.provider_model_field_cache_write },
]
let { detail, draft = false, disabled = false, onSave, onSelectionChange, onDirtyChange, rpm }: Props = $props()
let metadata = $state<ProviderModelMetadata>({})
let cost = $state<ProviderModelCostForm>(emptyProviderModelCost())
let structuralErrors = $state<string[]>([])
let extensionsOpen = $state(false)
let errorAlert = $state<HTMLDivElement>()
let initialFingerprint = $state('')
let editorRoot = $state<HTMLFieldSetElement>()

const extensionEntries = $derived(Object.entries(detail.extensions ?? {}))
const confirmedMetadata = $derived(detail.metadata)
const currentFingerprint = $derived(fingerprint())
const dirty = $derived(Boolean(initialFingerprint) && currentFingerprint !== initialFingerprint)

$effect(() => {
  onDirtyChange?.(dirty)
})

$effect.pre(() => {
  void currentFingerprint
  const scrollOwner = editorRoot?.closest<HTMLElement>('[data-provider-model-scroll-owner]')
  if (!scrollOwner || scrollOwner.scrollTop === 0) return

  const scrollTop = scrollOwner.scrollTop
  void tick().then(() => {
    if (scrollOwner.isConnected) scrollOwner.scrollTop = scrollTop
  })
})

$effect(() => {
  const snapshot = $state.snapshot(confirmedMetadata)
  const nextMetadata = structuredClone(snapshot)
  if (nextMetadata.reasoning_efforts) {
    nextMetadata.reasoning_efforts = normalizeProviderModelEfforts(nextMetadata.reasoning_efforts)
  }
  metadata = nextMetadata
  cost = providerModelCostFromMetadata(snapshot)
  structuralErrors = []
  initialFingerprint = untrack(fingerprint)
})
$effect(() => {
  if (structuralErrors.length > 0) {
    void tick().then(() => errorAlert?.focus())
  }
})

function fingerprint(): string {
  return providerModelFormFingerprint($state.snapshot(metadata), $state.snapshot(cost))
}

function hasField(key: keyof ProviderModelMetadata): boolean {
  return Object.prototype.hasOwnProperty.call(metadata, key) && metadata[key] !== null
}

// 可选字段出现后把焦点移入新控件，键盘用户无需再找一次刚添加的输入。
async function focusControl(id: string): Promise<void> {
  await tick()
  document.getElementById(id)?.focus()
}

function addStringField(key: StringField): void {
  metadata[key] = ''
  void focusControl(`provider-model-${key}`)
}

function addModalities(): void {
  metadata.modalities = { input: [], output: [] }
  void focusControl('provider-model-input-modalities')
}

function addCost(): void {
  metadata.cost = { tiers: [] }
  void focusControl('provider-model-cost-input')
}

function removeField(key: keyof ProviderModelMetadata): void {
  delete metadata[key]
  if (key === 'cost') cost = emptyProviderModelCost()
}

function setStringField(key: StringField, value: string): void {
  metadata[key] = value
}

function setModalityValues(target: 'input' | 'output', values: string[]): void {
  metadata.modalities ??= { input: [], output: [] }
  metadata.modalities[target] = values
}

function setLimit(value: string): void {
  metadata.limit = { context: value === '' ? null : Number(value) }
}

function addTier(): void {
  cost.tiers.push({ ...emptyProviderModelPrices(), threshold: '' })
  metadata.cost ??= { tiers: [] }
  void focusControl(`provider-model-tier-${cost.tiers.length - 1}`)
}

function removeTier(index: number): void {
  cost.tiers.splice(index, 1)
}

export function submit(): void {
  if (disabled) return
  const result = buildProviderModelMetadataJson(detail.id, $state.snapshot(metadata), $state.snapshot(cost))
  structuralErrors = result.errors
  const { json } = result
  if (json) onSave(json)
}
</script>

{#snippet stringControl(field: (typeof stringFields)[number])}
  {#if hasField(field.key)}
    <Field.Field class={field.multiline ? 'col-span-full' : undefined}>
      <Field.Label for={`provider-model-${field.key}`}>{field.label()}</Field.Label>
      {#if field.multiline}
        <Textarea
          id={`provider-model-${field.key}`}
          class="min-h-24 resize-y"
          value={String(metadata[field.key] ?? '')}
          oninput={(event: Event) => setStringField(field.key, inputValue(event))} />
      {:else}
        <Input
          id={`provider-model-${field.key}`}
          value={String(metadata[field.key] ?? '')}
          oninput={(event: Event) => setStringField(field.key, inputValue(event))} />
      {/if}
    </Field.Field>
  {/if}
{/snippet}

<fieldset bind:this={editorRoot} {disabled} class="@container/model-editor flex min-w-0 flex-col gap-6">
  <FormSection title={m.provider_model_editor_model_information()}>
    <!-- 设置列够宽时短字段两列并排；描述是多行文本，独占一行。 -->
    <Field.Group class={`${pairGrid} empty:hidden`}>
      {#each stringFields.filter((field) => !field.multiline) as field (field.key)}
        {@render stringControl(field)}
      {/each}
      {#if !draft}
        <Field.Field>
          <Field.Label for="provider-model-selection" hint={m.provider_model_editor_visibility_help()}
            >{m.common_availability_adding_models()}</Field.Label>
          <Select.Root
            type="single"
            {disabled}
            value={detail.selection_policy}
            onValueChange={(value: string) => value && onSelectionChange(value as ProviderModelSelectionPolicy)}>
            <Select.Trigger
              id="provider-model-selection"
              class="w-full min-w-0"
              aria-label={m.common_availability_adding_models()}>
              {providerModelSelectionPolicyLabel(detail.selection_policy, localeState.current)}
            </Select.Trigger>
            <Select.Content>
              <Select.Group>
                <Select.Item value="auto">{m.common_use_synced_status()}</Select.Item>
                <Select.Item value="force_enabled">{m.common_always_allow()}</Select.Item>
                <Select.Item value="force_disabled">{m.common_don_t_allow()}</Select.Item>
              </Select.Group>
            </Select.Content>
          </Select.Root>
        </Field.Field>
      {/if}
      {#each stringFields.filter((field) => field.multiline) as field (field.key)}
        {@render stringControl(field)}
      {/each}
    </Field.Group>
    <div class={alignedActions}>
      {#each stringFields.filter((field) => !hasField(field.key)) as field (field.key)}
        <Button type="button" variant="outline" size="sm" onclick={() => addStringField(field.key)}>
          <PlusIcon data-icon="inline-start" />{field.label()}
        </Button>
      {/each}
    </div>
  </FormSection>

  <FormSection title={m.provider_model_editor_capabilities()}>
    <Field.Group class={pairGrid}>
      <Field.Field>
        <Field.Label for="provider-model-limit-context">{m.provider_model_editor_context()}</Field.Label>
        <Input
          id="provider-model-limit-context"
          type="number"
          min="0"
          step="1"
          class="font-technical"
          value={metadata.limit?.context ?? ''}
          placeholder={m.model_specification_not_registered()}
          oninput={(event: Event) => setLimit(inputValue(event))} />
      </Field.Field>
      <Field.Field>
        <Field.Label for="provider-model-reasoning-efforts" hint={m.provider_model_editor_effort_help()}
          >{m.provider_model_editor_reasoning_efforts()}</Field.Label>
        <ReasoningEffortSelect
          id="provider-model-reasoning-efforts"
          {disabled}
          registered={detail.metadata.reasoning_efforts}
          bind:value={
            () => metadata.reasoning_efforts ?? [], (values: string[]) => (metadata.reasoning_efforts = values)
          } />
      </Field.Field>
      {#if hasField('modalities') && metadata.modalities}
        {#each modalityTargets as target (target)}
          <Field.Field>
            <Field.Label for={`provider-model-${target}-modalities`}>
              {target === 'input'
                ? m.provider_model_editor_accepted_input_types()
                : m.provider_model_editor_generated_output_types()}
            </Field.Label>
            <ModalitySelect
              id={`provider-model-${target}-modalities`}
              registered={detail.metadata.modalities?.[target]}
              {target}
              {disabled}
              bind:value={
                () => metadata.modalities?.[target] ?? [], (values: string[]) => setModalityValues(target, values)
              } />
          </Field.Field>
        {/each}
      {/if}
    </Field.Group>
    <div class={alignedActions}>
      {#if !hasField('modalities')}
        <Button type="button" variant="outline" size="sm" onclick={addModalities}
          ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_modalities()}</Button>
      {/if}
    </div>
  </FormSection>

  <FormSection title={rpm ? m.provider_model_editor_pricing_limits() : m.provider_model_editor_pricing()}>
    <div class="flex flex-col gap-4">
      {#if rpm}
        <div class={pairGrid}>{@render rpm()}</div>
      {/if}
      {#if hasField('cost')}
        <div class="flex flex-col gap-3">
          <div class="flex items-center justify-between gap-3">
            <p class="flex items-center gap-2 text-sm font-medium">
              {m.provider_model_editor_base_pricing()}
              <Field.Help hint={m.provider_model_editor_pricing_unit_help()} />
            </p>
            <Button type="button" variant="ghost" size="sm" onclick={() => removeField('cost')}
              ><Trash2Icon data-icon="inline-start" />{m.common_remove()}</Button>
          </div>
          <div class={priceGrid}>
            {#each priceFields as field (field.key)}
              <Field.Field orientation="vertical">
                <Field.Label for={`provider-model-cost-${field.key}`}>{field.label()}</Field.Label>
                <Input
                  id={`provider-model-cost-${field.key}`}
                  class="font-technical"
                  inputmode="decimal"
                  bind:value={cost.base[field.key]}
                  placeholder="0.00" />
              </Field.Field>
            {/each}
          </div>
          {#each cost.tiers as tier, index (tier)}
            <div class="flex flex-col gap-3 border-t border-dashed pt-4">
              <div class="flex items-end justify-between gap-3">
                <Field.Field orientation="vertical" class="max-w-64">
                  <Field.Label for={`provider-model-tier-${index}`}
                    >{m.provider_model_editor_tier_value_context_threshold({ index: index + 1 })}</Field.Label>
                  <Input
                    id={`provider-model-tier-${index}`}
                    type="number"
                    min="0"
                    step="1"
                    class="font-technical"
                    bind:value={tier.threshold} />
                </Field.Field>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  class="shrink-0"
                  aria-label={m.provider_model_editor_remove_tier()}
                  onclick={() => removeTier(index)}><Trash2Icon /></Button>
              </div>
              <div class={priceGrid}>
                {#each priceFields as field (field.key)}
                  <Field.Field orientation="vertical">
                    <Field.Label for={`provider-model-tier-${index}-${field.key}`}>{field.label()}</Field.Label>
                    <Input
                      id={`provider-model-tier-${index}-${field.key}`}
                      class="font-technical"
                      inputmode="decimal"
                      bind:value={tier[field.key]}
                      placeholder="0.00" />
                  </Field.Field>
                {/each}
              </div>
            </div>
          {/each}
          <Button type="button" variant="outline" size="sm" class="self-start" onclick={addTier}
            ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_pricing_tier()}</Button>
        </div>
      {:else}
        <Button type="button" variant="outline" size="sm" class="self-start" onclick={addCost}
          ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_pricing()}</Button>
      {/if}
    </div>
  </FormSection>

  {#if extensionEntries.length > 0}
    <Collapsible.Root class="border-t pt-2" bind:open={extensionsOpen}>
      <Collapsible.Trigger
        type="button"
        class="group/extensions flex w-full items-center gap-2 rounded-md py-2 text-left text-sm font-medium text-muted-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:text-foreground"
        ><ChevronRightIcon
          class="size-4 shrink-0 transition-transform duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-data-[state=open]/extensions:rotate-90"
          aria-hidden="true" />{m.provider_model_editor_extension_fields_read_only()} · {extensionEntries.length}</Collapsible.Trigger>
      <Collapsible.Content>
        <pre
          class="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 font-technical text-xs">{JSON.stringify(
            detail.extensions,
            null,
            2,
          )}</pre>
      </Collapsible.Content>
    </Collapsible.Root>
  {/if}

  {#if structuralErrors.length > 0}
    <Alert.Root bind:ref={errorAlert} tabindex={-1} variant="destructive" role="alert">
      <Alert.Title>{m.provider_model_editor_cannot_save()}</Alert.Title>
      <Alert.Description
        ><ul class="list-disc pl-5">
          {#each structuralErrors as error (error)}<li>{error}</li>{/each}
        </ul></Alert.Description>
    </Alert.Root>
  {/if}
</fieldset>

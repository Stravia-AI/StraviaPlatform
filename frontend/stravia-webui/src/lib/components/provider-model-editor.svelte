<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import PlusIcon from '@lucide/svelte/icons/plus'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { tick, untrack } from 'svelte'

import { localeState } from '$lib/localization.svelte'
import { providerModelSelectionPolicyLabel } from '$lib/provider-model-labels'
import type {
  ProviderModelDetail,
  ProviderModelMetadata,
  ProviderModelReasoningOption,
  ProviderModelSelectionPolicy,
} from '$lib/types'
import {
  buildProviderModelMetadataJson,
  emptyProviderModelCost,
  emptyProviderModelPrices,
  providerModelCostFromMetadata,
  providerModelFormFingerprint,
  type ProviderModelCostForm,
  type ProviderModelPriceForm,
} from './provider-model-form.js'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Switch } from '$lib/components/ui/switch'
import { Textarea } from '$lib/components/ui/textarea'

interface Props {
  detail: ProviderModelDetail
  draft?: boolean
  onSave: (metadataJson: string) => void
  onSelectionChange: (policy: ProviderModelSelectionPolicy) => void
  onDirtyChange?: (dirty: boolean) => void
}

type StringField = 'name' | 'description'
type BooleanField = 'attachment' | 'reasoning' | 'tool_call' | 'structured_output' | 'temperature'
type PriceField = keyof ProviderModelPriceForm
const reasoningOptionTypes: ProviderModelReasoningOption['type'][] = ['toggle', 'effort', 'budget_tokens']
const knownEffortValues: Array<string | null> = [
  'none',
  'minimal',
  'low',
  'medium',
  'high',
  'xhigh',
  'max',
  'default',
  null,
]
const knownModalities = ['text', 'image', 'audio', 'video', 'pdf']
const modalityTargets = ['input', 'output'] as const

const stringFields: Array<{ key: StringField; label: () => string; multiline?: boolean }> = [
  { key: 'name', label: m.provider_model_field_name },
  { key: 'description', label: m.provider_model_field_description, multiline: true },
]
const booleanFields: Array<{ key: BooleanField; label: () => string }> = [
  { key: 'attachment', label: m.provider_model_field_attachments },
  { key: 'reasoning', label: m.provider_model_field_reasoning },
  { key: 'tool_call', label: m.provider_model_field_tool_calls },
  { key: 'structured_output', label: m.provider_model_field_structured_output },
  { key: 'temperature', label: m.provider_model_field_temperature },
]
const priceFields: Array<{ key: PriceField; label: () => string }> = [
  { key: 'input', label: m.provider_model_field_input },
  { key: 'output', label: m.provider_model_field_output },
  { key: 'reasoning', label: m.provider_model_field_reasoning },
  { key: 'cache_read', label: m.provider_model_field_cache_read },
  { key: 'cache_write', label: m.provider_model_field_cache_write },
  { key: 'input_audio', label: m.provider_model_field_audio_input },
  { key: 'output_audio', label: m.provider_model_field_audio_output },
]
const visiblePriceFields = priceFields.filter(
  ({ key }) => key !== 'reasoning' && key !== 'input_audio' && key !== 'output_audio',
)
let { detail, draft = false, onSave, onSelectionChange, onDirtyChange }: Props = $props()
let metadata = $state<ProviderModelMetadata>({})
let cost = $state<ProviderModelCostForm>(emptyProviderModelCost())
let structuralErrors = $state<string[]>([])
let advancedOpen = $state(false)
let errorAlert = $state<HTMLDivElement>()
let initialFingerprint = $state('')
let editorRoot = $state<HTMLDivElement>()

const semanticWarnings = $derived.by(() => {
  const warnings: string[] = []
  const context = metadata.limit?.context
  if (context != null && metadata.limit?.input != null && metadata.limit.input > context) {
    warnings.push(m.provider_model_editor_input_limit_exceeds_context_limit())
  }
  if (context != null && metadata.limit?.output != null && metadata.limit.output > context) {
    warnings.push(m.provider_model_editor_output_limit_exceeds_context_limit())
  }
  return warnings
})
const extensionEntries = $derived(Object.entries(detail.extensions ?? {}))
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
  const snapshot = $state.snapshot(detail.metadata)
  metadata = structuredClone(snapshot)
  cost = providerModelCostFromMetadata(snapshot)
  structuralErrors = []
  initialFingerprint = untrack(fingerprint)
})
$effect(() => {
  if (structuralErrors.length > 0) {
    advancedOpen = true
    void tick().then(() => errorAlert?.focus())
  }
})

function fingerprint(): string {
  return providerModelFormFingerprint($state.snapshot(metadata), $state.snapshot(cost))
}

function hasField(key: keyof ProviderModelMetadata): boolean {
  return Object.prototype.hasOwnProperty.call(metadata, key) && metadata[key] !== null
}

function addStringField(key: StringField): void {
  metadata[key] = ''
}

function removeField(key: keyof ProviderModelMetadata): void {
  delete metadata[key]
  if (key === 'cost') cost = emptyProviderModelCost()
}

function setStringField(key: StringField, value: string): void {
  metadata[key] = value
}

function addBooleanField(key: BooleanField): void {
  metadata[key] = true
}

function setBooleanField(key: BooleanField, value: boolean): void {
  metadata[key] = value
}

function modalityOptions(target: 'input' | 'output'): string[] {
  const values = [...knownModalities]
  const knownValues = new Set(values)
  for (const value of metadata.modalities?.[target] ?? []) {
    if (!knownValues.has(value)) values.push(value)
  }
  return values
}

function setModalityValues(target: 'input' | 'output', selectedValues: string[]): void {
  metadata.modalities ??= { input: [], output: [] }
  const selected = new Set(selectedValues)
  metadata.modalities[target] = modalityOptions(target).filter((value) => selected.has(value))
}

function setLimit(key: 'context' | 'input' | 'output', value: string): void {
  metadata.limit ??= {}
  metadata.limit[key] = value === '' ? null : Number(value)
}

function addReasoningOption(type: ProviderModelReasoningOption['type'] = 'toggle'): void {
  if (hasReasoningOption(type)) return
  metadata.reasoning_options ??= []
  metadata.reasoning_options.push(reasoningOptionForType(type))
}

function hasReasoningOption(type: ProviderModelReasoningOption['type'], exceptIndex = -1): boolean {
  return metadata.reasoning_options?.some((option, index) => index !== exceptIndex && option.type === type) ?? false
}

function reasoningOptionForType(type: ProviderModelReasoningOption['type']): ProviderModelReasoningOption {
  if (type === 'effort') return { type, values: ['low', 'medium', 'high'] }
  if (type === 'budget_tokens') return { type, min: -1, max: 32768 }
  return { type: 'toggle' }
}

function changeReasoningType(index: number, type: ProviderModelReasoningOption['type']): void {
  if (hasReasoningOption(type, index)) return
  metadata.reasoning_options ??= []
  metadata.reasoning_options[index] = reasoningOptionForType(type)
}

function effortValueKey(value: string | null): string {
  return JSON.stringify(value)
}

function effortValueLabel(value: string | null): string {
  return value ?? m.provider_model_editor_use_service_default()
}

function effortValueOptions(
  option: Extract<ProviderModelReasoningOption, { type: 'effort' }>,
): Array<{ key: string; label: string; value: string | null }> {
  const values = [...knownEffortValues]
  const knownKeys = new Set(values.map(effortValueKey))
  for (const value of option.values) {
    if (!knownKeys.has(effortValueKey(value))) values.push(value)
  }
  return values.map((value) => ({ key: effortValueKey(value), label: effortValueLabel(value), value }))
}

function setEffortValues(
  option: Extract<ProviderModelReasoningOption, { type: 'effort' }>,
  selectedKeys: string[],
): void {
  const selected = new Set(selectedKeys)
  option.values = effortValueOptions(option)
    .filter(({ key }) => selected.has(key))
    .map(({ value }) => value)
}
function setBudgetValue(
  option: Extract<ProviderModelReasoningOption, { type: 'budget_tokens' }>,
  key: 'min' | 'max',
  value: string,
): void {
  option[key] = value === '' ? null : Number(value)
}

function removeReasoningOption(index: number): void {
  metadata.reasoning_options?.splice(index, 1)
}

function interleavedMode(): 'unset' | 'enabled' | 'disabled' | 'field' {
  if (!hasField('interleaved')) return 'unset'
  if (typeof metadata.interleaved === 'object') return 'field'
  return metadata.interleaved ? 'enabled' : 'disabled'
}

function setInterleavedMode(mode: 'unset' | 'enabled' | 'disabled' | 'field'): void {
  if (mode === 'unset') delete metadata.interleaved
  else if (mode === 'field') metadata.interleaved = { field: '' }
  else metadata.interleaved = mode === 'enabled'
}

function addTier(): void {
  cost.tiers.push({ ...emptyProviderModelPrices(), threshold: '' })
  metadata.cost ??= { tiers: [] }
}

function removeTier(index: number): void {
  cost.tiers.splice(index, 1)
}

export function submit(): void {
  const result = buildProviderModelMetadataJson(detail.id, $state.snapshot(metadata), $state.snapshot(cost))
  structuralErrors = result.errors
  const { json } = result
  if (json) onSave(json)
}
</script>

<div bind:this={editorRoot} class="flex flex-col gap-5">
  <div class="flex flex-wrap items-start justify-between gap-3 border-b pb-4">
    <div class="min-w-0">
      <div class="flex flex-wrap items-center gap-2">
        <h3 class="truncate text-base font-semibold">{metadata.name || detail.id}</h3>
        <Badge variant={detail.available ? 'secondary' : 'outline'}>
          {detail.available ? m.common_used() : m.common_unavailable()}
        </Badge>
        <Badge variant="outline">
          {detail.source_kind === 'manual' ? m.common_added_manually() : m.common_synced()}
        </Badge>
      </div>
      <p class="mt-1 break-all font-technical text-xs text-muted-foreground">{detail.id}</p>
    </div>
    {#if !draft}
      <div class="min-w-64 rounded-lg border bg-muted/20 p-3">
        <div class="flex items-center justify-between gap-3">
          <div>
            <p class="text-sm font-medium">{m.provider_model_editor_available_adding_models()}</p>
            <p class="mt-1 text-xs text-muted-foreground">
              {m.provider_model_editor_visibility_help()}
            </p>
          </div>
          <Select.Root
            type="single"
            value={detail.selection_policy}
            onValueChange={(value) => value && onSelectionChange(value as ProviderModelSelectionPolicy)}>
            <Select.Trigger class="w-40" aria-label={m.common_availability_adding_models()}>
              {providerModelSelectionPolicyLabel(detail.selection_policy, localeState.current)}
            </Select.Trigger>
            <Select.Content>
              <Select.Item value="auto">{m.common_use_synced_status()}</Select.Item>
              <Select.Item value="force_enabled">{m.common_always_allow()}</Select.Item>
              <Select.Item value="force_disabled">{m.common_don_t_allow()}</Select.Item>
            </Select.Content>
          </Select.Root>
        </div>
      </div>
    {/if}
  </div>

  <section class="flex flex-col gap-3">
    <div class="flex items-center justify-between gap-3">
      <h4 class="text-sm font-semibold">{m.provider_model_editor_model_information()}</h4>
    </div>
    <div class="grid gap-3 sm:grid-cols-2">
      <Field.Field>
        <Field.Label for="provider-model-id">{m.provider_model_editor_model_id()}</Field.Label>
        <Input id="provider-model-id" class="font-technical" value={detail.id} readonly />
      </Field.Field>
      {#each stringFields as field (field.key)}
        {#if hasField(field.key)}
          <Field.Field class={field.multiline ? 'sm:col-span-2' : ''}>
            <Field.Label for={`provider-model-${field.key}`}>{field.label()}</Field.Label>
            {#if field.multiline}
              <Textarea
                id={`provider-model-${field.key}`}
                value={String(metadata[field.key] ?? '')}
                oninput={(event) => setStringField(field.key, event.currentTarget.value)} />
            {:else}
              <Input
                id={`provider-model-${field.key}`}
                value={String(metadata[field.key] ?? '')}
                oninput={(event) => setStringField(field.key, event.currentTarget.value)} />
            {/if}
          </Field.Field>
        {/if}
      {/each}
    </div>
    <div class="flex flex-wrap gap-1.5">
      {#each stringFields.filter((field) => !hasField(field.key)) as field (field.key)}
        <Button type="button" variant="outline" size="xs" onclick={() => addStringField(field.key)}>
          <PlusIcon data-icon="inline-start" />{field.label()}
        </Button>
      {/each}
    </div>
  </section>

  <section class="flex flex-col gap-3 border-t pt-4">
    <h4 class="text-sm font-semibold">{m.provider_model_editor_supported_features()}</h4>
    <div class="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
      {#each booleanFields as field (field.key)}
        {#if hasField(field.key)}
          <div class="flex min-h-10 items-center justify-between gap-3 rounded-lg border px-3 py-2">
            <Field.Label for={`provider-model-${field.key}`}>{field.label()}</Field.Label>
            <Switch
              id={`provider-model-${field.key}`}
              size="sm"
              checked={metadata[field.key] === true}
              onCheckedChange={(checked) => setBooleanField(field.key, checked)} />
          </div>
        {/if}
      {/each}
    </div>
    <div class="flex flex-wrap gap-1.5">
      {#each booleanFields.filter((field) => !hasField(field.key)) as field (field.key)}
        <Button type="button" variant="outline" size="xs" onclick={() => addBooleanField(field.key)}>
          <PlusIcon data-icon="inline-start" />{field.label()}
        </Button>
      {/each}
    </div>
  </section>

  <section class="flex flex-col gap-3 border-t pt-4">
    <div class="flex items-center justify-between gap-3">
      <h4 class="text-sm font-semibold">{m.provider_model_editor_inputs_outputs_limits()}</h4>
    </div>
    {#if hasField('modalities') && metadata.modalities}
      <div class="rounded-lg border p-3">
        <p class="mb-3 text-sm font-medium">{m.provider_model_editor_supported_content_types()}</p>
        <div class="grid gap-3 sm:grid-cols-2">
          {#each modalityTargets as target (target)}
            <Field.Field>
              <Field.Label for={`provider-model-${target}-modalities`}>
                {target === 'input'
                  ? m.provider_model_editor_accepted_input_types()
                  : m.provider_model_editor_generated_output_types()}
              </Field.Label>
              <Select.Root
                type="multiple"
                value={metadata.modalities[target]}
                onValueChange={(values) => setModalityValues(target, values)}>
                <Select.Trigger
                  id={`provider-model-${target}-modalities`}
                  class="w-full min-w-0"
                  data-modality-select={target}>
                  <span class="truncate">
                    {metadata.modalities[target].length > 0
                      ? metadata.modalities[target].join(', ')
                      : m.provider_model_editor_select_content_types()}
                  </span>
                </Select.Trigger>
                <Select.Content>
                  <Select.Group>
                    {#each modalityOptions(target) as value (value)}
                      <Select.Item {value}>{value}</Select.Item>
                    {/each}
                  </Select.Group>
                </Select.Content>
              </Select.Root>
            </Field.Field>
          {/each}
        </div>
      </div>
    {/if}
    {#if hasField('limit') && metadata.limit}
      <div class="rounded-lg border p-3">
        <p class="mb-3 text-sm font-medium">{m.provider_model_editor_token_limits()}</p>
        <div class="grid gap-3 sm:grid-cols-3">
          {#each ['context', 'input', 'output'] as key (key)}
            <Field.Field>
              <Field.Label for={`provider-model-limit-${key}`}>{key}</Field.Label>
              <Input
                id={`provider-model-limit-${key}`}
                type="number"
                min="0"
                step="1"
                value={metadata.limit[key as keyof typeof metadata.limit] ?? ''}
                oninput={(event) => setLimit(key as 'context' | 'input' | 'output', event.currentTarget.value)} />
            </Field.Field>
          {/each}
        </div>
      </div>
    {/if}
    <div class="flex flex-wrap gap-1.5">
      {#if !hasField('modalities')}
        <Button
          type="button"
          variant="outline"
          size="xs"
          onclick={() => (metadata.modalities = { input: [], output: [] })}
          ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_modalities()}</Button>
      {/if}
      {#if !hasField('limit')}
        <Button type="button" variant="outline" size="xs" onclick={() => (metadata.limit = {})}
          ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_token_limits()}</Button>
      {/if}
    </div>
  </section>

  <details class="rounded-xl border bg-muted/10" bind:open={advancedOpen}>
    <summary class="min-h-12 cursor-pointer content-center px-4 text-sm font-semibold">
      {m.provider_model_editor_advanced_model_settings()}
      <span class="ml-2 font-normal text-muted-foreground">
        {m.provider_model_editor_advanced_fields_help()}
      </span>
    </summary>
    <div class="flex flex-col gap-5 border-t p-4">
      <section class="flex flex-col gap-3 border-t pt-4">
        <div class="flex flex-wrap items-center justify-between gap-3">
          <h4 class="text-sm font-semibold">{m.provider_model_editor_reasoning_behavior()}</h4>
          <div class="flex items-center gap-2">
            <span class="text-xs text-muted-foreground">{m.common_interleaved()}</span>
            <Select.Root
              type="single"
              value={interleavedMode()}
              onValueChange={(value) => value && setInterleavedMode(value as ReturnType<typeof interleavedMode>)}>
              <Select.Trigger class="w-32">{interleavedMode()}</Select.Trigger>
              <Select.Content>
                <Select.Item value="unset">{m.provider_model_editor_unspecified()}</Select.Item>
                <Select.Item value="enabled">{m.common_enable_action()}</Select.Item>
                <Select.Item value="disabled">{m.common_disable_action()}</Select.Item>
                <Select.Item value="field">{m.provider_model_editor_request_field()}</Select.Item>
              </Select.Content>
            </Select.Root>
          </div>
        </div>
        {#if typeof metadata.interleaved === 'object' && metadata.interleaved}
          <Field.Field>
            <Field.Label for="provider-model-interleaved-field"
              >{m.provider_model_editor_interleaved_request_field()}</Field.Label>
            <Input
              id="provider-model-interleaved-field"
              class="font-technical"
              bind:value={metadata.interleaved.field} />
          </Field.Field>
        {/if}
        {#if hasField('reasoning_options') && metadata.reasoning_options}
          <div class="flex flex-col gap-2">
            {#each metadata.reasoning_options as option, index (index)}
              <div class="grid gap-2 rounded-lg border p-3 sm:grid-cols-[10rem_1fr_auto]">
                <Select.Root
                  type="single"
                  value={option.type}
                  onValueChange={(value) =>
                    value && changeReasoningType(index, value as ProviderModelReasoningOption['type'])}>
                  <Select.Trigger>{option.type}</Select.Trigger>
                  <Select.Content>
                    <Select.Group>
                      {#each reasoningOptionTypes as type (type)}
                        {#if type === option.type || !hasReasoningOption(type, index)}
                          <Select.Item value={type}>{type}</Select.Item>
                        {/if}
                      {/each}
                    </Select.Group>
                  </Select.Content>
                </Select.Root>
                {#if option.type === 'effort'}
                  <Select.Root
                    type="multiple"
                    value={option.values.map(effortValueKey)}
                    onValueChange={(values) => setEffortValues(option, values)}>
                    <Select.Trigger class="w-full min-w-0" data-effort-values-select>
                      <span class="truncate">{option.values.map(effortValueLabel).join(', ')}</span>
                    </Select.Trigger>
                    <Select.Content>
                      <Select.Group>
                        {#each effortValueOptions(option) as choice (choice.key)}
                          <Select.Item value={choice.key} label={choice.label}>{choice.label}</Select.Item>
                        {/each}
                      </Select.Group>
                    </Select.Content>
                  </Select.Root>
                {:else if option.type === 'budget_tokens'}
                  <div class="grid grid-cols-2 gap-2">
                    <Input
                      type="number"
                      step="1"
                      value={option.min ?? ''}
                      aria-label={m.provider_model_editor_minimum_reasoning_tokens()}
                      oninput={(event) => setBudgetValue(option, 'min', event.currentTarget.value)} />
                    <Input
                      type="number"
                      min="0"
                      step="1"
                      value={option.max ?? ''}
                      aria-label={m.provider_model_editor_maximum_reasoning_tokens()}
                      oninput={(event) => setBudgetValue(option, 'max', event.currentTarget.value)} />
                  </div>
                {:else}
                  <p class="self-center text-xs text-muted-foreground">
                    {m.provider_model_editor_boolean_reasoning_control()}
                  </p>
                {/if}
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-sm"
                  aria-label={m.provider_model_editor_remove_reasoning_option()}
                  onclick={() => removeReasoningOption(index)}><Trash2Icon /></Button>
              </div>
            {/each}
            <div class="flex flex-wrap gap-2">
              {#if !hasReasoningOption('toggle')}
                <Button
                  type="button"
                  variant="outline"
                  size="xs"
                  data-reasoning-option-add="toggle"
                  onclick={() => addReasoningOption()}>
                  <PlusIcon data-icon="inline-start" />toggle
                </Button>
              {/if}
              {#if !hasReasoningOption('effort')}
                <Button
                  type="button"
                  variant="outline"
                  size="xs"
                  data-reasoning-option-add="effort"
                  onclick={() => addReasoningOption('effort')}>
                  <PlusIcon data-icon="inline-start" />effort
                </Button>
              {/if}
              {#if !hasReasoningOption('budget_tokens')}
                <Button
                  type="button"
                  variant="outline"
                  size="xs"
                  data-reasoning-option-add="budget_tokens"
                  onclick={() => addReasoningOption('budget_tokens')}>
                  <PlusIcon data-icon="inline-start" />budget
                </Button>
              {/if}
              <Button type="button" variant="ghost" size="xs" onclick={() => removeField('reasoning_options')}
                >{m.provider_model_editor_remove_group()}</Button>
            </div>
          </div>
        {:else}
          <Button type="button" variant="outline" size="xs" onclick={() => (metadata.reasoning_options = [])}
            ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_reasoning_options()}</Button>
        {/if}
      </section>

      <section class="flex flex-col gap-3 border-t pt-4">
        <div class="flex items-center justify-between gap-3">
          <div>
            <h4 class="text-sm font-semibold">{m.provider_model_editor_pricing()}</h4>
            <p class="text-xs text-muted-foreground">
              {m.provider_model_editor_pricing_unit_help()}
            </p>
          </div>
          {#if hasField('cost')}
            <Button type="button" variant="ghost" size="sm" onclick={() => removeField('cost')}
              ><Trash2Icon data-icon="inline-start" />{m.common_remove()}</Button>
          {/if}
        </div>
        {#if hasField('cost')}
          <div class="rounded-lg border p-3">
            <p class="mb-3 text-sm font-medium">{m.provider_model_editor_base_pricing()}</p>
            <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
              {#each visiblePriceFields as field (field.key)}
                <Field.Field>
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
          </div>
          {#each cost.tiers as tier, index (index)}
            <div class="rounded-lg border p-3">
              <div class="mb-3 flex items-end justify-between gap-3">
                <Field.Field class="max-w-64">
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
                  size="icon-sm"
                  aria-label={m.provider_model_editor_remove_tier()}
                  onclick={() => removeTier(index)}><Trash2Icon /></Button>
              </div>
              <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
                {#each visiblePriceFields as field (field.key)}
                  <Field.Field>
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
          <Button type="button" variant="outline" size="xs" onclick={addTier}
            ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_pricing_tier()}</Button>
        {:else}
          <Button type="button" variant="outline" size="xs" onclick={() => (metadata.cost = { tiers: [] })}
            ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_pricing()}</Button>
        {/if}
      </section>

      {#if extensionEntries.length > 0}
        <details class="rounded-lg border bg-muted/20 p-3">
          <summary class="cursor-pointer text-sm font-medium"
            >{m.provider_model_editor_extension_fields_read_only()} · {extensionEntries.length}</summary>
          <pre
            class="mt-3 max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-md bg-muted p-3 font-technical text-xs">{JSON.stringify(
              detail.extensions,
              null,
              2,
            )}</pre>
        </details>
      {/if}
    </div>
  </details>

  {#if semanticWarnings.length > 0}
    <div class="rounded-lg border border-warning/40 bg-warning/5 p-3 text-sm text-warning">
      <p class="font-medium">{m.provider_model_editor_review_saving()}</p>
      <ul class="mt-1 list-disc pl-5">
        {#each semanticWarnings as warning (warning)}<li>{warning}</li>{/each}
      </ul>
    </div>
  {/if}
  {#if structuralErrors.length > 0}
    <div
      bind:this={errorAlert}
      tabindex="-1"
      class="rounded-lg border border-destructive/40 bg-destructive/5 p-3 text-sm text-destructive"
      role="alert">
      <p class="font-medium">{m.provider_model_editor_cannot_save()}</p>
      <ul class="mt-1 list-disc pl-5">
        {#each structuralErrors as error (error)}<li>{error}</li>{/each}
      </ul>
    </div>
  {/if}
</div>

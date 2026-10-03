<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import PlusIcon from '@lucide/svelte/icons/plus'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { tick, untrack } from 'svelte'

import { specificationModalities, specificationModality, specificationReasoningEfforts } from '$lib/model-specification'
import ModalityIcons from '$lib/components/modality-icons.svelte'
import { localeState } from '$lib/localization.svelte'
import { providerModelSelectionPolicyLabel } from '$lib/provider-model-labels'
import type { ProviderModelDetail, ProviderModelMetadata, ProviderModelSelectionPolicy } from '$lib/types'
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
import { Button } from '$lib/components/ui/button'
import * as Alert from '$lib/components/ui/alert'
import * as Collapsible from '$lib/components/ui/collapsible'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Textarea } from '$lib/components/ui/textarea'
import { inputValue } from '$lib/utils.js'

interface Props {
  detail: ProviderModelDetail
  draft?: boolean
  onSave: (metadataJson: string) => void
  onSelectionChange: (policy: ProviderModelSelectionPolicy) => void
  onDirtyChange?: (dirty: boolean) => void
}

type StringField = 'name' | 'description'
type PriceField = keyof ProviderModelPriceForm
const knownModalities: string[] = specificationModalities.map(({ key }) => key)
const modalityTargets = ['input', 'output'] as const

const stringFields: Array<{ key: StringField; label: () => string; multiline?: boolean }> = [
  { key: 'name', label: m.provider_model_field_name },
  { key: 'description', label: m.provider_model_field_description, multiline: true },
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
let { detail, draft = false, onSave, onSelectionChange, onDirtyChange }: Props = $props()
let metadata = $state<ProviderModelMetadata>({})
let cost = $state<ProviderModelCostForm>(emptyProviderModelCost())
let structuralErrors = $state<string[]>([])
let customEffort = $state('')
let extensionsOpen = $state(false)
let errorAlert = $state<HTMLDivElement>()
let initialFingerprint = $state('')
let editorRoot = $state<HTMLDivElement>()

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
  const nextMetadata = structuredClone(snapshot)
  if (nextMetadata.reasoning_efforts) {
    nextMetadata.reasoning_efforts = normalizeProviderModelEfforts(nextMetadata.reasoning_efforts)
  }
  metadata = nextMetadata
  cost = providerModelCostFromMetadata(snapshot)
  structuralErrors = []
  customEffort = ''
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

function setLimit(value: string): void {
  metadata.limit = { context: value === '' ? null : Number(value) }
}

function effortOptions(): string[] {
  return [...new Set([...specificationReasoningEfforts, ...(metadata.reasoning_efforts ?? [])])]
}

function setEffortValues(values: string[]): void {
  metadata.reasoning_efforts = effortOptions().filter((value) => values.includes(value))
}

function addCustomEffort(): void {
  const value = customEffort.trim()
  if (!value || ['default', 'null'].includes(value.toLowerCase())) return
  metadata.reasoning_efforts = [...new Set([...(metadata.reasoning_efforts ?? []), value])]
  customEffort = ''
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

<div bind:this={editorRoot} class="@container/model-editor flex min-w-0 flex-col gap-5">
  {#if !draft}
    <Field.Group class="rounded-xl bg-muted/30 p-4">
      <Field.Field orientation="horizontal" class="[&>[data-slot=field-layout]]:flex-wrap">
        <Field.Content class="min-w-48 flex-1">
          <Field.Label for="provider-model-selection">{m.provider_model_editor_available_adding_models()}</Field.Label>
          <Field.Description id="provider-model-selection-help"
            >{m.provider_model_editor_visibility_help()}</Field.Description>
        </Field.Content>
        <Select.Root
          type="single"
          value={detail.selection_policy}
          onValueChange={(value: string) => value && onSelectionChange(value as ProviderModelSelectionPolicy)}>
          <Select.Trigger
            id="provider-model-selection"
            class="min-h-10 w-full shrink-0 @sm/model-editor:w-44"
            aria-label={m.common_availability_adding_models()}
            aria-describedby="provider-model-selection-help">
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
    </Field.Group>
  {/if}

  <section class="flex min-w-0 flex-col gap-4 border-t pt-4">
    <div class="flex items-center justify-between gap-3">
      <h4 class="text-sm font-semibold">{m.provider_model_editor_model_information()}</h4>
    </div>
    <Field.Group class="grid gap-4 @xl/model-editor:grid-cols-2">
      <Field.Field orientation="vertical">
        <Field.Label for="provider-model-id">{m.provider_model_editor_model_id()}</Field.Label>
        <Input id="provider-model-id" class="font-technical" value={detail.id} readonly />
      </Field.Field>
      {#each stringFields as field (field.key)}
        {#if hasField(field.key)}
          <Field.Field orientation="vertical" class={field.multiline ? '@xl/model-editor:col-span-2' : ''}>
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
      {/each}
    </Field.Group>
    <div class="flex flex-wrap gap-2 empty:hidden">
      {#each stringFields.filter((field) => !hasField(field.key)) as field (field.key)}
        <Button type="button" variant="outline" size="sm" class="min-h-10" onclick={() => addStringField(field.key)}>
          <PlusIcon data-icon="inline-start" />{field.label()}
        </Button>
      {/each}
    </div>
  </section>

  <section class="flex min-w-0 flex-col gap-4 border-t pt-4">
    <div class="flex items-center justify-between gap-3">
      <h4 class="text-sm font-semibold">{m.provider_model_editor_context_modalities()}</h4>
    </div>
    {#if hasField('modalities') && metadata.modalities}
      <div class="rounded-lg border p-3">
        <p class="mb-3 text-sm font-medium">{m.provider_model_editor_supported_content_types()}</p>
        <Field.Group class="grid gap-4 @xl/model-editor:grid-cols-2">
          {#each modalityTargets as target (target)}
            <Field.Field orientation="vertical">
              <Field.Label for={`provider-model-${target}-modalities`}>
                {target === 'input'
                  ? m.provider_model_editor_accepted_input_types()
                  : m.provider_model_editor_generated_output_types()}
              </Field.Label>
              <Select.Root
                type="multiple"
                bind:value={
                  () => metadata.modalities?.[target] ?? [], (values: string[]) => setModalityValues(target, values)
                }>
                <Select.Trigger
                  id={`provider-model-${target}-modalities`}
                  class="w-full min-w-0"
                  data-modality-select={target}>
                  {#if metadata.modalities[target].length > 0}
                    <ModalityIcons
                      values={metadata.modalities[target]}
                      tooltip={false}
                      class="flex-nowrap overflow-hidden" />
                  {:else}
                    <span class="truncate">{m.provider_model_editor_select_content_types()}</span>
                  {/if}
                </Select.Trigger>
                <Select.Content>
                  <Select.Group>
                    {#each modalityOptions(target) as value (value)}
                      {@const modality = specificationModality(value)}
                      <Select.Item {value} label={modality.label()}
                        ><modality.icon
                          class="size-3.5 text-muted-foreground"
                          aria-hidden="true" />{modality.label()}</Select.Item>
                    {/each}
                  </Select.Group>
                </Select.Content>
              </Select.Root>
            </Field.Field>
          {/each}
        </Field.Group>
      </div>
    {/if}
    <Field.Field orientation="vertical">
      <Field.Label for="provider-model-limit-context">{m.provider_model_editor_context()}</Field.Label>
      <Input
        id="provider-model-limit-context"
        type="number"
        min="0"
        step="1"
        value={metadata.limit?.context ?? ''}
        placeholder={m.model_specification_not_registered()}
        oninput={(event: Event) => setLimit(inputValue(event))} />
    </Field.Field>
    <div class="flex flex-wrap gap-2 empty:hidden">
      {#if !hasField('modalities')}
        <Button
          type="button"
          variant="outline"
          size="sm"
          class="min-h-10"
          onclick={() => (metadata.modalities = { input: [], output: [] })}
          ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_modalities()}</Button>
      {/if}
    </div>
  </section>

  <section class="flex min-w-0 flex-col gap-4 border-t pt-4">
    <h4 class="text-sm font-semibold">{m.provider_model_editor_reasoning_efforts()}</h4>
    <Field.Field orientation="vertical">
      <Field.Label for="provider-model-reasoning-efforts" class="sr-only"
        >{m.provider_model_editor_reasoning_efforts()}</Field.Label>
      <Select.Root type="multiple" bind:value={() => metadata.reasoning_efforts ?? [], setEffortValues}>
        <Select.Trigger id="provider-model-reasoning-efforts" class="w-full min-w-0" data-effort-values-select>
          <span class="truncate"
            >{metadata.reasoning_efforts?.length
              ? metadata.reasoning_efforts.join(', ')
              : m.provider_model_editor_select_efforts()}</span>
        </Select.Trigger>
        <Select.Content
          ><Select.Group>
            {#each effortOptions() as value (value)}<Select.Item {value}>{value}</Select.Item>{/each}
          </Select.Group></Select.Content>
      </Select.Root>
      <Field.Description>{m.provider_model_editor_effort_help()}</Field.Description>
    </Field.Field>
    <Field.Field orientation="vertical">
      <Field.Label for="provider-model-custom-effort">{m.provider_model_editor_custom_effort()}</Field.Label>
      <div class="flex flex-wrap gap-2">
        <Input id="provider-model-custom-effort" class="min-w-0 flex-1 font-technical" bind:value={customEffort} />
        <Button
          type="button"
          variant="outline"
          onclick={addCustomEffort}
          disabled={!customEffort.trim() || ['default', 'null'].includes(customEffort.trim().toLowerCase())}>
          <PlusIcon data-icon="inline-start" />{m.provider_model_editor_add_effort()}
        </Button>
      </div>
    </Field.Field>
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
      <div>
        <p class="mb-3 text-sm font-medium">{m.provider_model_editor_base_pricing()}</p>
        <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
          {#each priceFields as field (field.key)}
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
      {#each cost.tiers as tier, index (tier)}
        <div class="border-t pt-4">
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
            {#each priceFields as field (field.key)}
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
    <Collapsible.Root class="rounded-lg border p-3" bind:open={extensionsOpen}>
      <Collapsible.Trigger type="button" class="w-full text-left"
        >{m.provider_model_editor_extension_fields_read_only()} · {extensionEntries.length}</Collapsible.Trigger>
      <Collapsible.Content>
        <pre
          class="mt-3 max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-md bg-muted p-3 font-technical text-xs">{JSON.stringify(
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
</div>

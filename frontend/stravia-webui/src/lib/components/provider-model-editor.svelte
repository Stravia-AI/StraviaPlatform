<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import PlusIcon from '@lucide/svelte/icons/plus'
import { tick, untrack, type Snippet } from 'svelte'

import FormSection from '#lib/components/form-section.svelte'
import ModalitySelect from '#lib/components/modality-select.svelte'
import PriceEditor from '#lib/components/price-editor.svelte'
import ReasoningEffortSelect from '#lib/components/reasoning-effort-select.svelte'
import { localeState } from '#lib/localization.svelte.js'
import { providerModelSelectionPolicyLabel } from '#lib/provider-model-labels.js'
import type { ProviderModelDetail, ProviderModelMetadata, ProviderModelSelectionPolicy } from '#lib/types.js'
import {
  buildProviderModelMetadataJson,
  normalizeProviderModelEfforts,
  providerModelFormFingerprint,
  type PriceForm,
  type ProviderModelSubmission,
} from './provider-model-form.js'
import { buildProviderModelChanges, emptyPriceForm, priceFormFromCost, validatePriceForm } from './price-form.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
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
  customCurrency?: { symbol: string } | null
  onSave: (submission: ProviderModelSubmission) => void
  onSelectionChange: (policy: ProviderModelSelectionPolicy) => void
  onRestore?: (fields: string[]) => void
  onDirtyChange?: (dirty: boolean) => void
  /** Localized failure from the last save attempt; cleared when the next write starts. */
  saveError?: string
  /** 上游模型的 RPM 限额；只有已存在的模型才有，由使用方提供并随页面一起保存。 */
  rpm?: Snippet
}

type StringField = 'name' | 'description'
const modalityTargets = ['input', 'output'] as const

// 宽布局下的“添加可选字段”按钮贴设置列右缘，与控件右边缘对齐。
const alignedActions = 'flex flex-wrap gap-2 empty:hidden'
// 设置列够宽时短字段两列并排；窄时退回单列。
const pairGrid =
  'grid gap-x-6 gap-y-4 grid-cols-[repeat(auto-fill,minmax(max(min(100%,20rem),calc((100%-1.5rem)/2)),1fr))]'
const stringFields: Array<{ key: StringField; label: () => string; multiline?: boolean }> = [
  { key: 'name', label: m.provider_model_field_name },
  { key: 'description', label: m.provider_model_field_description, multiline: true },
]
let {
  detail,
  draft = false,
  disabled = false,
  customCurrency = null,
  onSave,
  onSelectionChange,
  onRestore,
  onDirtyChange,
  saveError = '',
  rpm,
}: Props = $props()
let metadata = $state<ProviderModelMetadata>({})
let cost = $state<PriceForm>(emptyPriceForm())
let touched = $state<string[]>([])
let structuralErrors = $state<string[]>([])
let extensionsOpen = $state(false)
let errorAlert = $state<HTMLDivElement | null>(null)
let initialFingerprint = $state('')
let editorRoot = $state<HTMLFieldSetElement>()

const extensionEntries = $derived(Object.entries(detail.extensions ?? {}))
const confirmedMetadata = $derived(detail.metadata)
const pinnedFields = $derived(new Set(detail.pinned_fields ?? []))
const restorableFields = $derived(new Set(detail.restorable_fields ?? []))
const hasUnrestorablePinnedFields = $derived((detail.pinned_fields ?? []).some((field) => !restorableFields.has(field)))
const currentFingerprint = $derived(fingerprint())
const dirty = $derived(touched.length > 0 || (Boolean(initialFingerprint) && currentFingerprint !== initialFingerprint))

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
  cost = priceFormFromCost(snapshot.cost)
  touched = []
  structuralErrors = []
  initialFingerprint = untrack(fingerprint)
})
$effect(() => {
  if (saveError || structuralErrors.length > 0) {
    void tick().then(() => errorAlert?.focus())
  }
})

function fingerprint(): string {
  return providerModelFormFingerprint($state.snapshot(metadata), $state.snapshot(cost))
}

function touch(key: string): void {
  if (!touched.includes(key)) touched = [...touched, key]
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
  cost = emptyPriceForm()
  void focusControl('provider-model-cost-input')
}

function removeCost(): void {
  // 移除定价是显式清空：所有定价键都提交 null，避免同步重新带回旧值。
  for (const field of ['input', 'output', 'cache_read', 'cache_write'] as const) touch(`cost.${field}`)
  touch('cost.tiers')
  touch('cost.peak')
  delete metadata.cost
  cost = emptyPriceForm()
}

function setStringField(key: StringField, value: string): void {
  metadata[key] = value
  touch(key)
}

function setModalityValues(target: 'input' | 'output', values: string[]): void {
  metadata.modalities ??= { input: [], output: [] }
  metadata.modalities[target] = values
  touch(`modalities.${target}`)
}

function setLimit(value: string): void {
  metadata.limit = { context: value === '' ? null : Number(value) }
  touch('limit.context')
}

function setEfforts(values: string[]): void {
  metadata.reasoning_efforts = values
  touch('reasoning_efforts')
}

export function submit(): void {
  if (disabled) return
  if (draft) {
    const result = buildProviderModelMetadataJson(detail.id, $state.snapshot(metadata), $state.snapshot(cost))
    structuralErrors = result.errors
    if (result.json) onSave({ kind: 'create', metadataJson: result.json })
    return
  }
  const costErrors = validatePriceForm($state.snapshot(cost))
  const result = buildProviderModelChanges(
    $state.snapshot(confirmedMetadata),
    $state.snapshot(metadata),
    priceFormFromCost(confirmedMetadata.cost),
    $state.snapshot(cost),
    new Set(touched),
  )
  structuralErrors = [...costErrors, ...result.errors]
  if (structuralErrors.length > 0 || Object.keys(result.changes).length === 0) return
  onSave({ kind: 'update', changes: result.changes })
}
</script>

{#snippet pinnedControl(field: string, label: string)}
  {#if pinnedFields.has(field)}
    <span class="flex shrink-0 items-center gap-1">
      <Badge variant="outline" class="px-1.5 py-0 text-[0.6875rem]">{m.provider_model_editor_pinned()}</Badge>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        class="h-6 px-1.5 text-xs"
        disabled={disabled || !restorableFields.has(field)}
        title={restorableFields.has(field) ? undefined : m.provider_model_editor_restore_unavailable()}
        aria-label={`${m.provider_model_editor_restore_sync()} ${label}`}
        onclick={() => onRestore?.([field])}>{m.provider_model_editor_restore_sync()}</Button>
    </span>
  {/if}
{/snippet}

{#snippet stringControl(field: (typeof stringFields)[number])}
  {#if hasField(field.key)}
    <Field.Field class={field.multiline ? 'col-span-full' : undefined}>
      <div class="flex min-h-6 flex-wrap items-center justify-between gap-1">
        <Field.Label for={`provider-model-${field.key}`}>{field.label()}</Field.Label>
        {@render pinnedControl(field.key, field.label())}
      </div>
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
  {#if hasUnrestorablePinnedFields}
    <p class="text-sm text-muted-foreground">{m.provider_model_editor_restore_unavailable_hint()}</p>
  {/if}

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
        <div class="flex min-h-6 flex-wrap items-center justify-between gap-1">
          <Field.Label for="provider-model-limit-context">{m.provider_model_editor_context()}</Field.Label>
          {@render pinnedControl('limit.context', m.provider_model_editor_context())}
        </div>
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
        <div class="flex min-h-6 flex-wrap items-center justify-between gap-1">
          <Field.Label for="provider-model-reasoning-efforts" hint={m.provider_model_editor_effort_help()}
            >{m.provider_model_editor_reasoning_efforts()}</Field.Label>
          {@render pinnedControl('reasoning_efforts', m.provider_model_editor_reasoning_efforts())}
        </div>
        <ReasoningEffortSelect
          id="provider-model-reasoning-efforts"
          {disabled}
          registered={detail.metadata.reasoning_efforts}
          bind:value={() => metadata.reasoning_efforts ?? [], setEfforts} />
      </Field.Field>
      {#if hasField('modalities') && metadata.modalities}
        {#each modalityTargets as target (target)}
          {@const label =
            target === 'input'
              ? m.provider_model_editor_accepted_input_types()
              : m.provider_model_editor_generated_output_types()}
          <Field.Field>
            <div class="flex min-h-6 flex-wrap items-center justify-between gap-1">
              <Field.Label for={`provider-model-${target}-modalities`}>{label}</Field.Label>
              {@render pinnedControl(`modalities.${target}`, label)}
            </div>
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
        <PriceEditor
          bind:value={cost}
          {customCurrency}
          {disabled}
          idPrefix="provider-model-cost"
          {pinnedFields}
          {restorableFields}
          onRestore={(field: string) => onRestore?.([field])}
          onFieldTouch={(field) => touch(`cost.${field}`)}
          onRemove={removeCost} />
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

  {#if saveError || structuralErrors.length > 0}
    <Alert.Root bind:ref={errorAlert} tabindex={-1} variant="destructive" role="alert">
      <Alert.Title>{m.provider_model_editor_cannot_save()}</Alert.Title>
      <Alert.Description
        ><ul class="list-disc pl-5">
          {#if saveError}<li>{saveError}</li>{/if}
          {#each structuralErrors as error (error)}<li>{error}</li>{/each}
        </ul></Alert.Description>
    </Alert.Root>
  {/if}
</fieldset>

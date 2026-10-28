<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import PlusIcon from '@lucide/svelte/icons/plus'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { tick } from 'svelte'

import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Checkbox } from '#lib/components/ui/checkbox/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import {
  basePriceFields,
  isoWeekdays,
  peakFormPrefill,
  priceFieldLabel,
  validatePriceForm,
  type BasePriceFieldName,
  type PeakForm,
  type PriceCurrency,
  type PriceForm,
  type PriceTouchField,
} from './price-form.js'

interface Props {
  value: PriceForm
  /** `null` hides every currency control; USD is the only choice. */
  customCurrency?: { symbol: string } | null
  disabled?: boolean
  idPrefix?: string
  pinnedFields?: ReadonlySet<string>
  restorableFields?: ReadonlySet<string>
  onRestore?: (field: string) => void
  onFieldTouch?: (field: PriceTouchField) => void
  /** Shows the Provider Model pricing removal action when provided. */
  onRemove?: () => void
}

let {
  value = $bindable(),
  customCurrency = null,
  disabled = false,
  idPrefix = 'price',
  pinnedFields,
  restorableFields,
  onRestore,
  onFieldTouch,
  onRemove,
}: Props = $props()

const priceGrid = 'grid gap-3 grid-cols-[repeat(auto-fill,minmax(max(min(100%,10rem),calc((100%-2.25rem)/4)),1fr))]'
const errors = $derived(validatePriceForm(value))

function touch(field: PriceTouchField): void {
  onFieldTouch?.(field)
}

function setCurrency(field: BasePriceFieldName, currency: PriceCurrency): void {
  value.base[field].currency = currency
  touch(field)
}

function setTiersCurrency(currency: PriceCurrency): void {
  value.tiersCurrency = currency
  touch('tiers')
}

function addTier(): void {
  value.tiers.push({ threshold: '', prices: { input: '', output: '', cache_read: '', cache_write: '' } })
  touch('tiers')
  void tick().then(() => document.getElementById(`${idPrefix}-tier-${value.tiers.length - 1}`)?.focus())
}

function removeTier(index: number): void {
  value.tiers.splice(index, 1)
  touch('tiers')
}

function addPeak(): void {
  value.peak = peakFormPrefill()
  touch('peak')
  void tick().then(() => document.getElementById(`${idPrefix}-peak-timezone`)?.focus())
}

function removePeak(): void {
  value.peak = null
  touch('peak')
}

function toggleWeekday(peak: PeakForm, day: number, checked: boolean): void {
  peak.weekdays = checked
    ? [...new Set([...peak.weekdays, day])].sort()
    : peak.weekdays.filter((value) => value !== day)
  touch('peak')
}

function addRange(peak: PeakForm): void {
  peak.ranges.push({ start: '', end: '' })
  touch('peak')
}

function removeRange(peak: PeakForm, index: number): void {
  peak.ranges.splice(index, 1)
  touch('peak')
}

function weekdayLabel(day: number): string {
  switch (day) {
    case 1:
      return m.pricing_weekday_mon()
    case 2:
      return m.pricing_weekday_tue()
    case 3:
      return m.pricing_weekday_wed()
    case 4:
      return m.pricing_weekday_thu()
    case 5:
      return m.pricing_weekday_fri()
    case 6:
      return m.pricing_weekday_sat()
    default:
      return m.pricing_weekday_sun()
  }
}
</script>

{#snippet currencyToggle(current: PriceCurrency, label: string, onSelect: (currency: PriceCurrency) => void)}
  <div class="flex gap-1" role="group" aria-label={m.pricing_currency_for({ field: label })}>
    <Button
      type="button"
      size="sm"
      variant={current === 'usd' ? 'secondary' : 'ghost'}
      class="h-7 min-w-0 flex-1 px-2 text-xs"
      aria-pressed={current === 'usd'}
      {disabled}
      onclick={() => onSelect('usd')}>{m.pricing_currency_usd()}</Button>
    <Button
      type="button"
      size="sm"
      variant={current === 'custom' ? 'secondary' : 'ghost'}
      class="h-7 min-w-0 flex-1 truncate px-2 text-xs"
      aria-pressed={current === 'custom'}
      {disabled}
      onclick={() => onSelect('custom')}>{customCurrency?.symbol}</Button>
  </div>
{/snippet}

{#snippet pinControl(field: string, label: string)}
  {#if pinnedFields?.has(field)}
    <span class="flex shrink-0 items-center gap-1">
      <Badge variant="outline" class="px-1.5 py-0 text-[0.6875rem]">{m.provider_model_editor_pinned()}</Badge>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        class="h-6 px-1.5 text-xs"
        disabled={disabled || !restorableFields?.has(field)}
        title={restorableFields?.has(field) ? undefined : m.provider_model_editor_restore_unavailable()}
        aria-label={`${m.provider_model_editor_restore_sync()} ${label}`}
        onclick={() => onRestore?.(field)}>{m.provider_model_editor_restore_sync()}</Button>
    </span>
  {/if}
{/snippet}

<div class="flex flex-col gap-3" data-price-editor>
  <div class="flex flex-wrap items-center justify-between gap-2">
    <p class="flex items-center gap-2 text-sm font-medium">
      {m.provider_model_editor_base_pricing()}
      <Field.Help hint={m.pricing_unit_help()} />
    </p>
    {#if onRemove}
      <Button type="button" variant="ghost" size="sm" {disabled} onclick={onRemove}
        ><Trash2Icon data-icon="inline-start" />{m.common_remove()}</Button>
    {/if}
  </div>

  <div class={priceGrid}>
    {#each basePriceFields as field (field)}
      {@const label = priceFieldLabel(field)}
      <Field.Field orientation="vertical">
        <div class="flex min-h-6 flex-wrap items-center justify-between gap-1">
          <Field.Label for={`${idPrefix}-${field}`}>{label}</Field.Label>
          {@render pinControl(`cost.${field}`, label)}
        </div>
        <Input
          id={`${idPrefix}-${field}`}
          class="font-technical"
          inputmode="decimal"
          bind:value={value.base[field].amount}
          oninput={() => touch(field)}
          placeholder="0.00"
          {disabled} />
        {#if customCurrency}
          {@render currencyToggle(value.base[field].currency, label, (currency) => setCurrency(field, currency))}
        {/if}
      </Field.Field>
    {/each}
  </div>

  <div class="flex flex-col gap-3 border-t border-dashed pt-4">
    <div class="flex flex-wrap items-center justify-between gap-2">
      <p class="flex items-center gap-2 text-sm font-medium">
        {m.pricing_tiers_title()}
        <Field.Help hint={m.pricing_tiers_help()} />
        {@render pinControl('cost.tiers', m.pricing_tiers_title())}
      </p>
      {#if customCurrency}
        <div class="flex items-center gap-2">
          <span class="text-xs text-muted-foreground">{m.pricing_tiers_currency()}</span>
          {@render currencyToggle(value.tiersCurrency, m.pricing_tiers_title(), setTiersCurrency)}
        </div>
      {/if}
    </div>

    {#each value.tiers as tier, index (index)}
      <div class="flex flex-col gap-3 border-t border-dashed pt-4 first:border-t-0 first:pt-0">
        <div class="flex items-end justify-between gap-3">
          <Field.Field orientation="vertical" class="max-w-64">
            <Field.Label for={`${idPrefix}-tier-${index}`}
              >{m.provider_model_editor_tier_value_context_threshold({ index: index + 1 })}</Field.Label>
            <Input
              id={`${idPrefix}-tier-${index}`}
              inputmode="numeric"
              class="font-technical"
              bind:value={tier.threshold}
              oninput={() => touch('tiers')}
              {disabled} />
          </Field.Field>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            class="shrink-0"
            aria-label={m.provider_model_editor_remove_tier()}
            {disabled}
            onclick={() => removeTier(index)}><Trash2Icon /></Button>
        </div>
        <div class={priceGrid}>
          {#each basePriceFields as field (field)}
            <Field.Field orientation="vertical">
              <Field.Label for={`${idPrefix}-tier-${index}-${field}`}>{priceFieldLabel(field)}</Field.Label>
              <Input
                id={`${idPrefix}-tier-${index}-${field}`}
                class="font-technical"
                inputmode="decimal"
                bind:value={tier.prices[field]}
                oninput={() => touch('tiers')}
                placeholder="0.00"
                {disabled} />
            </Field.Field>
          {/each}
        </div>
      </div>
    {/each}

    <Button type="button" variant="outline" size="sm" class="self-start" {disabled} onclick={addTier}
      ><PlusIcon data-icon="inline-start" />{m.provider_model_editor_pricing_tier()}</Button>
  </div>

  <div class="flex flex-col gap-3 border-t border-dashed pt-4">
    <div class="flex flex-wrap items-center justify-between gap-2">
      <p class="flex items-center gap-2 text-sm font-medium">
        {m.pricing_peak_title()}
        <Field.Help hint={m.pricing_peak_help()} />
      </p>
      {#if value.peak}
        <Button type="button" variant="ghost" size="sm" {disabled} onclick={removePeak}
          ><Trash2Icon data-icon="inline-start" />{m.pricing_peak_remove()}</Button>
      {/if}
    </div>

    {#if value.peak}
      {@const peak = value.peak}
      <div class="grid gap-4 grid-cols-[repeat(auto-fit,minmax(min(100%,14rem),1fr))]">
        <Field.Field orientation="vertical">
          <Field.Label for={`${idPrefix}-peak-timezone`}>{m.pricing_peak_timezone()}</Field.Label>
          <Input
            id={`${idPrefix}-peak-timezone`}
            class="font-technical"
            bind:value={peak.timezone}
            oninput={() => touch('peak')}
            placeholder="Asia/Shanghai"
            {disabled} />
        </Field.Field>
        <Field.Field orientation="vertical">
          <Field.Label for={`${idPrefix}-peak-multiplier`}>{m.pricing_peak_multiplier()}</Field.Label>
          <Input
            id={`${idPrefix}-peak-multiplier`}
            class="font-technical"
            inputmode="decimal"
            bind:value={peak.multiplier}
            oninput={() => touch('peak')}
            {disabled} />
        </Field.Field>
      </div>
      <Field.Field orientation="vertical">
        <Field.Label>{m.pricing_peak_weekdays()}</Field.Label>
        <div class="flex flex-wrap gap-x-4 gap-y-2" role="group" aria-label={m.pricing_peak_weekdays()}>
          {#each isoWeekdays as day (day)}
            <label class="flex items-center gap-1.5 text-sm">
              <Checkbox
                checked={peak.weekdays.includes(day)}
                {disabled}
                onCheckedChange={(checked) => toggleWeekday(peak, day, Boolean(checked))} />
              {weekdayLabel(day)}
            </label>
          {/each}
        </div>
      </Field.Field>
      <div class="flex flex-col gap-3">
        <div class="flex items-center justify-between gap-2">
          <span class="text-sm font-medium">{m.pricing_peak_ranges()}</span>
          <Button type="button" variant="outline" size="sm" {disabled} onclick={() => addRange(peak)}
            ><PlusIcon data-icon="inline-start" />{m.pricing_peak_add_range()}</Button>
        </div>
        {#each peak.ranges as range, index (index)}
          <div class="flex flex-wrap items-end gap-3">
            <Field.Field orientation="vertical" class="min-w-32 flex-1">
              <Field.Label for={`${idPrefix}-peak-range-${index}-start`}>{m.pricing_peak_range_start()}</Field.Label>
              <Input
                id={`${idPrefix}-peak-range-${index}-start`}
                class="font-technical"
                inputmode="numeric"
                placeholder="09:00"
                bind:value={range.start}
                oninput={() => touch('peak')}
                {disabled} />
            </Field.Field>
            <Field.Field orientation="vertical" class="min-w-32 flex-1">
              <Field.Label for={`${idPrefix}-peak-range-${index}-end`}>{m.pricing_peak_range_end()}</Field.Label>
              <Input
                id={`${idPrefix}-peak-range-${index}-end`}
                class="font-technical"
                inputmode="numeric"
                placeholder="12:00"
                bind:value={range.end}
                oninput={() => touch('peak')}
                {disabled} />
            </Field.Field>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              class="mb-0.5"
              aria-label={m.pricing_peak_remove_range({ range: index + 1 })}
              {disabled}
              onclick={() => removeRange(peak, index)}><Trash2Icon /></Button>
          </div>
        {/each}
      </div>
    {:else}
      <Button type="button" variant="outline" size="sm" class="self-start" {disabled} onclick={addPeak}
        ><PlusIcon data-icon="inline-start" />{m.pricing_peak_add()}</Button>
    {/if}
  </div>

  {#if errors.length > 0}
    <ul class="list-disc space-y-1 pl-5 text-sm text-destructive" role="alert">
      {#each errors as error (error)}
        <li>{error}</li>
      {/each}
    </ul>
  {/if}
</div>

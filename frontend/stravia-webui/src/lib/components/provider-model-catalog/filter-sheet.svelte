<script lang="ts">
import * as m from '$lib/paraglide/messages.js'

import ModelSpecificationFilter from '$lib/components/model-specification-filter.svelte'
import type { SpecificationFilter } from '$lib/model-specification-filter'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { allCatalogFilterValue, catalogFilterOptions } from './filter-options'

interface Props {
  open?: boolean
  availability: string
  source: string
  reference: string
  specification: SpecificationFilter
  onSpecificationChange: (value: SpecificationFilter) => void
  onFilterChange: (columnId: string, value: string) => void
  onClear: () => void
}

let {
  open = $bindable(false),
  availability,
  source,
  reference,
  specification,
  onSpecificationChange,
  onFilterChange,
  onClear,
}: Props = $props()
const options = $derived(catalogFilterOptions())
</script>

{#snippet availabilitySelect(id: string)}
  <Select.Root
    type="single"
    bind:value={() => availability, (value) => onFilterChange('availability', value ?? allCatalogFilterValue)}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_model_availability()}>
      {options.availability.options.find((option) => option.value === availability)?.label ??
        options.availability.allLabel}
    </Select.Trigger>
    <Select.Content>
      <Select.Group>
        <Select.Item value={allCatalogFilterValue}>{options.availability.allLabel}</Select.Item>
        {#each options.availability.options as option (option.value)}
          <Select.Item value={option.value}>{option.label}</Select.Item>
        {/each}
      </Select.Group>
    </Select.Content>
  </Select.Root>
{/snippet}

{#snippet sourceSelect(id: string)}
  <Select.Root
    type="single"
    bind:value={() => source, (value) => onFilterChange('source_kind', value ?? allCatalogFilterValue)}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_how_models_were_added()}>
      {options.source.options.find((option) => option.value === source)?.label ?? options.source.allLabel}
    </Select.Trigger>
    <Select.Content>
      <Select.Group>
        <Select.Item value={allCatalogFilterValue}>{options.source.allLabel}</Select.Item>
        {#each options.source.options as option (option.value)}
          <Select.Item value={option.value}>{option.label}</Select.Item>
        {/each}
      </Select.Group>
    </Select.Content>
  </Select.Root>
{/snippet}

{#snippet referenceSelect(id: string)}
  <Select.Root
    type="single"
    bind:value={() => reference, (value) => onFilterChange('usage', value ?? allCatalogFilterValue)}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_model_usage()}>
      {options.reference.options.find((option) => option.value === reference)?.label ?? options.reference.allLabel}
    </Select.Trigger>
    <Select.Content>
      <Select.Group>
        <Select.Item value={allCatalogFilterValue}>{options.reference.allLabel}</Select.Item>
        {#each options.reference.options as option (option.value)}
          <Select.Item value={option.value}>{option.label}</Select.Item>
        {/each}
      </Select.Group>
    </Select.Content>
  </Select.Root>
{/snippet}

<Sheet.Root bind:open>
  <Sheet.Content
    side="right"
    class="w-full! max-w-none! gap-0 p-0 sm:max-w-sm!"
    closeLabel={m.provider_model_catalog_close_model_filters()}>
    <Sheet.Header class="border-b">
      <Sheet.Title>{m.provider_model_catalog_filter_models()}</Sheet.Title>
      <Sheet.Description>{m.provider_model_catalog_filter_models_description()}</Sheet.Description>
    </Sheet.Header>
    <div class="route-overlay-body">
      <Field.FieldGroup>
        <Field.Field>
          <Field.FieldLabel for="provider-model-availability-mobile"
            >{m.provider_model_catalog_model_availability()}</Field.FieldLabel>
          {@render availabilitySelect('provider-model-availability-mobile')}
        </Field.Field>
        <Field.Field>
          <Field.FieldLabel for="provider-model-source-mobile"
            >{m.provider_model_catalog_how_models_were_added()}</Field.FieldLabel>
          {@render sourceSelect('provider-model-source-mobile')}
        </Field.Field>
        <Field.Field>
          <Field.FieldLabel for="provider-model-reference-mobile"
            >{m.provider_model_catalog_model_usage()}</Field.FieldLabel>
          {@render referenceSelect('provider-model-reference-mobile')}
        </Field.Field>
        <Field.FieldSet>
          <Field.FieldLegend>{m.model_specification_title()}</Field.FieldLegend>
          <ModelSpecificationFilter value={specification} onChange={onSpecificationChange} />
        </Field.FieldSet>
      </Field.FieldGroup>
    </div>
    <Sheet.Footer class="route-overlay-footer">
      <Button variant="outline" onclick={onClear}>{m.provider_model_catalog_clear_filters()}</Button>
      <Sheet.Close class="h-10 rounded-md bg-primary px-3 text-primary-foreground">
        {m.provider_model_catalog_show_models()}
      </Sheet.Close>
    </Sheet.Footer>
  </Sheet.Content>
</Sheet.Root>

<script lang="ts">
import * as m from '$lib/paraglide/messages.js'

import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'

interface Props {
  open?: boolean
  availability: string
  source: string
  reference: string
  onFilterChange: (columnId: string, value: string) => void
  onClear: () => void
}

let {
  open = $bindable(false),
  availability,
  source,
  reference,
  onFilterChange,
  onClear,
}: Props = $props()
</script>

{#snippet availabilitySelect(id: string)}
  <Select.Root type="single" bind:value={() => availability, (value) => onFilterChange('availability', value ?? 'all')}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_model_availability()}>
      {availability === 'all' ? m.common_all_models() : availability === 'available' ? m.common_used() : m.common_unavailable()}
    </Select.Trigger>
    <Select.Content>
      <Select.Item value="all">{m.common_all_models()}</Select.Item>
      <Select.Item value="available">{m.common_used()}</Select.Item>
      <Select.Item value="unavailable">{m.common_unavailable()}</Select.Item>
    </Select.Content>
  </Select.Root>
{/snippet}

{#snippet sourceSelect(id: string)}
  <Select.Root type="single" bind:value={() => source, (value) => onFilterChange('source_kind', value ?? 'all')}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_how_models_were_added()}>
      {source === 'all' ? m.provider_model_catalog_all_sources() : source === 'manual' ? m.common_added_manually() : m.common_synced()}
    </Select.Trigger>
    <Select.Content>
      <Select.Item value="all">{m.provider_model_catalog_all_sources()}</Select.Item>
      <Select.Item value="discovered">{m.common_synced()}</Select.Item>
      <Select.Item value="manual">{m.common_added_manually()}</Select.Item>
    </Select.Content>
  </Select.Root>
{/snippet}

{#snippet referenceSelect(id: string)}
  <Select.Root type="single" bind:value={() => reference, (value) => onFilterChange('usage', value ?? 'all')}>
    <Select.Trigger {id} class="h-10 w-full font-normal" aria-label={m.provider_model_catalog_model_usage()}>
      {reference === 'all' ? m.provider_model_catalog_all_usage() : reference === 'referenced' ? m.provider_model_catalog_use() : m.provider_model_catalog_not_use()}
    </Select.Trigger>
    <Select.Content>
      <Select.Item value="all">{m.provider_model_catalog_all_usage()}</Select.Item>
      <Select.Item value="referenced">{m.provider_model_catalog_use()}</Select.Item>
      <Select.Item value="unreferenced">{m.provider_model_catalog_not_use()}</Select.Item>
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
          <Field.FieldLabel for="provider-model-availability-mobile">{m.provider_model_catalog_model_availability()}</Field.FieldLabel>
          {@render availabilitySelect('provider-model-availability-mobile')}
        </Field.Field>
        <Field.Field>
          <Field.FieldLabel for="provider-model-source-mobile">{m.provider_model_catalog_how_models_were_added()}</Field.FieldLabel>
          {@render sourceSelect('provider-model-source-mobile')}
        </Field.Field>
        <Field.Field>
          <Field.FieldLabel for="provider-model-reference-mobile">{m.provider_model_catalog_model_usage()}</Field.FieldLabel>
          {@render referenceSelect('provider-model-reference-mobile')}
        </Field.Field>
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

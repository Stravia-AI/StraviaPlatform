<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import type { CanonicalModelSummary } from '#lib/types.js'
import { idNameOption } from '#lib/search-combobox-option.js'

import SearchCombobox from '#lib/components/search-combobox.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

interface Props {
  open?: boolean
  templateId?: string
  models: CanonicalModelSummary[]
  modelsPending: boolean
  preparing: boolean
  onOpenChange: (open: boolean) => void
  onSelect: (templateId: string) => void
  onClear: () => void
  onContinue: () => void
}

let {
  open = false,
  templateId = $bindable(''),
  models,
  modelsPending,
  preparing,
  onOpenChange,
  onSelect,
  onClear,
  onContinue,
}: Props = $props()
</script>

<Dialog.Root bind:open={() => open, onOpenChange}>
  <Dialog.Layout>
    {#snippet header()}
      <Dialog.Title>{m.provider_model_catalog_add_model_manually_label()}</Dialog.Title>
      <Dialog.Description>{m.provider_model_catalog_add_model_manually_description()}</Dialog.Description>
    {/snippet}
    <Field.Field orientation="vertical">
      <Field.Label for="manual-provider-model-search">{m.provider_model_catalog_model_template()}</Field.Label>
      <SearchCombobox
        id="manual-provider-model-search"
        value={templateId}
        options={models.map(idNameOption)}
        placeholder={m.provider_model_catalog_search_model()}
        emptyText={m.common_no_matching_models()}
        ariaLabel={m.provider_model_catalog_search_model()}
        clearLabel={m.provider_model_catalog_clear_selected_model()}
        disabled={modelsPending || preparing}
        onValueChange={(value: string) => (value ? onSelect(value) : onClear())} />
    </Field.Field>
    {#snippet footer()}
      <Button variant="outline" onclick={() => onOpenChange(false)}>{m.common_cancel()}</Button>
      <Button onclick={onContinue} disabled={preparing || !templateId.trim()}>
        {#if preparing}<Spinner data-icon="inline-start" />{/if}{m.provider_model_catalog_continue()}
      </Button>
    {/snippet}
  </Dialog.Layout>
</Dialog.Root>

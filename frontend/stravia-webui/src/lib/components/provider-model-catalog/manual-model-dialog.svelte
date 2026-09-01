<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import type { CanonicalModelSummary } from '$lib/types'

import ModelCombobox from '$lib/components/model-combobox.svelte'
import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import * as Field from '$lib/components/ui/field'
import { Spinner } from '$lib/components/ui/spinner'

interface Props {
  open?: boolean
  templateId?: string
  models: CanonicalModelSummary[]
  modelsPending: boolean
  preparing: boolean
  onSelect: (templateId: string) => void
  onClear: () => void
  onContinue: () => void
}

let {
  open = $bindable(false),
  templateId = $bindable(''),
  models,
  modelsPending,
  preparing,
  onSelect,
  onClear,
  onContinue,
}: Props = $props()
</script>

<Dialog.Root bind:open>
  <Dialog.Content>
    <Dialog.Header>
      <Dialog.Title>{m.provider_model_catalog_add_model_manually_label()}</Dialog.Title>
    </Dialog.Header>
    <Field.Field>
      <Field.Label>{m.provider_model_catalog_search_model()}</Field.Label>
      <ModelCombobox
        id="manual-provider-model-search"
        value={templateId}
        {models}
        placeholder={m.provider_model_catalog_search_model()}
        searchPlaceholder={m.provider_model_catalog_search_model()}
        emptyText={m.provider_model_catalog_no_models_found()}
        ariaLabel={m.provider_model_catalog_search_model()}
        searchAriaLabel={m.provider_model_catalog_search_model()}
        clearAriaLabel={m.provider_model_catalog_clear_selected_model()}
        disabled={modelsPending}
        {onSelect}
        {onClear} />
    </Field.Field>
    <Dialog.Footer>
      <Button variant="outline" onclick={() => (open = false)}>{m.common_cancel()}</Button>
      <Button onclick={onContinue} disabled={preparing || !templateId.trim()}>
        {#if preparing}<Spinner data-icon="inline-start" />{/if}{m.provider_model_catalog_continue()}
      </Button>
    </Dialog.Footer>
  </Dialog.Content>
</Dialog.Root>

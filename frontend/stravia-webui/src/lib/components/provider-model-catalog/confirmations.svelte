<script lang="ts">
import { resolve } from '$app/paths'
import * as m from '$lib/paraglide/messages.js'
import { effectiveModelDisplayName, logicalModelSecondaryId } from '$lib/logical-model'
import type { ProviderModelDetail, Route } from '$lib/types'

import * as AlertDialog from '$lib/components/ui/alert-dialog'

interface Props {
  discardOpen?: boolean
  deleteOpen?: boolean
  detail?: ProviderModelDetail
  references: Array<{ route: Route; target: Route['targets'][number] }>
  routeReferencesReady: boolean
  saving: boolean
  onKeepEditing: () => void
  onDiscard: () => void
  onDelete: () => void
}

let {
  discardOpen = $bindable(false),
  deleteOpen = $bindable(false),
  detail,
  references,
  routeReferencesReady,
  saving,
  onKeepEditing,
  onDiscard,
  onDelete,
}: Props = $props()
</script>

<AlertDialog.Root bind:open={discardOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.provider_model_catalog_discard_unsaved_model_changes()}</AlertDialog.Title>
      <AlertDialog.Description>{m.provider_model_catalog_unsaved_changes_warning()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={onKeepEditing}>{m.provider_model_catalog_keep_editing()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" onclick={onDiscard}
        >{m.provider_model_catalog_discard_changes()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>
        {m.provider_model_catalog_delete_value({ id: detail?.metadata.name || detail?.id || m.common_model() })}
      </AlertDialog.Title>
      <AlertDialog.Description>
        {#if routeReferencesReady}
          {m.provider_model_catalog_remove_manual_references({ count: references.length })}
        {:else}
          {m.provider_model_catalog_usage_check_error()}
        {/if}
      </AlertDialog.Description>
    </AlertDialog.Header>
    {#if references.length > 0}
      <div class="flex flex-col gap-1 rounded-lg border p-3">
        {#each references as reference (reference.target.id)}
          <a
            class="flex min-h-10 items-center rounded-md px-2 font-medium hover:bg-muted"
            href={resolve('/models/[id]', { id: reference.route.model_id })}>
            {effectiveModelDisplayName(reference.route)}
            {#if logicalModelSecondaryId(reference.route)}
              · {reference.route.model_id}{/if}
            · {reference.target.model}
          </a>
        {/each}
      </div>
    {/if}
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" disabled={saving || !routeReferencesReady} onclick={onDelete}>
        {m.provider_model_catalog_remove_list()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

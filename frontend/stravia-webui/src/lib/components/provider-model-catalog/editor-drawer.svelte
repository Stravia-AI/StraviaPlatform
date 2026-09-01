<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import type { ProviderModelDetail, ProviderModelSelectionPolicy } from '$lib/types'

import ProviderModelEditor from '$lib/components/provider-model-editor.svelte'
import { Button } from '$lib/components/ui/button'
import * as Sheet from '$lib/components/ui/sheet'
import { Spinner } from '$lib/components/ui/spinner'

interface Props {
  open?: boolean
  detail?: ProviderModelDetail
  draft: boolean
  loading: boolean
  saving: boolean
  onOpenChange: (open: boolean) => void
  onClose: () => void
  onSave: (metadataJson: string) => void
  onSelectionChange: (policy: ProviderModelSelectionPolicy) => void
  onDirtyChange: (dirty: boolean) => void
}

let {
  open = $bindable(false),
  detail,
  draft,
  loading,
  saving,
  onOpenChange,
  onClose,
  onSave,
  onSelectionChange,
  onDirtyChange,
}: Props = $props()

let editor = $state<{ submit: () => void }>()
</script>

<Sheet.Root bind:open {onOpenChange}>
  <Sheet.Content
    side="right"
    class="provider-model-drawer w-full! max-w-none! gap-0 overflow-hidden p-0 sm:max-w-[960px]!"
    closeLabel={m.provider_model_catalog_close_model_editor()}>
    {#if detail}
      <Sheet.Header class="border-b pr-14">
        <Sheet.Title class="truncate">{detail.metadata.name || detail.id}</Sheet.Title>
        <Sheet.Description class="break-all font-technical">{detail.id}</Sheet.Description>
      </Sheet.Header>
      <div class="route-overlay-body" data-provider-model-scroll-owner>
        {#if loading}
          <div class="grid min-h-72 place-items-center"><Spinner /></div>
        {:else}
          <ProviderModelEditor
            bind:this={editor}
            {detail}
            {draft}
            {onSave}
            {onSelectionChange}
            {onDirtyChange} />
        {/if}
      </div>
      <Sheet.Footer class="route-overlay-footer justify-between sm:justify-between">
        <Button variant="outline" onclick={onClose}>{m.common_cancel()}</Button>
        <Button onclick={() => editor?.submit()} disabled={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}
          {m.common_add_model()}
        </Button>
      </Sheet.Footer>
    {/if}
  </Sheet.Content>
</Sheet.Root>

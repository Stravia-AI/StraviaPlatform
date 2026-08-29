<script lang="ts">
import ChevronsUpDownIcon from '@lucide/svelte/icons/chevrons-up-down'
import { tick } from 'svelte'

import { Button } from '$lib/components/ui/button'
import * as Command from '$lib/components/ui/command'
import * as Popover from '$lib/components/ui/popover'

interface Props {
  id: string
  value: string
  models: Array<{ id: string; name: string }>
  placeholder: string
  searchPlaceholder: string
  emptyText: string
  ariaLabel: string
  searchAriaLabel: string
  clearAriaLabel?: string
  disabled?: boolean
  onSelect: (modelId: string) => void
  onClear?: () => void
}

let {
  id,
  value,
  models,
  placeholder,
  searchPlaceholder,
  emptyText,
  ariaLabel,
  searchAriaLabel,
  clearAriaLabel,
  disabled = false,
  onSelect,
  onClear,
}: Props = $props()
let open = $state(false)
let triggerRef = $state<HTMLButtonElement>(null!)
const selectedModel = $derived(models.find((model) => model.id === value))

function selectModel(modelId: string): void {
  open = false
  onSelect(modelId)
  void tick().then(() => triggerRef.focus())
}

function clearSelection(): void {
  open = false
  onClear?.()
  void tick().then(() => triggerRef.focus())
}
</script>

<Popover.Root bind:open>
  <Popover.Trigger bind:ref={triggerRef}>
    {#snippet child({ props })}
      <Button
        {...props}
        {id}
        type="button"
        variant="outline"
        class="w-full min-w-0 justify-between font-normal"
        role="combobox"
        aria-label={ariaLabel}
        aria-expanded={open}
        {disabled}>
        <span class="truncate text-left font-technical">
          {selectedModel ? `${selectedModel.name} · ${selectedModel.id}` : value || placeholder}
        </span>
        <ChevronsUpDownIcon data-icon="inline-end" class="ml-auto opacity-50" />
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content align="start" class="w-(--bits-popover-anchor-width) p-0">
    <Command.Root {value} label={ariaLabel}>
      {#if value && onClear}
        <div class="border-b p-1">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            class="w-full justify-start"
            aria-label={clearAriaLabel ?? `${ariaLabel}: clear selection`}
            onclick={clearSelection}>
            {clearAriaLabel ?? 'Clear selection'}
          </Button>
        </div>
      {/if}
      <Command.Input placeholder={searchPlaceholder} aria-label={searchAriaLabel} />
      <Command.List>
        <Command.Empty>{emptyText}</Command.Empty>
        <Command.Group>
          {#each models as model (model.id)}
            <Command.Item value={model.id} keywords={[model.name]} onSelect={() => selectModel(model.id)}>
              <span class="truncate">{model.name}</span>
              <span class="truncate font-technical text-muted-foreground">{model.id}</span>
            </Command.Item>
          {/each}
        </Command.Group>
      </Command.List>
    </Command.Root>
  </Popover.Content>
</Popover.Root>

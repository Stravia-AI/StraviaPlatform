<script lang="ts">
import CheckIcon from '@lucide/svelte/icons/check'
import ChevronsUpDownIcon from '@lucide/svelte/icons/chevrons-up-down'
import { tick } from 'svelte'

import { Button } from '#lib/components/ui/button/index.js'
import * as Command from '#lib/components/ui/command/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'

interface Props {
  id: string
  value: string
  providers: Array<{ id: string; name: string }>
  placeholder: string
  searchPlaceholder: string
  emptyText: string
  ariaLabel: string
  disabled?: boolean
  onSelect: (providerId: string) => void
}

let {
  id,
  value,
  providers,
  placeholder,
  searchPlaceholder,
  emptyText,
  ariaLabel,
  disabled = false,
  onSelect,
}: Props = $props()
let open = $state(false)
let triggerRef = $state<HTMLButtonElement>(null!)
const selectedProvider = $derived(providers.find((provider) => provider.id === value))

function selectProvider(providerId: string): void {
  open = false
  onSelect(providerId)
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
        <span class={['truncate text-left', !selectedProvider && 'text-muted-foreground']}>
          {selectedProvider?.name ?? placeholder}
        </span>
        <ChevronsUpDownIcon data-icon="inline-end" class="ml-auto opacity-50" />
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content align="start" class="w-(--bits-popover-anchor-width) max-w-[calc(100vw-2rem)] p-0">
    <Command.Root {value} label={ariaLabel}>
      <Command.Input placeholder={searchPlaceholder} aria-label={searchPlaceholder} />
      <Command.List>
        <Command.Empty>{emptyText}</Command.Empty>
        <Command.Group>
          {#each providers as provider (provider.id)}
            <Command.Item value={provider.id} keywords={[provider.name]} onSelect={() => selectProvider(provider.id)}>
              <span class="min-w-0 flex-1 truncate">{provider.name}</span>
              {#if provider.id === value}<CheckIcon class="size-4 shrink-0" aria-hidden="true" />{/if}
            </Command.Item>
          {/each}
        </Command.Group>
      </Command.List>
    </Command.Root>
  </Popover.Content>
</Popover.Root>

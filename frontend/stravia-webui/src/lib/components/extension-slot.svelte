<script lang="ts">
import type { Component } from 'svelte'
import { untrack } from 'svelte'

import { ensureBuildExtensionsRegistered, type ExtensionSlotContribution } from '#lib/extensions/index.js'

interface Props {
  slot: string
  capabilities: readonly string[]
  props?: Record<string, unknown>
  class?: string
}

let { slot, capabilities, props: slotProps = {}, class: className = '' }: Props = $props()

const extensions = ensureBuildExtensionsRegistered()

type ComponentSlotContribution = ExtensionSlotContribution & {
  component: NonNullable<ExtensionSlotContribution['component']>
}

const contributions = $derived(
  extensions
    .activeSlots(capabilities, slot)
    .filter((item): item is ComponentSlotContribution => typeof item.component === 'function'),
)

// eslint-disable-next-line @typescript-eslint/no-explicit-any
let loaded = $state<Record<string, Component<any>>>({})

$effect(() => {
  let cancelled = false
  const items = contributions
  const current = untrack(() => loaded)
  for (const item of items) {
    if (current[item.id]) continue
    void item
      .component()
      .then((mod) => {
        if (cancelled) return
        loaded = { ...loaded, [item.id]: mod.default }
      })
      .catch(() => {
        // Keep the host CE page usable if an optional extension chunk fails to load.
      })
  }
  return () => {
    cancelled = true
  }
})
</script>

{#if contributions.length > 0}
  <div class={['flex min-w-0 flex-col gap-4', className]} data-extension-slot={slot}>
    {#each contributions as item (item.id)}
      {@const SlotComponent = loaded[item.id]}
      {#if SlotComponent}
        <SlotComponent {...slotProps} />
      {/if}
    {/each}
  </div>
{/if}

<script lang="ts">
import CopyIcon from '@lucide/svelte/icons/copy'
import MinusIcon from '@lucide/svelte/icons/minus'
import SquareIcon from '@lucide/svelte/icons/square'
import XIcon from '@lucide/svelte/icons/x'

let {
  isMaximized,
  minimizeLabel,
  maximizeLabel,
  restoreLabel,
  closeLabel,
  onMinimize,
  onToggleMaximize,
  onClose,
}: {
  isMaximized: boolean
  minimizeLabel: string
  maximizeLabel: string
  restoreLabel: string
  closeLabel: string
  onMinimize: () => void
  onToggleMaximize: () => void
  onClose: () => void
} = $props()

const toggleLabel = $derived(isMaximized ? restoreLabel : maximizeLabel)
</script>

<div class="flex h-10 items-stretch text-foreground/80" data-no-drag>
  <button
    type="button"
    title={minimizeLabel}
    aria-label={minimizeLabel}
    class="flex h-10 w-12 items-center justify-center rounded-none transition-[background-color,color] duration-[140ms] hover:bg-accent hover:text-accent-foreground"
    onclick={onMinimize}>
    <MinusIcon class="size-4" strokeWidth={1.5} />
  </button>
  <button
    type="button"
    title={toggleLabel}
    aria-label={toggleLabel}
    class="flex h-10 w-12 items-center justify-center rounded-none transition-[background-color,color] duration-[140ms] hover:bg-accent hover:text-accent-foreground"
    onclick={onToggleMaximize}>
    {#if isMaximized}
      <CopyIcon class="size-3.5" strokeWidth={1.5} />
    {:else}
      <SquareIcon class="size-3.5" strokeWidth={1.5} />
    {/if}
  </button>
  <button
    type="button"
    title={closeLabel}
    aria-label={closeLabel}
    class="flex h-10 w-12 items-center justify-center rounded-none transition-[background-color,color] duration-[140ms] hover:bg-destructive hover:text-destructive-foreground"
    onclick={onClose}>
    <XIcon class="size-4" strokeWidth={1.5} />
  </button>
</div>

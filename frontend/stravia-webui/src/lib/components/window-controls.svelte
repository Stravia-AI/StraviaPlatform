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

<div class="flex h-10 items-stretch text-sidebar-foreground/80" data-no-drag>
  <button
    type="button"
    title={minimizeLabel}
    aria-label={minimizeLabel}
    class="flex h-10 w-12 items-center justify-center rounded-none transition-[background-color,color] duration-[140ms] hover:bg-(--shell-control-hover) hover:text-sidebar-foreground active:bg-(--shell-control-active) focus-visible:outline-offset-[-2px]"
    onclick={onMinimize}>
    <MinusIcon class="size-4" strokeWidth={1.5} />
  </button>
  <button
    type="button"
    title={toggleLabel}
    aria-label={toggleLabel}
    class="flex h-10 w-12 items-center justify-center rounded-none transition-[background-color,color] duration-[140ms] hover:bg-(--shell-control-hover) hover:text-sidebar-foreground active:bg-(--shell-control-active) focus-visible:outline-offset-[-2px]"
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
    class="flex h-10 w-12 items-center justify-center rounded-none transition-[background-color,color] duration-[140ms] hover:bg-destructive hover:text-destructive-foreground active:bg-destructive/90 focus-visible:outline-offset-[-2px]"
    onclick={onClose}>
    <XIcon class="size-4" strokeWidth={1.5} />
  </button>
</div>

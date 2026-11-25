<script lang="ts">
import { Select as SelectPrimitive } from 'bits-ui'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import { cn, type WithoutChild } from '#lib/utils.js'

let {
  ref = $bindable(null),
  class: className,
  children,
  size = 'default',
  ...restProps
}: WithoutChild<SelectPrimitive.TriggerProps> & { size?: 'sm' | 'default' } = $props()
</script>

<SelectPrimitive.Trigger
  bind:ref
  data-slot="select-trigger"
  data-size={size}
  class={cn(
    "flex h-8 w-fit items-center justify-between gap-1.5 rounded-lg border border-input bg-background pr-2.5 pl-3 text-sm whitespace-nowrap transition-colors duration-[140ms] outline-none select-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:bg-input/50 disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 data-placeholder:text-muted-foreground data-[size=sm]:h-7 *:data-[slot=select-value]:line-clamp-1 *:data-[slot=select-value]:flex *:data-[slot=select-value]:items-center *:data-[slot=select-value]:gap-1.5 dark:bg-input/30 dark:hover:bg-input/50 dark:disabled:bg-input/80 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
    className,
  )}
  {...restProps}>
  {@render children?.()}
  <ChevronDownIcon class="size-4 text-muted-foreground pointer-events-none" />
</SelectPrimitive.Trigger>

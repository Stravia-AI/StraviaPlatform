<script lang="ts">
import { cn, type WithElementRef } from '$lib/utils.js'
import type { HTMLAttributes } from 'svelte/elements'

let {
  ref = $bindable(null),
  class: className,
  children,
  ...restProps
}: WithElementRef<HTMLAttributes<HTMLElement>> = $props()
</script>

<div
  bind:this={ref}
  data-slot="sidebar-menu-badge"
  data-sidebar="menu-badge"
  class={cn(
    'pointer-events-none absolute top-1/2 right-1 flex h-5 min-w-5 -translate-y-1/2 items-center justify-center rounded-md px-1 text-xs font-medium text-sidebar-foreground tabular-nums select-none peer-hover/menu-button:text-sidebar-accent-foreground peer-data-[active=true]/menu-button:text-sidebar-accent-foreground group-data-[collapsible=icon]:hidden',
    className,
  )}
  {...restProps}>
  {@render children?.()}
</div>

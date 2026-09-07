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
  data-slot="sidebar-content"
  data-sidebar="content"
  class={cn('gap-0 flex min-h-0 flex-1 flex-col overflow-x-hidden overflow-y-auto', className)}
  {...restProps}>
  {@render children?.()}
</div>

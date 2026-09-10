<script lang="ts">
import { cn, type WithElementRef } from '$lib/utils.js'
import type { HTMLAttributes } from 'svelte/elements'

let {
  ref = $bindable(null),
  class: className,
  children,
  ...restProps
}: WithElementRef<HTMLAttributes<HTMLSpanElement>> = $props()
</script>

<span
  bind:this={ref}
  data-slot="marker-content"
  class={cn(
    'group-data-[variant=separator]/marker:flex-none group-data-[variant=separator]/marker:text-center *:[a]:underline *:[a]:underline-offset-3 *:[a]:hover:text-foreground min-w-0 wrap-break-word',
    className,
  )}
  {...restProps}>
  {@render children?.()}
</span>

<script lang="ts">
import { cn, type WithElementRef, type WithoutChildren } from '#lib/utils.js'
import type { HTMLTextareaAttributes } from 'svelte/elements'

let {
  ref = $bindable(null),
  value = $bindable(),
  class: className,
  'data-slot': dataSlot = 'textarea',
  ...restProps
}: WithoutChildren<WithElementRef<HTMLTextareaAttributes>> = $props()
</script>

<textarea
  bind:this={ref}
  data-slot={dataSlot}
  class={cn(
    'flex field-sizing-content min-h-16 w-full min-w-0 rounded-lg border border-input/85 bg-background px-3 py-2 text-base transition-colors duration-[140ms] outline-none placeholder:text-muted-foreground hover:border-input focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:bg-input/50 disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 md:text-sm dark:border-input/80 dark:bg-background/65 dark:hover:border-input dark:disabled:bg-input/80 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40',
    className,
  )}
  bind:value
  {...restProps}></textarea>

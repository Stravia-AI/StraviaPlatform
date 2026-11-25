<script lang="ts">
import { cn } from '#lib/utils.js'
import type { InputProps } from './props.js'

type Props = InputProps

const inputClasses =
  'h-8 w-full min-w-0 rounded-lg border border-input bg-background px-3 py-1 text-base transition-colors duration-[140ms] outline-none file:inline-flex file:h-8 file:border-0 file:bg-transparent file:text-sm file:font-medium file:text-foreground placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:cursor-not-allowed disabled:bg-input/50 disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 md:text-sm dark:bg-input/30 dark:disabled:bg-input/80 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40'

let {
  ref = $bindable(null),
  value = $bindable(),
  type,
  files = $bindable(),
  class: className,
  'data-slot': dataSlot = 'input',
  ...restProps
}: Props = $props()
</script>

{#if type === 'file'}
  <input
    bind:this={ref}
    data-slot={dataSlot}
    class={cn(inputClasses, className)}
    type="file"
    bind:files
    bind:value
    {...restProps} />
{:else}
  <input bind:this={ref} data-slot={dataSlot} class={cn(inputClasses, className)} {type} bind:value {...restProps} />
{/if}

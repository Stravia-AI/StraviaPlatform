<script lang="ts">
import { resolve } from '$app/paths'

import { cn } from '$lib/utils.js'
import { buttonVariants, type ButtonProps } from './variants.js'

let {
  class: className,
  variant = 'default',
  size = 'default',
  ref = $bindable(null),
  href = undefined,
  type = 'button',
  disabled,
  children,
  ...restProps
}: ButtonProps = $props()
</script>

{#if href}
  {#if disabled}
    <a
      bind:this={ref}
      data-slot="button"
      class={cn(buttonVariants({ variant, size }), className)}
      aria-disabled="true"
      role="link"
      tabindex={-1}
      {...restProps}>
      {@render children?.()}
    </a>
  {:else}
    <a
      bind:this={ref}
      data-slot="button"
      class={cn(buttonVariants({ variant, size }), className)}
      href={resolve(href as '/')}
      {...restProps}>
      {@render children?.()}
    </a>
  {/if}
{:else}
  <button
    bind:this={ref}
    data-slot="button"
    class={cn(buttonVariants({ variant, size }), className)}
    {type}
    {disabled}
    {...restProps}>
    {@render children?.()}
  </button>
{/if}

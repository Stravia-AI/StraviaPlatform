<script lang="ts" module>
import { tv, type VariantProps } from 'tailwind-variants'

export const markerVariants = tv({
  base: "min-h-4 gap-2 text-left text-sm text-muted-foreground [&_svg:not([class*='size-'])]:size-4 [a]:underline [a]:underline-offset-3 [a]:hover:text-foreground group/marker relative flex w-full items-center",
  variants: {
    variant: {
      default: 'cn-marker-variant-default',
      separator:
        'before:mr-1 before:h-px before:min-w-0 before:flex-1 before:bg-border after:ml-1 after:h-px after:min-w-0 after:flex-1 after:bg-border',
      border: 'border-b border-border pb-2',
    },
  },
  defaultVariants: { variant: 'default' },
})

export type MarkerVariant = VariantProps<typeof markerVariants>['variant']
</script>

<script lang="ts">
import { cn, type WithElementRef } from '$lib/utils.js'
import type { Snippet } from 'svelte'
import type { HTMLAttributes } from 'svelte/elements'

let {
  ref = $bindable(null),
  class: className,
  variant = 'default',
  child,
  ...restProps
}: WithElementRef<HTMLAttributes<HTMLDivElement>> & {
  variant?: MarkerVariant
  child?: Snippet<[{ props: Record<string, unknown> }]>
} = $props()

const mergedProps = $derived({
  class: cn(markerVariants({ variant }), className),
  'data-slot': 'marker',
  'data-variant': variant,
  ...restProps,
})
</script>

{#if child}
  {@render child({ props: mergedProps })}
{:else}
  <div bind:this={ref} {...mergedProps}>
    {@render mergedProps.children?.()}
  </div>
{/if}

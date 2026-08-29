<script lang="ts" module>
import { tv, type VariantProps } from 'tailwind-variants'

export const fieldVariants = tv({
  base: 'data-[invalid=true]:text-destructive group/field @container/field w-full min-w-0 [--field-control-width:36rem]',
  variants: {
    orientation: {
      vertical: '',
      horizontal: '',
      responsive: '',
    },
    size: {
      number: '[--field-control-width:7rem]',
      datetime: '[--field-control-width:18rem]',
      name: '[--field-control-width:24rem]',
      select: '[--field-control-width:28rem]',
      fill: '[--field-control-width:36rem]',
    },
  },
  compoundVariants: [
    { orientation: 'vertical', size: 'number', class: 'max-w-[7rem]' },
    { orientation: 'vertical', size: 'datetime', class: 'max-w-[18rem]' },
    { orientation: 'vertical', size: 'name', class: 'max-w-[24rem]' },
    { orientation: 'vertical', size: 'select', class: 'max-w-[28rem]' },
    { orientation: 'vertical', size: 'fill', class: 'max-w-[36rem]' },
  ],
  defaultVariants: { orientation: 'responsive' },
})

export type FieldOrientation = VariantProps<typeof fieldVariants>['orientation']
export type FieldSize = VariantProps<typeof fieldVariants>['size']
</script>

<script lang="ts">
import { cn, type WithElementRef } from '$lib/utils.js'
import type { HTMLAttributes } from 'svelte/elements'

let {
  ref = $bindable(null),
  class: className,
  orientation = 'responsive',
  size,
  children,
  ...restProps
}: WithElementRef<HTMLAttributes<HTMLDivElement>> & { orientation?: FieldOrientation; size?: FieldSize } = $props()
</script>

<div
  bind:this={ref}
  role="group"
  data-slot="field"
  data-orientation={orientation}
  data-size={size}
  class={cn(fieldVariants({ orientation, size }), className)}
  {...restProps}>
  <div
    data-slot="field-layout"
    class={cn(
      'gap-2 flex min-w-0',
      orientation === 'horizontal'
        ? 'cn-field-orientation-horizontal min-h-10 w-full flex-row items-center has-[>[data-slot=field-content]]:items-start [&>[data-slot=field-label]]:flex-auto has-[>[data-slot=field-content]]:[&>[role=checkbox],[role=radio]]:mt-px'
        : 'w-full flex-col [&>.sr-only]:w-auto',
      orientation === 'vertical' && 'cn-field-orientation-vertical',
      orientation === 'responsive' && 'cn-field-orientation-responsive',
    )}>
    {@render children?.()}
  </div>
</div>

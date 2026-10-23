<script lang="ts" module>
import { tv, type VariantProps } from 'tailwind-variants'

import type { FieldControlSize } from './field-sizes.js'

export const fieldVariants = tv({
  base: 'data-[invalid=true]:text-destructive group/field @container/field w-full min-w-0',
  variants: {
    // 左右排布的 Field 以设置列为上限，label 与控件的距离不随内容容器放大。
    orientation: {
      vertical: '',
      horizontal: 'max-w-(--content-column-max-width)',
      responsive: 'max-w-(--content-column-max-width)',
    },
  },
  defaultVariants: { orientation: 'responsive' },
})

export type FieldOrientation = VariantProps<typeof fieldVariants>['orientation']
export type FieldSize = FieldControlSize
</script>

<script lang="ts">
import { cn, type WithElementRef } from '#lib/utils.js'
import { fieldControlWidths } from './field-sizes.js'
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
  style:--field-control-width={fieldControlWidths[size ?? 'fill']}
  class={cn(
    fieldVariants({ orientation }),
    orientation === 'vertical' && size && 'max-w-(--field-control-width)',
    className,
  )}
  {...restProps}>
  <div
    data-slot="field-layout"
    class={cn(
      'gap-2 flex min-w-0',
      orientation === 'horizontal'
        ? 'cn-field-orientation-horizontal w-full flex-row items-center has-[>[data-slot=field-content]]:items-start [&>[data-slot=field-label]]:flex-auto has-[>[data-slot=field-content]]:[&>[role=checkbox],[role=radio]]:mt-px'
        : 'w-full flex-col [&>.sr-only]:w-auto',
      orientation === 'vertical' && 'cn-field-orientation-vertical',
      orientation === 'responsive' && 'cn-field-orientation-responsive',
    )}>
    {@render children?.()}
  </div>
</div>

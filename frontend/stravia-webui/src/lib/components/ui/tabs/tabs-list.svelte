<script lang="ts" module>
import { tv, type VariantProps } from 'tailwind-variants'

export const tabsListVariants = tv({
  base: 'rounded-lg p-1 data-[variant=line]:rounded-none group/tabs-list inline-flex h-auto w-fit max-w-full flex-wrap items-center justify-start gap-y-2 text-muted-foreground group-data-[orientation=vertical]/tabs:flex-col',
  variants: {
    variant: {
      default: 'cn-tabs-list-variant-default bg-muted',
      line: 'cn-tabs-list-variant-line gap-1 bg-transparent',
    },
  },
  defaultVariants: { variant: 'default' },
})

export type TabsListVariant = VariantProps<typeof tabsListVariants>['variant']
</script>

<script lang="ts">
import { Tabs as TabsPrimitive } from 'bits-ui'
import { cn } from '$lib/utils.js'

let {
  ref = $bindable(null),
  variant = 'default',
  class: className,
  ...restProps
}: TabsPrimitive.ListProps & { variant?: TabsListVariant } = $props()
</script>

<TabsPrimitive.List
  bind:ref
  data-slot="tabs-list"
  data-variant={variant}
  class={cn(tabsListVariants({ variant }), className)}
  {...restProps} />

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

export const tabsTriggerVariants = tv({
  base: [
    "gap-1.5 rounded-md border border-transparent px-2.5 py-0.5 text-sm font-medium has-data-[icon=inline-end]:pr-2 has-data-[icon=inline-start]:pl-2 group-data-[variant=default]/tabs-list:data-active:shadow-sm group-data-[variant=line]/tabs-list:data-active:shadow-none [&_svg:not([class*='size-'])]:size-4 relative inline-flex min-h-10 max-w-full flex-none items-center justify-center whitespace-normal text-muted-foreground transition-[background-color,border-color,color,box-shadow,opacity] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] group-data-[orientation=vertical]/tabs:w-full group-data-[orientation=vertical]/tabs:justify-start hover:text-foreground focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 focus-visible:outline-1 focus-visible:outline-ring disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0",
    'group-data-[variant=line]/tabs-list:bg-transparent group-data-[variant=line]/tabs-list:data-active:bg-transparent dark:group-data-[variant=line]/tabs-list:data-active:border-transparent dark:group-data-[variant=line]/tabs-list:data-active:bg-transparent',
    'data-active:bg-background data-active:text-foreground dark:data-active:border-input dark:data-active:bg-input/30 dark:data-active:text-foreground',
    'after:absolute after:bg-foreground after:opacity-0 after:transition-opacity group-data-[orientation=horizontal]/tabs-list:after:inset-x-0 group-data-[orientation=horizontal]/tabs-list:after:bottom-[-5px] group-data-[orientation=horizontal]/tabs-list:after:h-0.5 group-data-[orientation=vertical]/tabs-list:after:inset-y-0 group-data-[orientation=vertical]/tabs-list:after:-right-1 group-data-[orientation=vertical]/tabs-list:after:w-0.5 group-data-[variant=line]/tabs-list:data-active:after:opacity-100',
  ],
})

export type TabsListVariant = VariantProps<typeof tabsListVariants>['variant']

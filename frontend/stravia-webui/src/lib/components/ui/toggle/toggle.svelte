<script lang="ts" module>
import { type VariantProps, tv } from 'tailwind-variants'

export const toggleVariants = tv({
  base: "gap-1 rounded-lg text-sm font-medium transition-[background-color,border-color,color,box-shadow,transform,opacity] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] active:scale-[0.96] hover:text-foreground focus-visible:border-ring focus-visible:ring-ring/50 aria-invalid:border-destructive aria-invalid:ring-destructive/20 aria-pressed:bg-muted data-[state=on]:bg-muted dark:aria-invalid:ring-destructive/40 [&_svg:not([class*='size-'])]:size-4 group/toggle inline-flex items-center justify-center whitespace-nowrap outline-none hover:bg-muted focus-visible:ring-[3px] disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0",
  variants: {
    variant: { default: 'bg-transparent', outline: 'border border-input bg-transparent hover:bg-muted' },
    size: {
      default: 'h-10 min-w-10 px-3 has-data-[icon=inline-end]:pr-2.5 has-data-[icon=inline-start]:pl-2.5',
      sm: "h-10 min-w-10 rounded-md px-2.5 text-[0.8rem] has-data-[icon=inline-end]:pr-2 has-data-[icon=inline-start]:pl-2 [&_svg:not([class*='size-'])]:size-3.5",
      lg: 'h-11 min-w-11 px-3 has-data-[icon=inline-end]:pr-2.5 has-data-[icon=inline-start]:pl-2.5',
    },
  },
  defaultVariants: { variant: 'default', size: 'default' },
})

export type ToggleVariant = VariantProps<typeof toggleVariants>['variant']
export type ToggleSize = VariantProps<typeof toggleVariants>['size']
export type ToggleVariants = VariantProps<typeof toggleVariants>
</script>

<script lang="ts">
import { Toggle as TogglePrimitive } from 'bits-ui'
import { cn } from '$lib/utils.js'

let {
  ref = $bindable(null),
  pressed = $bindable(false),
  class: className,
  size = 'default',
  variant = 'default',
  ...restProps
}: TogglePrimitive.RootProps & { variant?: ToggleVariant; size?: ToggleSize } = $props()
</script>

<TogglePrimitive.Root
  bind:ref
  bind:pressed
  data-slot="toggle"
  class={cn(toggleVariants({ variant, size }), className)}
  {...restProps} />

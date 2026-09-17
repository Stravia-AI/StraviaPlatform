<script lang="ts" module>
import { tv, type VariantProps } from 'tailwind-variants'

const inputGroupButtonVariants = tv({
  base: 'gap-2 text-sm flex items-center shadow-none',
  variants: {
    size: {
      xs: "h-10 min-w-10 gap-1 rounded-md px-2 [&>svg:not([class*='size-'])]:size-3.5",
      sm: 'cn-input-group-button-size-sm',
      'icon-xs': 'size-10 rounded-md p-0 has-[>svg]:p-0',
      'icon-sm': 'size-10 rounded-md p-0 has-[>svg]:p-0',
    },
  },
  defaultVariants: { size: 'xs' },
})

export type InputGroupButtonSize = VariantProps<typeof inputGroupButtonVariants>['size']
</script>

<script lang="ts">
import { Button, type ButtonProps } from '$lib/components/ui/button/index.js'
import { cn } from '$lib/utils.js'

let {
  ref = $bindable(null),
  class: className,
  children,
  type = 'button',
  variant = 'ghost',
  size = 'xs',
  ...restProps
}: Omit<ButtonProps, 'href' | 'size'> & { size?: InputGroupButtonSize } = $props()
</script>

<Button
  bind:ref
  {type}
  data-size={size}
  {variant}
  class={cn(inputGroupButtonVariants({ size }), className)}
  {...restProps}>
  {@render children?.()}
</Button>

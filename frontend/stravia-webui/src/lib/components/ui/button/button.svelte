<script lang="ts" module>
import { type VariantProps, tv } from 'tailwind-variants'
import { cn, type WithElementRef } from '$lib/utils.js'
import type { HTMLAnchorAttributes, HTMLButtonAttributes } from 'svelte/elements'

export const buttonVariants = tv({
  base: "rounded-lg border border-transparent bg-clip-padding text-sm font-medium focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 active:not-aria-[haspopup]:scale-[0.96] aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40 [&_svg:not([class*='size-'])]:size-4 group/button inline-flex shrink-0 items-center justify-center whitespace-nowrap transition-[background-color,border-color,color,box-shadow,transform,opacity] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] outline-none select-none disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0",
  variants: {
    variant: {
      default: 'bg-primary text-primary-foreground [a]:hover:bg-primary/80',
      outline:
        'border-border bg-background hover:bg-muted hover:text-foreground aria-expanded:bg-muted aria-expanded:text-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50',
      secondary:
        'bg-secondary text-secondary-foreground hover:bg-secondary/80 aria-expanded:bg-secondary aria-expanded:text-secondary-foreground',
      ghost:
        'hover:bg-muted hover:text-foreground aria-expanded:bg-muted aria-expanded:text-foreground dark:hover:bg-muted/50',
      destructive:
        'bg-destructive text-destructive-foreground hover:bg-destructive/90 focus-visible:border-destructive focus-visible:ring-destructive/30 dark:bg-destructive dark:hover:bg-destructive/90 dark:focus-visible:ring-destructive/40',
      link: 'text-primary underline-offset-4 hover:underline',
    },
    size: {
      default: 'h-10 gap-1.5 px-3 has-data-[icon=inline-end]:pr-2.5 has-data-[icon=inline-start]:pl-2.5',
      xs: "h-10 gap-1 rounded-md px-2.5 text-xs in-data-[slot=button-group]:rounded-lg has-data-[icon=inline-end]:pr-2 has-data-[icon=inline-start]:pl-2 [&_svg:not([class*='size-'])]:size-3",
      sm: "h-10 gap-1 rounded-md px-3 text-[0.8rem] in-data-[slot=button-group]:rounded-lg has-data-[icon=inline-end]:pr-2.5 has-data-[icon=inline-start]:pl-2.5 [&_svg:not([class*='size-'])]:size-3.5",
      lg: 'h-11 gap-1.5 px-4 has-data-[icon=inline-end]:pr-3 has-data-[icon=inline-start]:pl-3',
      icon: 'size-10',
      'icon-xs': "size-10 rounded-md in-data-[slot=button-group]:rounded-lg [&_svg:not([class*='size-'])]:size-3",
      'icon-sm': 'size-10 rounded-md in-data-[slot=button-group]:rounded-lg',
      'icon-lg': 'size-11',
    },
  },
  defaultVariants: { variant: 'default', size: 'default' },
})

export type ButtonVariant = VariantProps<typeof buttonVariants>['variant']
export type ButtonSize = VariantProps<typeof buttonVariants>['size']

export type ButtonProps = WithElementRef<HTMLButtonAttributes> &
  WithElementRef<HTMLAnchorAttributes> & { variant?: ButtonVariant; size?: ButtonSize }
</script>

<script lang="ts">
import { resolve } from '$app/paths'

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

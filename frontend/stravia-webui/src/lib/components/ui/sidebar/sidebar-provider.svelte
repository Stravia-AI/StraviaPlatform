<script lang="ts">
import * as Tooltip from '$lib/components/ui/tooltip/index.js'
import { cn, type WithElementRef } from '$lib/utils.js'
import { SIDEBAR_WIDTH, SIDEBAR_WIDTH_ICON } from './constants.js'
import { setSidebar } from './context.svelte.js'
import type { HTMLAttributes } from 'svelte/elements'

let {
  ref = $bindable(null),
  open = $bindable(true),
  openMobile = $bindable(false),
  isMobile,
  onOpenChange,
  class: className,
  style,
  children,
  ...restProps
}: WithElementRef<HTMLAttributes<HTMLDivElement>> & {
  open?: boolean
  openMobile?: boolean
  isMobile: boolean
  onOpenChange?: (open: boolean) => void
} = $props()

setSidebar({
  open: () => open,
  setOpen: (value) => {
    open = value
    onOpenChange?.(value)
  },
  openMobile: () => openMobile,
  setOpenMobile: (value) => {
    openMobile = value
  },
  isMobile: () => isMobile,
})
</script>

<Tooltip.Provider delayDuration={0}>
  <div
    data-slot="sidebar-wrapper"
    style="--sidebar-width: {SIDEBAR_WIDTH}; --sidebar-width-icon: {SIDEBAR_WIDTH_ICON}; {style ?? ''}"
    class={cn('group/sidebar-wrapper flex min-h-svh w-full has-data-[variant=inset]:bg-sidebar', className)}
    bind:this={ref}
    {...restProps}>
    {@render children?.()}
  </div>
</Tooltip.Provider>

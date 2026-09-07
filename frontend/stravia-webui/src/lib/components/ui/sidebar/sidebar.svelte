<script lang="ts">
import * as Sheet from '$lib/components/ui/sheet/index.js'
import { cn, type WithElementRef } from '$lib/utils.js'
import { SIDEBAR_WIDTH_MOBILE } from './constants.js'
import { useSidebar } from './context.svelte.js'
import type { ComponentProps } from 'svelte'
import type { HTMLAttributes } from 'svelte/elements'

let {
  ref = $bindable(null),
  side = 'left',
  variant = 'sidebar',
  collapsible = 'offcanvas',
  class: className,
  children,
  title,
  description,
  closeLabel,
  onOpenAutoFocus,
  onCloseAutoFocus,
  ...restProps
}: WithElementRef<HTMLAttributes<HTMLDivElement>> & {
  title: string
  description?: string
  closeLabel?: string
  onOpenAutoFocus?: ComponentProps<typeof Sheet.Content>['onOpenAutoFocus']
  onCloseAutoFocus?: ComponentProps<typeof Sheet.Content>['onCloseAutoFocus']
  side?: 'left' | 'right'
  variant?: 'sidebar' | 'floating' | 'inset'
  collapsible?: 'offcanvas' | 'icon' | 'none'
} = $props()

const sidebar = useSidebar()
</script>

{#if collapsible === 'none'}
  <div
    class={cn('flex h-full w-(--sidebar-width) flex-col bg-sidebar text-sidebar-foreground', className)}
    bind:this={ref}
    {...restProps}>
    {@render children?.()}
  </div>
{:else if sidebar.isMobile}
  <Sheet.Root bind:open={() => sidebar.openMobile, (v) => sidebar.setOpenMobile(v)} {...restProps}>
    <Sheet.Content
      bind:ref
      data-sidebar="sidebar"
      data-slot="sidebar"
      data-mobile="true"
      class={cn(
        'data-[side=left]:w-[min(var(--sidebar-width),100vw)] data-[side=left]:sm:max-w-[var(--sidebar-width)] data-[side=right]:w-[min(var(--sidebar-width),100vw)] data-[side=right]:sm:max-w-[var(--sidebar-width)] bg-sidebar gap-0 p-0 text-sidebar-foreground',
        className,
      )}
      style="--sidebar-width: {SIDEBAR_WIDTH_MOBILE};"
      {side}
      {closeLabel}
      {onOpenAutoFocus}
      {onCloseAutoFocus}>
      <Sheet.Header class="sr-only">
        <Sheet.Title>{title}</Sheet.Title>
        {#if description}<Sheet.Description>{description}</Sheet.Description>{/if}
      </Sheet.Header>
      <div class="flex h-full w-full flex-col">
        {@render children?.()}
      </div>
    </Sheet.Content>
  </Sheet.Root>
{:else}
  <div
    bind:this={ref}
    class="group peer hidden text-sidebar-foreground md:block"
    data-state={sidebar.state}
    data-collapsible={sidebar.state === 'collapsed' ? collapsible : ''}
    data-variant={variant}
    data-side={side}
    data-slot="sidebar">
    <!-- This is what handles the sidebar gap on desktop -->
    <div
      data-slot="sidebar-gap"
      class={cn(
        'transition-[width] duration-200 ease-linear relative w-(--sidebar-width) bg-transparent',
        'group-data-[collapsible=offcanvas]:w-0',
        'group-data-[side=right]:rotate-180',
        variant === 'floating' || variant === 'inset'
          ? 'group-data-[collapsible=icon]:w-[calc(var(--sidebar-width-icon)+(--spacing(4)))]'
          : 'group-data-[collapsible=icon]:w-(--sidebar-width-icon)',
      )}>
    </div>
    <div
      data-slot="sidebar-container"
      class={cn(
        'absolute top-[var(--sidebar-top,0px)] bottom-0 z-10 hidden w-(--sidebar-width) transition-[left,right,width] duration-200 ease-linear md:flex',
        side === 'left'
          ? 'start-0 group-data-[collapsible=offcanvas]:start-[calc(var(--sidebar-width)*-1)]'
          : 'end-0 group-data-[collapsible=offcanvas]:end-[calc(var(--sidebar-width)*-1)]',
        // Adjust the padding for floating and inset variants.
        variant === 'floating' || variant === 'inset'
          ? 'p-2 group-data-[collapsible=icon]:w-[calc(var(--sidebar-width-icon)+(--spacing(4))+2px)]'
          : 'group-data-[collapsible=icon]:w-(--sidebar-width-icon)',
        className,
      )}
      {...restProps}>
      <div
        data-sidebar="sidebar"
        data-slot="sidebar-inner"
        class="bg-[var(--sidebar-surface,var(--sidebar))] group-data-[variant=floating]:rounded-lg group-data-[variant=floating]:shadow-sm group-data-[variant=floating]:ring-1 group-data-[variant=floating]:ring-sidebar-border flex size-full flex-col">
        {@render children?.()}
      </div>
    </div>
  </div>
{/if}

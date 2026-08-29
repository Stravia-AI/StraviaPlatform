<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { Dialog as DialogPrimitive } from 'bits-ui'
import XIcon from '@lucide/svelte/icons/x'
import { Button } from '$lib/components/ui/button/index.js'
import { cn, type WithoutChildrenOrChild } from '$lib/utils.js'
import * as Dialog from './index.js'
import DialogPortal from './dialog-portal.svelte'
import type { Snippet } from 'svelte'
import type { ComponentProps } from 'svelte'

let {
  ref = $bindable(null),
  class: className,
  portalProps,
  children,
  showCloseButton = true,
  ...restProps
}: WithoutChildrenOrChild<DialogPrimitive.ContentProps> & {
  portalProps?: WithoutChildrenOrChild<ComponentProps<typeof DialogPortal>>
  children: Snippet
  showCloseButton?: boolean
} = $props()
</script>

<DialogPortal {...portalProps}>
  <Dialog.Overlay />
  <DialogPrimitive.Content
    bind:ref
    data-slot="dialog-content"
    class={cn(
      'grid grid-cols-[minmax(0,1fr)] max-w-[calc(100%-2rem)] gap-4 rounded-xl bg-popover p-4 text-sm text-popover-foreground ring-1 ring-foreground/10 duration-[220ms] ease-[cubic-bezier(0.2,0,0,1)] sm:max-w-sm data-open:animate-in data-open:fade-in-0 data-open:zoom-in-95 data-closed:animate-out data-closed:fade-out-0 data-closed:zoom-out-95 fixed top-1/2 left-1/2 z-50 w-full -translate-x-1/2 -translate-y-1/2 outline-none',
      className,
    )}
    {...restProps}>
    {@render children?.()}
    {#if showCloseButton}
      <DialogPrimitive.Close data-slot="dialog-close">
        {#snippet child({ props })}
          <Button variant="ghost" class="absolute top-2 right-2" size="icon-sm" {...props}>
            <XIcon />
            <span class="sr-only">{m.common_close()}</span>
          </Button>
        {/snippet}
      </DialogPrimitive.Close>
    {/if}
  </DialogPrimitive.Content>
</DialogPortal>

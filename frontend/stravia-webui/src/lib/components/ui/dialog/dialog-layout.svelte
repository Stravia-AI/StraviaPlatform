<script lang="ts">
import type { Dialog as DialogPrimitive } from 'bits-ui'
import type { Snippet } from 'svelte'

import { cn, type WithoutChildrenOrChild } from '#lib/utils.js'
import Content from './dialog-content.svelte'

let {
  ref = $bindable(null),
  class: className,
  header,
  footer,
  children,
  showCloseButton = true,
  ...restProps
}: WithoutChildrenOrChild<DialogPrimitive.ContentProps> & {
  portalProps?: WithoutChildrenOrChild<DialogPrimitive.PortalProps>
  header?: Snippet
  footer?: Snippet
  children?: Snippet
  showCloseButton?: boolean
} = $props()
</script>

<!--
  圆角裁剪放在内层 frame，Content 保持 overflow: visible：Content 带 transform，是 fixed 浮层的包含块，
  若它裁剪，内联渲染的下拉候选（如 SearchCombobox）会被对话框边缘截断。
-->
<Content bind:ref class={cn('gap-0 p-0', className)} {showCloseButton} {...restProps}>
  <div
    data-slot="dialog-frame"
    class="flex max-h-[calc(100dvh-2rem)] min-w-0 flex-col overflow-hidden rounded-[inherit]">
    {#if header}
      <div
        data-slot="dialog-header"
        class={cn('flex shrink-0 flex-col gap-2 p-4', children && 'border-b', showCloseButton && 'pr-14')}>
        {@render header()}
      </div>
    {/if}
    {#if children}
      <div
        data-slot="dialog-body"
        class={cn(
          'flex min-h-0 min-w-0 flex-col gap-4 overflow-y-auto overscroll-contain p-4',
          !header && showCloseButton && 'pr-14',
        )}>
        {@render children()}
      </div>
    {/if}
    {#if footer}
      <div
        data-slot="dialog-footer"
        class={cn(
          'flex shrink-0 flex-col-reverse gap-2 bg-muted/50 p-4 sm:flex-row sm:justify-end',
          (header || children) && 'border-t',
        )}>
        {@render footer()}
      </div>
    {/if}
  </div>
</Content>

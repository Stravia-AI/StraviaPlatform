<script lang="ts">
import PanelLeftIcon from '@lucide/svelte/icons/panel-left'
import { Button, type ButtonProps } from '$lib/components/ui/button/index.js'
import { cn } from '$lib/utils.js'
import { useSidebar } from './context.svelte.js'

let { ref = $bindable(null), class: className, onclick, ...restProps }: ButtonProps = $props()

const sidebar = useSidebar()
</script>

<Button
  bind:ref
  data-sidebar="trigger"
  data-slot="sidebar-trigger"
  variant="ghost"
  size="icon-sm"
  class={cn('cn-sidebar-trigger', className)}
  type="button"
  onclick={(e: MouseEvent) => {
    ;(onclick as ((event: MouseEvent) => void) | undefined)?.(e)
    sidebar.toggle()
  }}
  {...restProps}>
  <PanelLeftIcon />
  <span class="sr-only">Toggle Sidebar</span>
</Button>

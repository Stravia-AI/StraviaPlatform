<script lang="ts">
import { Separator } from '$lib/components/ui/separator/index.js'
import { cn } from '$lib/utils.js'
import type { Separator as SeparatorPrimitive } from 'bits-ui'

let { ref = $bindable(null), class: className, ...restProps }: SeparatorPrimitive.RootProps = $props()
</script>

<Separator
  bind:ref
  data-slot="sidebar-separator"
  data-sidebar="separator"
  class={cn('mx-2 bg-sidebar-border w-auto', className)}
  {...restProps} />

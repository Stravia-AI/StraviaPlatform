<script lang="ts">
import { Switch as SwitchPrimitive } from 'bits-ui'
import { cn, type WithoutChildrenOrChild } from '$lib/utils.js'

let {
  ref = $bindable(null),
  class: className,
  checked = $bindable(false),
  size = 'default',
  ...restProps
}: WithoutChildrenOrChild<SwitchPrimitive.RootProps> & { size?: 'sm' | 'default' } = $props()
</script>

<SwitchPrimitive.Root
  bind:ref
  bind:checked
  data-slot="switch"
  data-size={size}
  class={cn(
    'peer group/switch relative inline-flex size-10 shrink-0 items-center justify-center rounded-md border border-transparent outline-none transition-[border-color,box-shadow] duration-[180ms] ease-[cubic-bezier(0.2,0,0,1)] focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 data-disabled:cursor-not-allowed data-disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40',
    className,
  )}
  {...restProps}>
  <span
    data-slot="switch-track"
    class="pointer-events-none inline-flex shrink-0 items-center justify-center rounded-full bg-input transition-colors duration-[180ms] group-data-[size=default]/switch:h-[18px] group-data-[size=default]/switch:w-8 group-data-[size=sm]/switch:h-3.5 group-data-[size=sm]/switch:w-6 group-data-[state=checked]/switch:bg-primary dark:bg-input/80">
    <SwitchPrimitive.Thumb
      data-slot="switch-thumb"
      class="pointer-events-none block -translate-x-1.5 rounded-full bg-background ring-0 transition-transform duration-[180ms] group-data-[state=checked]/switch:translate-x-1.5 group-data-[size=default]/switch:size-4 group-data-[size=sm]/switch:size-3 dark:bg-foreground dark:group-data-[state=checked]/switch:bg-primary-foreground" />
  </span>
</SwitchPrimitive.Root>

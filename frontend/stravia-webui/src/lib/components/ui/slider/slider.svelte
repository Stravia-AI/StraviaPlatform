<script lang="ts">
import { Slider as SliderPrimitive } from 'bits-ui'
import { cn } from '$lib/utils.js'

let {
  ref = $bindable(null),
  value = $bindable(0),
  orientation = 'horizontal',
  class: className,
  min = 0,
  max = 100,
  step = 1,
  disabled = false,
  id,
  'aria-label': ariaLabel,
}: {
  ref?: HTMLElement | null
  value?: number
  orientation?: 'horizontal' | 'vertical'
  class?: string
  min?: number
  max?: number
  step?: number
  disabled?: boolean
  id?: string
  'aria-label'?: string
} = $props()
</script>

<SliderPrimitive.Root
  type="single"
  bind:ref
  bind:value
  {min}
  {max}
  {step}
  {disabled}
  {id}
  {orientation}
  data-slot="slider"
  class={cn(
    'relative flex w-full touch-none items-center select-none data-disabled:opacity-50 data-[orientation=vertical]:h-full data-[orientation=vertical]:min-h-40 data-[orientation=vertical]:w-auto data-[orientation=vertical]:flex-col',
    className,
  )}>
  {#snippet children({ thumbItems })}
    <span
      data-slot="slider-track"
      data-orientation={orientation}
      class="relative grow overflow-hidden rounded-full bg-input data-[orientation=horizontal]:h-1 data-[orientation=horizontal]:w-full data-[orientation=vertical]:h-full data-[orientation=vertical]:w-1">
      <SliderPrimitive.Range data-slot="slider-range" class="absolute bg-primary select-none data-[orientation=horizontal]:h-full data-[orientation=vertical]:w-full" />
    </span>
    {#each thumbItems as thumb (thumb.index)}
      <SliderPrimitive.Thumb
        data-slot="slider-thumb"
        index={thumb.index}
        aria-label={ariaLabel}
        class="relative block size-3 shrink-0 rounded-full border border-ring bg-background ring-ring/50 transition-[color,box-shadow] after:absolute after:-inset-[0.875rem] select-none hover:ring-3 focus-visible:ring-3 focus-visible:outline-hidden active:ring-3 disabled:pointer-events-none disabled:opacity-50" />
    {/each}
  {/snippet}
</SliderPrimitive.Root>

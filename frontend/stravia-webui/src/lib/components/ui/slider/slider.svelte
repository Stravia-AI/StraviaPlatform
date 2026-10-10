<script lang="ts">
import { Slider as SliderPrimitive } from 'bits-ui'
import { cn } from '$lib/utils.js'

let {
  ref = $bindable(null),
  value = $bindable(0),
  orientation = 'horizontal',
  size = 'default',
  class: className,
  min = 0,
  max = 100,
  step = 1,
  disabled = false,
  id,
  'aria-label': ariaLabel,
  'aria-valuetext': ariaValueText,
}: {
  ref?: HTMLElement | null
  value?: number
  orientation?: 'horizontal' | 'vertical'
  /** `lg` 是水平离散档位滑杆：滑块位于粗轨道内，并在每个步进位置显示刻度点。 */
  size?: 'default' | 'lg'
  class?: string
  min?: number
  max?: number
  step?: number
  disabled?: boolean
  id?: string
  'aria-label'?: string
  'aria-valuetext'?: string
} = $props()
const large = $derived(size === 'lg')
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
  orientation={large ? 'horizontal' : orientation}
  data-slot="slider"
  data-size={size}
  class={cn(
    'relative flex w-full touch-none items-center select-none data-disabled:opacity-50 data-[orientation=vertical]:h-full data-[orientation=vertical]:min-h-40 data-[orientation=vertical]:w-auto data-[orientation=vertical]:flex-col',
    large && 'h-10',
    className,
  )}>
  {#snippet children({ thumbItems, tickItems })}
    <span
      data-slot="slider-track"
      data-orientation={orientation}
      class={cn(
        'relative grow overflow-hidden rounded-full',
        large
          ? 'h-10 w-full bg-muted dark:bg-input/60'
          : 'bg-input data-[orientation=horizontal]:h-1 data-[orientation=horizontal]:w-full data-[orientation=vertical]:h-full data-[orientation=vertical]:w-1',
      )}>
      <SliderPrimitive.Range
        data-slot="slider-range"
        class="absolute bg-primary select-none data-[orientation=horizontal]:h-full data-[orientation=vertical]:w-full" />
    </span>
    {#if large}
      {#each tickItems as tick (tick.index)}
        <SliderPrimitive.Tick
          index={tick.index}
          data-slot="slider-tick"
          class="pointer-events-none top-[calc(50%-3px)] size-1.5 rounded-full bg-muted-foreground/50 data-bounded:bg-primary-foreground/60" />
      {/each}
    {/if}
    {#each thumbItems as thumb (thumb.index)}
      <SliderPrimitive.Thumb
        data-slot="slider-thumb"
        index={thumb.index}
        aria-label={ariaLabel}
        aria-valuetext={ariaValueText}
        class={cn(
          'relative block shrink-0 rounded-full ring-ring/50 transition-[color,box-shadow] select-none focus-visible:outline-hidden disabled:pointer-events-none disabled:opacity-50',
          large
            ? 'size-8 bg-background shadow-sm ring-offset-0 hover:ring-3 focus-visible:ring-3 active:ring-3 dark:bg-foreground'
            : 'size-3 border border-ring bg-background after:absolute after:-inset-[0.875rem] hover:ring-3 focus-visible:ring-3 active:ring-3',
        )} />
    {/each}
  {/snippet}
</SliderPrimitive.Root>

<script lang="ts" module>
export interface SegmentedControlOption<T extends string = string> {
  value: T
  label: string
  disabled?: boolean
}

export interface SegmentedControlProps<T extends string = string> {
  options: readonly SegmentedControlOption<T>[]
  value: T
  onValueChange: (value: T) => void
  id?: string
  ariaLabel?: string
  ariaLabelledby?: string
  ariaDescribedby?: string
  disabled?: boolean
  busy?: boolean
  class?: string
  ref?: HTMLDivElement | null
}
</script>

<script lang="ts" generics="T extends string">
import { tabsListVariants, tabsTriggerVariants } from '#lib/components/ui/tabs/variants.js'
import { cn } from '#lib/utils.js'

let {
  options,
  value,
  onValueChange,
  id,
  ariaLabel,
  ariaLabelledby,
  ariaDescribedby,
  disabled = false,
  busy = false,
  class: className,
  ref = $bindable(null),
}: SegmentedControlProps<T> = $props()

const tabStopIndex = $derived.by(() => {
  if (disabled) return -1
  const selectedIndex = options.findIndex((option) => option.value === value && !option.disabled)
  if (selectedIndex !== -1) return selectedIndex
  return options.findIndex((option) => !option.disabled)
})

function findEnabledIndex(startIndex: number, step: 1 | -1): number {
  const count = options.length
  for (let offset = 1; offset <= count; offset += 1) {
    const candidate = (startIndex + step * offset + count) % count
    if (!options[candidate]?.disabled) return candidate
  }
  return -1
}

function findBoundaryIndex(fromEnd: boolean): number {
  if (!fromEnd) return options.findIndex((option) => !option.disabled)
  for (let index = options.length - 1; index >= 0; index -= 1) {
    if (!options[index]?.disabled) return index
  }
  return -1
}

function handleSelect(option: SegmentedControlOption<T>): void {
  if (disabled || option.disabled || option.value === value) return
  onValueChange(option.value)
}

function handleKeydown(event: KeyboardEvent, currentIndex: number): void {
  if (disabled) return
  let nextIndex: number
  switch (event.key) {
    case 'ArrowRight':
    case 'ArrowDown':
      nextIndex = findEnabledIndex(currentIndex, 1)
      break
    case 'ArrowLeft':
    case 'ArrowUp':
      nextIndex = findEnabledIndex(currentIndex, -1)
      break
    case 'Home':
      nextIndex = findBoundaryIndex(false)
      break
    case 'End':
      nextIndex = findBoundaryIndex(true)
      break
    default:
      return
  }
  if (nextIndex === -1) return
  const nextOption = options[nextIndex]
  if (!nextOption) return
  event.preventDefault()
  ref?.querySelectorAll<HTMLButtonElement>('[role="radio"]')[nextIndex]?.focus()
  if (nextOption.value !== value) onValueChange(nextOption.value)
}
</script>

<div
  bind:this={ref}
  {id}
  data-slot="segmented-control"
  data-variant="default"
  role="radiogroup"
  class={cn(
    tabsListVariants({ variant: 'default' }),
    'min-h-8 border border-border/60 bg-muted/75 p-0.5 dark:bg-background/70',
    className,
  )}
  aria-label={ariaLabel}
  aria-labelledby={ariaLabelledby}
  aria-describedby={ariaDescribedby}
  aria-busy={busy || undefined}
  aria-disabled={disabled || undefined}>
  {#each options as option, index (option.value)}
    {@const selected = value === option.value}
    {@const optionDisabled = disabled || Boolean(option.disabled)}
    <button
      type="button"
      role="radio"
      data-slot="segmented-control-item"
      value={option.value}
      class={cn(
        tabsTriggerVariants(),
        'min-h-6.5 px-2.5 py-0 data-active:border-border/70 data-active:bg-card data-active:text-foreground data-active:shadow-xs group-data-[variant=default]/tabs-list:data-active:shadow-xs dark:data-active:border-border/90 dark:data-active:bg-card dark:data-active:shadow-xs',
      )}
      tabindex={index === tabStopIndex ? 0 : -1}
      aria-checked={selected}
      data-state={selected ? 'active' : 'inactive'}
      data-active={selected ? '' : undefined}
      disabled={optionDisabled}
      onclick={() => handleSelect(option)}
      onkeydown={(event) => handleKeydown(event, index)}>
      {option.label}
    </button>
  {/each}
</div>

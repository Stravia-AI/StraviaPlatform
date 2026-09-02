<script lang="ts">
import XIcon from '@lucide/svelte/icons/x'

import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'

interface CatalogModel {
  id: string
  name: string
}

interface Props {
  id: string
  value: string
  models: CatalogModel[]
  placeholder: string
  emptyText: string
  ariaLabel: string
  clearAriaLabel: string
  onInput: (value: string) => void
  onSelect: (model: CatalogModel) => void
  onClear: () => void
}

let { id, value, models, placeholder, emptyText, ariaLabel, clearAriaLabel, onInput, onSelect, onClear }: Props =
  $props()
let open = $state(false)
let highlightedIndex = $state(0)

const filteredModels = $derived.by(() => {
  const query = value.trim().toLocaleLowerCase()
  if (!query) return models
  return models.filter((model) => `${model.name} ${model.id}`.toLocaleLowerCase().includes(query))
})

function changeValue(nextValue: string): void {
  highlightedIndex = 0
  open = true
  onInput(nextValue)
}

function choose(model: CatalogModel): void {
  open = false
  onSelect(model)
}

function handleKeydown(event: KeyboardEvent): void {
  if (event.key === 'ArrowDown') {
    event.preventDefault()
    open = true
    highlightedIndex = Math.min(highlightedIndex + 1, Math.max(filteredModels.length - 1, 0))
    return
  }
  if (event.key === 'ArrowUp') {
    event.preventDefault()
    open = true
    highlightedIndex = Math.max(highlightedIndex - 1, 0)
    return
  }
  if (event.key === 'Enter') {
    event.preventDefault()
    event.stopPropagation()
    if (open && filteredModels[highlightedIndex]) choose(filteredModels[highlightedIndex])
    else open = false
    return
  }
  if (event.key === 'Escape') open = false
}

function handleFocusout(event: FocusEvent): void {
  if (!(event.currentTarget as HTMLDivElement).contains(event.relatedTarget as Node | null)) open = false
}
</script>

<div class="relative" onfocusout={handleFocusout}>
  <Input
    {id}
    class="pr-10 font-technical"
    {value}
    {placeholder}
    role="combobox"
    aria-label={ariaLabel}
    aria-autocomplete="list"
    aria-expanded={open}
    aria-controls={`${id}-options`}
    onfocus={() => (open = true)}
    oninput={(event) => changeValue(event.currentTarget.value)}
    onkeydown={handleKeydown} />
  {#if value}
    <Button
      type="button"
      variant="ghost"
      size="icon"
      class="absolute top-0 right-0 size-10"
      aria-label={clearAriaLabel}
      onclick={() => {
        open = false
        onClear()
      }}><XIcon /></Button>
  {/if}
  {#if open}
    <div
      id={`${id}-options`}
      role="listbox"
      class="absolute z-50 mt-1 max-h-72 w-full overflow-auto rounded-md border bg-popover p-1 text-popover-foreground shadow-md">
      {#if filteredModels.length === 0}
        <p class="px-2 py-6 text-center text-sm text-muted-foreground">{emptyText}</p>
      {:else}
        {#each filteredModels as model, index (model.id)}
          <button
            type="button"
            role="option"
            aria-selected={index === highlightedIndex}
            class="flex min-h-10 w-full items-center justify-between gap-3 rounded-sm px-2 py-1.5 text-left text-sm outline-none hover:bg-accent hover:text-accent-foreground aria-selected:bg-accent aria-selected:text-accent-foreground"
            onpointermove={() => (highlightedIndex = index)}
            onmousedown={(event) => event.preventDefault()}
            onclick={() => choose(model)}>
            <span class="truncate">{model.name}</span>
            <span class="truncate font-technical text-xs text-muted-foreground">{model.id}</span>
          </button>
        {/each}
      {/if}
    </div>
  {/if}
</div>

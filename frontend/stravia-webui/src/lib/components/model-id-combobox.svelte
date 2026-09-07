<script lang="ts">
import XIcon from '@lucide/svelte/icons/x'
import { tick } from 'svelte'

import * as Command from '$lib/components/ui/command'
import * as InputGroup from '$lib/components/ui/input-group'
import * as Popover from '$lib/components/ui/popover'
import { localeState } from '$lib/localization.svelte'

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
let highlighted = $state('')
let input = $state<HTMLInputElement | null>(null)
let composing = false

const filteredModels = $derived.by(() => {
  const query = value.trim().toLocaleLowerCase(localeState.current)
  if (!query) return models
  return models.filter((model) => `${model.name} ${model.id}`.toLocaleLowerCase(localeState.current).includes(query))
})
const highlightedModel = $derived(filteredModels.find((model) => model.id === highlighted))

function changeValue(nextValue: string): void {
  open = true
  onInput(nextValue)
}

function choose(model: CatalogModel): void {
  open = false
  onSelect(model)
  void tick().then(() => input?.focus())
}

function handleKeydown(event: KeyboardEvent): void {
  if (event.key === 'Home' || event.key === 'End') {
    event.stopPropagation()
    return
  }
  if (event.key === 'Enter') {
    event.preventDefault()
    event.stopPropagation()
    if (composing || event.isComposing || event.keyCode === 229) return
    if (open && highlightedModel) choose(highlightedModel)
    else open = false
  } else if (event.key === 'Escape') {
    if (composing || event.isComposing) return
    event.preventDefault()
    event.stopPropagation()
    open = false
  } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    if (composing || event.isComposing) {
      event.stopPropagation()
      return
    }
    open = true
  }
}
</script>

<Popover.Root bind:open>
  <Command.Root bind:value={highlighted} shouldFilter={false} vimBindings={false} class="h-auto overflow-visible p-0">
    <InputGroup.Root>
      <InputGroup.Input
        {id}
        bind:ref={input}
        class="font-technical"
        {value}
        {placeholder}
        role="combobox"
        aria-label={ariaLabel}
        aria-autocomplete="list"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? `${id}-options` : undefined}
        aria-activedescendant={open && highlightedModel
          ? `${id}-option-${encodeURIComponent(highlightedModel.id)}`
          : undefined}
        onfocus={() => (open = true)}
        onblur={() => (open = false)}
        oninput={(event) => changeValue(event.currentTarget.value)}
        oncompositionstart={() => (composing = true)}
        oncompositionend={() => (composing = false)}
        onkeydown={handleKeydown} />
      {#if value}
        <InputGroup.Addon align="inline-end">
          <InputGroup.Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label={clearAriaLabel}
            onclick={() => {
              onClear()
              input?.focus()
              open = false
            }}><XIcon /></InputGroup.Button>
        </InputGroup.Addon>
      {/if}
    </InputGroup.Root>
    <Popover.Content
      customAnchor={input}
      portalProps={{ disabled: true }}
      align="start"
      class="w-(--bits-popover-anchor-width) p-0"
      trapFocus={false}
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onInteractOutside={(event) => {
        if (event.target === input) event.preventDefault()
      }}
      onpointerdown={(event) => event.preventDefault()}>
      <Command.List id={`${id}-options`} aria-label={ariaLabel}>
        {#if filteredModels.length === 0}
          <Command.Empty>{emptyText}</Command.Empty>
        {:else}
          <Command.Group>
            {#each filteredModels as model (model.id)}
              <Command.Item
                id={`${id}-option-${encodeURIComponent(model.id)}`}
                value={model.id}
                onSelect={() => choose(model)}>
                <span class="truncate">{model.name}</span>
                <span class="truncate font-technical text-xs text-muted-foreground">{model.id}</span>
              </Command.Item>
            {/each}
          </Command.Group>
        {/if}
      </Command.List>
    </Popover.Content>
  </Command.Root>
</Popover.Root>

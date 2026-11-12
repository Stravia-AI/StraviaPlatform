<script lang="ts">
import XIcon from '@lucide/svelte/icons/x'
import { tick } from 'svelte'

import * as Command from '#lib/components/ui/command/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'
import { localeState } from '#lib/localization.svelte.js'
import type { SearchComboboxOption } from '#lib/search-combobox-option.js'
import { cn, inputValue } from '#lib/utils.js'

interface Props {
  id: string
  value: string
  options: SearchComboboxOption[]
  placeholder: string
  emptyText: string
  ariaLabel?: string
  /** 提供时显示清除按钮；清除写回空字符串。 */
  clearLabel?: string
  /** 允许输入列表外的任意值（如模型 ID）；否则只能从列表选择，未选中就离开时还原为当前值。 */
  freeText?: boolean
  disabled?: boolean
  class?: string
  /** 自由输入时每次键入都会调用（不带 option）；选择列表项时带上该项。 */
  onValueChange: (value: string, option?: SearchComboboxOption) => void
  onClear?: () => void
}

let {
  id,
  value,
  options,
  placeholder,
  emptyText,
  ariaLabel,
  clearLabel,
  freeText = false,
  disabled = false,
  class: className,
  onValueChange,
  onClear,
}: Props = $props()
let open = $state(false)
let highlighted = $state('')
// 只选模式下正在键入的搜索词；null 表示输入框显示当前选中项。
let query = $state<string | null>(null)
let input = $state<HTMLInputElement | null>(null)
let group = $state<HTMLElement | null>(null)
let composing = false
// 鼠标点入时 focus 里的全选会被随后的 mouseup 取消，需要拦下这一次。
let keepSelectionOnMouseUp = false

const selected = $derived(options.find((option) => option.value === value))
const editing = $derived(!freeText && query !== null)
const text = $derived(freeText ? value : (query ?? selected?.label ?? value))
// 已保存的值可能已不在候选里（如已删除的供应商），此时原样显示 ID。
const technicalText = $derived(
  freeText || (!editing && Boolean(value) && (selected ? Boolean(selected.technical) : true)),
)
const filteredOptions = $derived.by(() => {
  const needle = (freeText ? value : (query ?? '')).trim().toLocaleLowerCase(localeState.current)
  if (!needle) return options
  return options.filter((option) =>
    [option.label, option.value, option.detail ?? '', ...(option.keywords ?? [])]
      .join(' ')
      .toLocaleLowerCase(localeState.current)
      .includes(needle),
  )
})
const highlightedOption = $derived(filteredOptions.find((option) => option.value === highlighted && !option.disabled))

function optionId(option: SearchComboboxOption): string {
  return `${id}-option-${encodeURIComponent(option.value)}`
}

function openList(): void {
  if (open) return
  open = true
  if (!freeText && selected) highlighted = selected.value
}

function closeList(): void {
  open = false
  query = null
}

function changeText(nextText: string): void {
  openList()
  if (freeText) onValueChange(nextText)
  else query = nextText
}

function choose(option: SearchComboboxOption): void {
  closeList()
  onValueChange(option.value, option)
  void tick().then(() => input?.focus())
}

function clear(): void {
  onValueChange('')
  onClear?.()
  input?.focus()
  closeList()
}

// 聚焦本身不展开列表：Dialog 自动聚焦或选中后回焦时不应弹出候选；点击、键入或方向键才展开。
function handleFocus(): void {
  if (!freeText) input?.select()
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
    if (open && highlightedOption) choose(highlightedOption)
    else closeList()
  } else if (event.key === 'Escape') {
    // 列表已关且没有未提交的搜索词时交给外层（如关闭 Sheet / Dialog）。
    if (composing || event.isComposing || (!open && query === null)) return
    event.preventDefault()
    event.stopPropagation()
    closeList()
  } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    if (composing || event.isComposing) {
      event.stopPropagation()
      return
    }
    openList()
  }
}
</script>

<Popover.Root bind:open>
  <Command.Root
    bind:value={highlighted}
    shouldFilter={false}
    vimBindings={false}
    class={cn('h-auto overflow-visible p-0', className)}>
    <InputGroup.Root bind:ref={group}>
      <InputGroup.Input
        {id}
        bind:ref={input}
        class={[technicalText && 'font-technical']}
        value={text}
        {placeholder}
        {disabled}
        role="combobox"
        aria-label={ariaLabel}
        aria-autocomplete="list"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? `${id}-options` : undefined}
        aria-activedescendant={open && highlightedOption ? optionId(highlightedOption) : undefined}
        onfocus={handleFocus}
        onpointerdown={() => (keepSelectionOnMouseUp = document.activeElement !== input)}
        onmouseup={(event: MouseEvent) => {
          if (keepSelectionOnMouseUp) event.preventDefault()
          keepSelectionOnMouseUp = false
        }}
        onclick={openList}
        onblur={closeList}
        oninput={(event: Event) => changeText(inputValue(event))}
        oncompositionstart={() => (composing = true)}
        oncompositionend={() => (composing = false)}
        onkeydown={handleKeydown} />
      {#if clearLabel && value && !disabled}
        <InputGroup.Addon align="inline-end">
          <InputGroup.Button type="button" variant="ghost" size="icon-sm" aria-label={clearLabel} onclick={clear}
            ><XIcon /></InputGroup.Button>
        </InputGroup.Addon>
      {/if}
    </InputGroup.Root>
    <Popover.Content
      customAnchor={group}
      portalProps={{ disabled: true }}
      collisionPadding={8}
      align="start"
      class="w-(--bits-popover-anchor-width) p-0"
      trapFocus={false}
      onOpenAutoFocus={(event: Event) => event.preventDefault()}
      onCloseAutoFocus={(event: Event) => event.preventDefault()}
      onInteractOutside={(event: Event) => {
        if (event.target === input) event.preventDefault()
      }}
      onmousedown={(event: MouseEvent) => event.preventDefault()}>
      <!-- 候选列表随浮层可用空间收缩并内部滚动，窄视口或矮对话框里也不会被边缘截断。 -->
      <Command.List
        id={`${id}-options`}
        aria-label={ariaLabel}
        class="max-h-[min(18rem,var(--bits-popover-content-available-height))]">
        {#if filteredOptions.length === 0}
          <Command.Empty>{emptyText}</Command.Empty>
        {:else}
          <Command.Group>
            {#each filteredOptions as option (option.value)}
              <Command.Item
                id={optionId(option)}
                value={option.value}
                disabled={option.disabled}
                data-checked={!freeText && option.value === value}
                onSelect={() => choose(option)}>
                <!-- 同系列变体只在名称/ID 末尾不同，截断会让候选无法区分，所以上下堆叠并完整换行。 -->
                <span class="flex min-w-0 flex-1 flex-col gap-0.5">
                  <span class={['wrap-anywhere', option.technical && 'font-technical']}>{option.label}</span>
                  {#if option.detail}
                    <span class="font-technical text-xs wrap-anywhere text-muted-foreground">{option.detail}</span>
                  {/if}
                </span>
              </Command.Item>
            {/each}
          </Command.Group>
        {/if}
      </Command.List>
    </Popover.Content>
  </Command.Root>
</Popover.Root>

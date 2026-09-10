<script lang="ts">
import MarkdownContent from '$lib/components/markdown-content.svelte'
import { tick } from 'svelte'
import * as Tooltip from '$lib/components/ui/tooltip'

let {
  text,
  label,
  emptyLabel,
  contextLabel = '',
  tail = false,
}: { text: string | null; label: string; emptyLabel: string; contextLabel?: string; tail?: boolean } = $props()
let open = $state(false)
let pinned = $state(false)
let trigger = $state<HTMLButtonElement | null>(null)
let content = $state<HTMLDivElement | null>(null)
const contentId = $props.id()

function close() {
  pinned = false
  open = false
}

function dismissPinnedOnOutside(element: HTMLDivElement) {
  if (!pinned) return
  const dismiss = (event: PointerEvent) => {
    const target = event.target as Node
    if (!element.contains(target) && !trigger?.contains(target)) close()
  }
  // 点按固定的预览按 pointerdown 关闭，不依赖 hover Tooltip 的合成 click 时序。
  window.addEventListener('pointerdown', dismiss, true)
  return () => window.removeEventListener('pointerdown', dismiss, true)
}

async function handleKeydown(event: KeyboardEvent) {
  event.stopPropagation()
  if (event.key === 'Escape') {
    event.preventDefault()
    close()
  } else if (event.key === 'ArrowDown') {
    event.preventDefault()
    pinned = true
    open = true
    await tick()
    content?.focus()
  }
}
</script>

{#snippet markdown()}
  <div class="preview-markdown" style:--markdown-first-margin={contextLabel ? '0.35rem' : '0'}>
    {#if contextLabel}<strong>{contextLabel}</strong>{/if}
    {#if text}
      <MarkdownContent {text} />
    {:else if !contextLabel}
      {emptyLabel}
    {/if}
  </div>
{/snippet}

<Tooltip.Root
  bind:open={
    () => open,
    (value) => {
      if (!pinned) open = value
    }
  }
  delayDuration={200}
  disableHoverableContent={false}
  disableCloseOnTriggerClick>
  <Tooltip.Trigger
    bind:ref={trigger}
    type="button"
    aria-label={label}
    class="preview-trigger nodrag nopan nowheel"
    onpointerdown={(event) => event.stopPropagation()}
    onkeydown={handleKeydown}
    onkeyup={(event) => event.stopPropagation()}
    onclick={(event) => {
      event.preventDefault()
      event.stopPropagation()
      pinned = true
      open = true
    }}>
    {#snippet child({ props })}
      <button {...props} aria-describedby={open ? contentId : undefined}>
        <div class={['preview-viewport', tail ? 'tail-preview' : 'input-preview', !text && 'text-muted-foreground']}>
          {@render markdown()}
        </div>
      </button>
    {/snippet}
  </Tooltip.Trigger>
  <Tooltip.Content
    role="tooltip"
    portalProps={{ to: trigger?.closest<HTMLElement>('.observation-workspace') ?? undefined }}
    aria-label={label}
    side="top"
    sideOffset={8}
    collisionPadding={16}
    strategy="fixed"
    updatePositionStrategy="always"
    class="max-w-[min(32rem,calc(100vw-2rem))]"
    onInteractOutside={(event) => {
      if (trigger?.contains(event.target as Node)) event.preventDefault()
      else close()
    }}
    onFocusOutside={(event) => {
      if (!trigger?.contains(event.target as Node)) close()
    }}
    onEscapeKeydown={(event) => {
      event.preventDefault()
      event.stopPropagation()
      close()
    }}>
    <div
      id={contentId}
      bind:this={content}
      {@attach dismissPinnedOnOutside}
      class="expanded-preview nowheel"
      role="region"
      aria-label={label}
      tabindex="-1">
      <p class="preview-label">{label}</p>
      {@render markdown()}
    </div>
  </Tooltip.Content>
</Tooltip.Root>

<style>
:global(.preview-trigger) {
  display: block;
  width: 100%;
  min-height: 40px;
  flex: none;
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 0.35rem 0.45rem;
  text-align: left;
  background: transparent;
  cursor: pointer;
  transition: border-color 140ms ease;
}
:global(.preview-trigger:hover) {
  border-color: var(--input);
}
.preview-viewport {
  display: flex;
  flex-direction: column;
  overflow: hidden;
  font-size: 0.75rem;
  line-height: 1rem;
  overflow-wrap: anywhere;
}
.input-preview {
  height: 2rem;
}
.tail-preview {
  height: 3rem;
  justify-content: flex-end;
}
.expanded-preview {
  width: min(29rem, calc(100vw - 4rem));
  max-height: min(24rem, calc(100dvh - 4rem), var(--bits-tooltip-content-available-height));
  overflow: auto;
  overscroll-behavior: contain;
  overflow-wrap: anywhere;
  line-height: 1.5;
}
.preview-label {
  margin-bottom: 0.5rem;
  font-weight: 600;
}
.preview-markdown {
  flex: none;
  min-width: 0;
  --markdown-border: var(--border);
}
</style>

<script lang="ts">
import MarkdownContent from '#lib/components/markdown-content.svelte'
import StreamingMarkdown from '#lib/components/streaming-markdown.svelte'
import { tick } from 'svelte'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'
import * as m from '#lib/paraglide/messages.js'
import { observationContents } from '#lib/observation-contents-client.js'
import type { ObservationContentSummary } from '#lib/types.js'

/** 卡片视口只容纳两到三行；输入取开头、输出取结尾，各自读取到足够的非空文本为止。 */
const INPUT_PREVIEW_CHARS = 600
const OUTPUT_PREVIEW_CHARS = 4096

let {
  summary,
  separator,
  liveText,
  label,
  emptyLabel,
  contextLabel = '',
  tail = false,
  active = false,
  textStart = 0,
  snapshotKey = 0,
}: {
  /** 列表摘要：有序正文引用。卡片与 Tooltip 都按引用经内容端点读取。 */
  summary: ObservationContentSummary
  /** 引用正文的拼接分隔符，与服务端 `input_text` / `output_text` 一致。 */
  separator: string
  /** 选中卡片的实时尾窗（含尚未提交的正文）；提供时替代按引用读取的卡片预览。 */
  liveText?: string
  label: string
  emptyLabel: string
  contextLabel?: string
  tail?: boolean
  active?: boolean
  textStart?: number
  snapshotKey?: number
} = $props()
const refsKey = $derived(summary.refs.map((ref) => ref.content_id).join(','))
const empty = $derived(summary.refs.length === 0)
let previewText = $state<string | null>(null)
let previewFailed = $state(false)
let fullText = $state<{ key: string; text: string } | null>(null)
let fullFailed = $state(false)
// 流式进行中的 Tooltip 沿用实时尾窗：摘要引用尚不含未提交正文。
const liveTooltip = $derived(active && liveText !== undefined)
const cardText = $derived(liveText ?? (empty ? '' : previewText))
const cardStatus = $derived(cardText !== null ? 'ready' : previewFailed ? 'failed' : 'loading')
const hasVisibleText = $derived(/\S/.test(cardText ?? ''))
let open = $state(false)
let pinned = $state(false)
let trigger = $state<HTMLButtonElement | null>(null)
let content = $state<HTMLDivElement | null>(null)
const contentId = $props.id()

$effect(() => {
  void refsKey
  if (liveText !== undefined || empty) return
  const refs = summary.refs
  let current = true
  previewFailed = false
  observationContents
    .preview(refs, {
      separator,
      edge: tail ? 'tail' : 'head',
      limit: tail ? OUTPUT_PREVIEW_CHARS : INPUT_PREVIEW_CHARS,
    })
    .then((text) => {
      if (current) previewText = text
    })
    .catch(() => {
      if (current) previewFailed = true
    })
  return () => {
    current = false
  }
})

$effect(() => {
  if (!open || liveTooltip || empty) return
  const key = refsKey
  const refs = summary.refs
  let current = true
  fullFailed = false
  observationContents
    .read(refs, separator)
    .then((text) => {
      if (current) fullText = { key, text }
    })
    .catch(() => {
      if (current) fullFailed = true
    })
  return () => {
    current = false
  }
})

const tooltipText = $derived(
  liveTooltip ? liveText! : empty ? '' : fullText?.key === refsKey ? fullText.text : null,
)
const tooltipStatus = $derived(tooltipText !== null ? 'ready' : fullFailed ? 'failed' : 'loading')

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

{#snippet markdown(text: string | null, status: 'ready' | 'loading' | 'failed', animate: boolean, streamed: boolean)}
  <div class="preview-markdown" style:--markdown-first-margin={contextLabel ? '0.35rem' : '0'}>
    {#if contextLabel}<strong>{contextLabel}</strong>{/if}
    {#if text !== null && /\S/.test(text)}
      {#if streamed}
        <StreamingMarkdown {text} active={animate} streaming={active} {textStart} {snapshotKey} />
      {:else}
        <MarkdownContent {text} streaming={active} />
      {/if}
    {:else if status === 'loading'}
      {m.observation_loading_details()}
    {:else if status === 'failed'}
      {m.observation_content_load_failed()}
    {:else if !contextLabel}
      {emptyLabel}
    {/if}
  </div>
{/snippet}

<Tooltip.Root
  bind:open={
    () => open,
    (value: boolean) => {
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
    onpointerdown={(event: PointerEvent) => event.stopPropagation()}
    onkeydown={handleKeydown}
    onkeyup={(event: KeyboardEvent) => event.stopPropagation()}
    onclick={(event: MouseEvent) => {
      event.preventDefault()
      event.stopPropagation()
      pinned = true
      open = true
    }}>
    {#snippet child({ props })}
      <button {...props} aria-describedby={open ? contentId : undefined}>
        <div
          class={[
            'preview-viewport',
            tail ? 'tail-preview' : 'input-preview',
            (cardStatus !== 'ready' || !hasVisibleText) && 'text-muted-foreground',
          ]}>
          {@render markdown(cardText, cardStatus, active, tail)}
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
    onInteractOutside={(event: Event) => {
      if (trigger?.contains(event.target as Node)) event.preventDefault()
      else close()
    }}
    onFocusOutside={(event: Event) => {
      if (!trigger?.contains(event.target as Node)) close()
    }}
    onEscapeKeydown={(event: KeyboardEvent) => {
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
      {@render markdown(tooltipText, tooltipStatus, active && open, liveTooltip && tail)}
    </div>
  </Tooltip.Content>
</Tooltip.Root>

<style>
:global(.preview-trigger) {
  display: block;
  width: 100%;

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

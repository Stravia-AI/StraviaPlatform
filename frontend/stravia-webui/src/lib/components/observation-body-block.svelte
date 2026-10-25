<script lang="ts">
import { untrack, type Snippet } from 'svelte'
import { getObservationBodyViewport } from '#lib/observation-body-viewport.js'

let { source, children }: { source: string; children: Snippet } = $props()
const viewport = getObservationBodyViewport()
let near = $state(false)
let retained = $state(false)
let height = $state<number | null>(null)
let width = $state(0)
let body: HTMLElement | undefined
let resizeFromAbove: boolean | null = null
const mounted = $derived(near || retained)
// This is only the initial scrollbar estimate; visited blocks retain their measured border-box height.
const estimatedHeight = $derived.by(() => {
  const columns = Math.max(20, Math.floor((width || viewport.root()?.clientWidth || 640) / 7))
  let lines = 0
  let start = 0
  for (;;) {
    const newline = source.indexOf('\n', start)
    const end = newline < 0 ? source.length : newline
    lines += Math.max(1, Math.ceil((end - start) / columns))
    if (newline < 0) break
    start = newline + 1
  }
  return Math.max(1, lines * 22.12)
})

$effect.pre(() => {
  void source
  untrack(() => {
    const root = viewport.root()
    if (mounted && body && root && root.clientHeight > 0) {
      resizeFromAbove = body.getBoundingClientRect().bottom <= root.getBoundingClientRect().top
    }
  })
})

function observe(element: HTMLElement): () => void {
  return untrack(() => {
    body = element
    let inRange = false
    let previousHeight = height ?? estimatedHeight
    const pinned = () => {
      const selection = document.getSelection()
      return (
        element.contains(document.activeElement) ||
        element.querySelector('[data-markdown-dialog-open]') !== null ||
        Boolean(
          selection &&
          !selection.isCollapsed &&
          (element.contains(selection.anchorNode) ||
            element.contains(selection.focusNode) ||
            (selection.rangeCount > 0 && selection.getRangeAt(0).intersectsNode(element))),
        )
      )
    }
    const retain = () => {
      retained = !inRange && pinned()
    }
    const afterFocus = () => queueMicrotask(retain)
    const stop = viewport.observe(element, (value) => {
      // IO 批次先读取旧几何，再统一触发 Svelte 更新；不能用其他块已重排后的坐标补偿。
      const root = viewport.root()
      if (root && root.clientHeight > 0) {
        resizeFromAbove = element.getBoundingClientRect().bottom <= root.getBoundingClientRect().top
      }
      inRange = value
      // Never remove a focused control, open image dialog trigger, or the user's selection.
      retain()
      near = value
    })
    const resize = new ResizeObserver(() => {
      const rect = element.getBoundingClientRect()
      if (rect.width === 0) return // Hidden tabs must not erase the last visible measurement.
      width = rect.width
      if (!mounted) {
        previousHeight = rect.height
        return
      }
      const delta = rect.height - previousHeight
      const root = viewport.root()
      // Correct only blocks wholly above the reader. A visible block reflows naturally on resize.
      if (root && delta && (resizeFromAbove ?? rect.bottom <= root.getBoundingClientRect().top)) {
        viewport.resizeAbove(delta)
      }
      resizeFromAbove = null
      previousHeight = rect.height
      height = rect.height
    })
    resize.observe(element)
    document.addEventListener('selectionchange', retain)
    document.addEventListener('focusin', retain)
    element.addEventListener('focusout', afterFocus)
    return () => {
      stop()
      resize.disconnect()
      document.removeEventListener('selectionchange', retain)
      document.removeEventListener('focusin', retain)
      element.removeEventListener('focusout', afterFocus)
      if (body === element) body = undefined
    }
  })
}
</script>

<!-- A placeholder is not a second accessible copy of the body. Full-text mode restores continuous reading. -->
<div
  data-observation-body
  style:height={mounted ? undefined : `${height ?? estimatedHeight}px`}
  aria-hidden={mounted ? undefined : true}
  {@attach observe}>
  {#if mounted}{@render children()}{/if}
</div>

<style>
div {
  /* Contain margins so the retained height includes all of the Markdown block's spacing. */
  display: flow-root;
  min-width: 0;
}
</style>

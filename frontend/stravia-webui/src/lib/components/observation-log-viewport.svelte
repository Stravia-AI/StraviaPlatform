<script lang="ts">
import { tick, untrack, type Snippet } from 'svelte'
import { SvelteMap } from 'svelte/reactivity'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import * as m from '#lib/paraglide/messages.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { setObservationBodyViewport } from '#lib/observation-body-viewport.js'

let {
  label,
  olderCursor,
  olderLoading = false,
  onolder,
  children,
  activityKey,
}: {
  label: string
  olderCursor: number | null
  olderLoading?: boolean
  onolder?: () => Promise<void>
  /** `older` 是更早事件的加载行；调用方把它放在已加载记录与未加载空洞的交界处。 */
  children: Snippet<[older: Snippet]>
  /** Explicit activity signal when body mounts/unmounts are not themselves new content. */
  activityKey?: unknown
} = $props()

// 加载行常驻固定高度：出现、转圈、失败与消失都不改变其上下内容的位置。
const PRELOAD_MARGIN = 80

let viewport: HTMLElement | undefined
let loader: HTMLElement | undefined
let mutation: MutationObserver | undefined
let following = true
let previousTop = 0
let loading = $state(false)
// 读取失败时游标不前进：记住失败的游标，改为显式重试，避免停在视口顶部反复请求。
let failedCursor = $state<number | null>(null)
const failed = $derived(failedCursor !== null && failedCursor === olderCursor)
let hasNewActivity = $state(false)
const bodyTargets = new SvelteMap<HTMLElement, (near: boolean) => void>()
let bodyObserver: IntersectionObserver | undefined
let activityCursor = untrack(() => olderCursor)
setObservationBodyViewport({
  root: () => viewport,
  resizeAbove(delta) {
    if (!viewport) return
    syncScrollPosition()
    if (settlePosition !== null) holdReadingPosition(settlePosition)
    else if (following) scrollToLatest()
    else if (pausedPosition?.anchor?.element.isConnected) holdReadingPosition(pausedPosition)
    else {
      viewport.scrollTop += delta
      previousTop = viewport.scrollTop
      pausedPosition = readingPosition()
    }
    // 高度补偿可能让已相交的加载行重新可读，而不会产生新的 IO 边沿。
    void loadOlder()
  },
  observe(element, update) {
    bodyTargets.set(element, update)
    bodyObserver?.observe(element)
    return () => {
      bodyObserver?.unobserve(element)
      bodyTargets.delete(element)
    }
  },
})

// Source updates, unlike viewport materialization, represent new activity.
$effect(() => {
  const key = activityKey
  const cursor = prependCursor
  untrack(() => {
    const prepended = cursor !== activityCursor
    activityCursor = cursor
    if (key === undefined || prepended || !viewport || settlePosition !== null) return
    if (following) scrollToLatest()
    else hasNewActivity = true
  })
})

function atBottom(element: HTMLElement): boolean {
  return element.scrollHeight - element.clientHeight - element.scrollTop <= 2
}

function scrollToLatest(): void {
  if (!viewport) return
  viewport.scrollTop = viewport.scrollHeight
  previousTop = viewport.scrollTop
}

function returnToLatest(): void {
  following = true
  pausedPosition = null
  hasNewActivity = false
  scrollToLatest()
}

interface ReadingPosition {
  distance: number
  anchor?: { element: HTMLElement; offset: number }
}

// 虚拟正文的高度也会在读者下方变化，不能把所有 scrollHeight 变化都当作补入历史。
let prependPosition: ReadingPosition | null = null
let settlePosition: ReadingPosition | null = null
let pausedPosition: ReadingPosition | null = null
let settleFrame = 0

function readingPosition(): ReadingPosition | null {
  if (!viewport || viewport.clientHeight === 0) return null
  const position: ReadingPosition = { distance: viewport.scrollHeight - viewport.scrollTop }
  const bounds = viewport.getBoundingClientRect()
  for (const element of viewport.querySelectorAll<HTMLElement>(
    '[data-observation-body], [data-activity-id] [data-slot="collapsible-trigger"]',
  )) {
    const rect = element.getBoundingClientRect()
    if (rect.bottom > bounds.top && rect.top < bounds.bottom) {
      position.anchor = { element, offset: rect.top - bounds.top }
      break
    }
  }
  return position
}

function syncScrollPosition(userEvent = false): void {
  if (!viewport || viewport.clientHeight === 0 || viewport.scrollTop === previousTop) return
  const top = viewport.scrollTop
  // 原生滚动可能先于 scroll 事件；重排也会夹紧 scrollTop，但不代表用户暂停跟随。
  if (userEvent && top < previousTop && !atBottom(viewport)) following = false
  else if (top > previousTop && atBottom(viewport)) {
    following = true
    hasNewActivity = false
  }
  previousTop = top
  pausedPosition = following ? null : readingPosition()
  if (settlePosition !== null) settlePosition = pausedPosition
}

function holdReadingPosition(position: ReadingPosition): void {
  if (!viewport || viewport.clientHeight === 0) return
  if (following) scrollToLatest()
  else {
    if (position.anchor?.element.isConnected) {
      viewport.scrollTop +=
        position.anchor.element.getBoundingClientRect().top -
        viewport.getBoundingClientRect().top -
        position.anchor.offset
    } else viewport.scrollTop = viewport.scrollHeight - position.distance
    previousTop = viewport.scrollTop
  }
}

// 父组件每次实时更新都会换新详情对象；只在游标值真正变化（补入历史）时触发恢复。
const prependCursor = $derived(olderCursor)

$effect.pre(() => {
  void prependCursor
  untrack(() => {
    syncScrollPosition()
    prependPosition = readingPosition()
  })
})
$effect(() => {
  void prependCursor
  untrack(() => {
    // 补入的历史不是新活动，不能触发跟随或「有新活动」提示。
    mutation?.takeRecords()
    const position = prependPosition
    prependPosition = null
    if (position === null) return
    if (!following) pausedPosition = position
    settlePosition = position
    holdReadingPosition(position)
    cancelAnimationFrame(settleFrame)
    settleFrame = requestAnimationFrame(() => {
      settlePosition = null
    })
  })
})

function loaderInView(): boolean {
  if (!viewport || !loader || viewport.clientHeight === 0) return false
  const view = viewport.getBoundingClientRect()
  const rect = loader.getBoundingClientRect()
  return rect.bottom >= view.top - PRELOAD_MARGIN && rect.top <= view.bottom
}

async function loadOlder(): Promise<void> {
  if (!onolder || loading || failed || olderLoading || olderCursor === null || !loaderInView()) return
  const cursor = olderCursor
  loading = true
  try {
    await onolder()
  } finally {
    loading = false
  }
  if (olderCursor === cursor) {
    failedCursor = cursor
    return
  }
  await tick()
  void loadOlder()
}

function retryOlder(): void {
  failedCursor = null
  void loadOlder()
}

function observeLoader(element: HTMLElement): () => void {
  return untrack(() => {
    const root = element.closest('[data-log-viewport]')
    if (!(root instanceof HTMLElement)) return () => {}
    loader = element
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) void loadOlder()
      },
      { root, rootMargin: `${PRELOAD_MARGIN}px 0px 0px 0px` },
    )
    observer.observe(element)
    return () => {
      observer.disconnect()
      if (loader === element) loader = undefined
    }
  })
}

function ownActivity(record: MutationRecord): boolean {
  const target = record.target instanceof Element ? record.target : record.target.parentElement
  if (target?.closest('[data-older-loader]')) return true
  // Body content can mount, release or resize without receiving any new source data.
  if (target?.closest('[data-observation-markdown]')) return true
  if (record.type !== 'childList') return false
  // 用户展开已有详情并不是收到新内容；仍观察详情内部的实时文本变化。
  if (target?.getAttribute('data-slot') === 'collapsible') return true
  return [...record.addedNodes, ...record.removedNodes].every(
    (node) => node instanceof Element && node.matches('[data-older-loader]'),
  )
}

function follow(element: HTMLElement): () => void {
  return untrack(() => {
    viewport = element
    const content = element.firstElementChild!
    // Tab 隐藏时视口尺寸归零、滚动位置丢失；重新显示时按隐藏前到底部的距离还原阅读位置。
    let visibleHeight = element.clientHeight
    let distance = 0
    const remember = () => {
      if (element.clientHeight > 0) distance = element.scrollHeight - element.scrollTop
    }
    const onScroll = () => {
      syncScrollPosition(true)
      remember()
    }
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0) following = false
      pausedPosition = null
    }
    const onKey = (event: KeyboardEvent) => {
      if (['ArrowUp', 'PageUp', 'Home'].includes(event.key)) following = false
      else if (event.key === 'End') {
        following = true
        hasNewActivity = false
      }
      if (['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End'].includes(event.key)) pausedPosition = null
    }
    // 展开或收起详情是阅读动作：停止跟随，避免展开的内容被自动滚走。
    const onInspect = (event: MouseEvent) => {
      if (event.target instanceof Element && event.target.closest('[data-slot="collapsible-trigger"]')) {
        following = false
        pausedPosition = readingPosition()
      }
    }
    const resize = new ResizeObserver(() => {
      const height = element.clientHeight
      if (visibleHeight > 0) syncScrollPosition()
      if (settlePosition !== null) holdReadingPosition(settlePosition)
      else if (following) scrollToLatest()
      else if (pausedPosition?.anchor?.element.isConnected) holdReadingPosition(pausedPosition)
      else if (visibleHeight === 0 && height > 0) {
        element.scrollTop = element.scrollHeight - distance
        previousTop = element.scrollTop
      }
      visibleHeight = height
      remember()
    })
    mutation = new MutationObserver((records) => {
      if (visibleHeight > 0) syncScrollPosition()
      if (settlePosition !== null) {
        holdReadingPosition(settlePosition)
        return
      }
      if (activityKey !== undefined) {
        // 正文挂载仍需在 scroll 事件前跟随到底部，但不能把挂载本身当成新内容。
        if (following) scrollToLatest()
        else if (pausedPosition?.anchor?.element.isConnected) holdReadingPosition(pausedPosition)
        return
      }
      if (records.every(ownActivity)) return
      if (following) scrollToLatest()
      else hasNewActivity = true
    })
    scrollToLatest()
    bodyObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          bodyTargets.get(entry.target as HTMLElement)?.(entry.isIntersecting)
        }
      },
      { root: element, rootMargin: '600px 0px' },
    )
    for (const target of bodyTargets.keys()) bodyObserver.observe(target)
    resize.observe(content)
    resize.observe(element)
    mutation.observe(content, { childList: true, subtree: true, characterData: true })
    element.addEventListener('scroll', onScroll, { passive: true })
    element.addEventListener('wheel', onWheel, { passive: true })
    element.addEventListener('keydown', onKey)
    element.addEventListener('click', onInspect, { capture: true })
    return () => {
      resize.disconnect()
      bodyObserver?.disconnect()
      bodyObserver = undefined
      mutation?.disconnect()
      mutation = undefined
      element.removeEventListener('scroll', onScroll)
      element.removeEventListener('wheel', onWheel)
      element.removeEventListener('keydown', onKey)
      element.removeEventListener('click', onInspect, { capture: true })
      cancelAnimationFrame(settleFrame)
      settlePosition = null
      if (viewport === element) viewport = undefined
    }
  })
}
</script>

{#snippet older()}
  {#if olderCursor !== null}
    <div class="older-loader" data-older-loader {@attach observeLoader}>
      {#if failed}
        <Button variant="ghost" onclick={retryOlder}>{m.observation_older_retry()}</Button>
      {:else}
        <Spinner aria-label={m.observation_older_loading()} class="text-muted-foreground" />
      {/if}
    </div>
  {/if}
{/snippet}

<div class="log-shell">
  <!-- 独立滚动的记录需要参与 Tab 顺序，支持键盘翻阅。 -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div
    class="log-viewport"
    data-log-viewport
    role="log"
    aria-label={label}
    aria-live="off"
    aria-busy={loading || olderLoading}
    tabindex="0"
    {@attach follow}>
    <div class="log-content">
      {@render children(older)}
    </div>
  </div>
  {#if hasNewActivity}
    <div class="latest-action">
      <Button variant="secondary" onclick={returnToLatest}>
        <ArrowDownIcon data-icon="inline-start" />{m.observation_chat_latest()}
      </Button>
    </div>
  {/if}
</div>

<style>
.log-shell {
  position: relative;
  display: flex;
  height: 100%;
  min-height: 0;
  flex-direction: column;
}
.log-viewport {
  min-height: 0;
  flex: 1;
  overflow-y: auto;
  overscroll-behavior: contain;
  /* 阅读位置由上面的到底部距离显式维护；浏览器自带锚定会与之叠加产生二次位移。 */
  overflow-anchor: none;
  scrollbar-gutter: stable;
}
.older-loader {
  display: flex;
  flex: none;
  align-items: center;
  justify-content: center;
  height: 2.5rem;
}
.latest-action {
  display: flex;
  justify-content: center;
  padding: 0.5rem 1rem max(0.5rem, env(safe-area-inset-bottom));
}
</style>

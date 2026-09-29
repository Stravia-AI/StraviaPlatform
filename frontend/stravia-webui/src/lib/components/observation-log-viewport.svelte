<script lang="ts">
import { tick, untrack, type Snippet } from 'svelte'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import * as m from '$lib/paraglide/messages.js'
import { Button } from '$lib/components/ui/button'
import { Spinner } from '$lib/components/ui/spinner'

let {
  label,
  olderCursor,
  olderLoading = false,
  onolder,
  children,
}: {
  label: string
  olderCursor: number | null
  olderLoading?: boolean
  onolder?: () => Promise<void>
  /** `older` 是更早事件的加载行；调用方把它放在已加载记录与未加载空洞的交界处。 */
  children: Snippet<[older: Snippet]>
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
  hasNewActivity = false
  scrollToLatest()
}

// 更早事件只会插入在读者位置之上：记下到底部的距离，DOM 更新后在绘制前恢复，可见内容保持不动。
// 子组件（如流式 Markdown）会在随后的微任务里继续渲染同一批历史，因此直到下一帧前都按同一距离恢复。
let prependDistance: number | null = null
let settleDistance: number | null = null
let settleFrame = 0

function holdReadingPosition(distance: number): void {
  if (!viewport) return
  if (following) scrollToLatest()
  else {
    viewport.scrollTop = viewport.scrollHeight - distance
    previousTop = viewport.scrollTop
  }
}

// 父组件每次实时更新都会换新详情对象；只在游标值真正变化（补入历史）时触发恢复。
const prependCursor = $derived(olderCursor)

$effect.pre(() => {
  void prependCursor
  untrack(() => {
    prependDistance = viewport && viewport.clientHeight > 0 ? viewport.scrollHeight - viewport.scrollTop : null
  })
})
$effect(() => {
  void prependCursor
  untrack(() => {
    // 补入的历史不是新活动，不能触发跟随或「有新活动」提示。
    mutation?.takeRecords()
    const distance = prependDistance
    prependDistance = null
    if (distance === null) return
    holdReadingPosition(distance)
    settleDistance = distance
    cancelAnimationFrame(settleFrame)
    settleFrame = requestAnimationFrame(() => {
      settleDistance = null
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
      const top = element.scrollTop
      if (top < previousTop || atBottom(element)) {
        following = atBottom(element)
        if (following) hasNewActivity = false
      }
      previousTop = top
      remember()
    }
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0) following = false
    }
    const onKey = (event: KeyboardEvent) => {
      if (['ArrowUp', 'PageUp', 'Home'].includes(event.key)) following = false
    }
    // 展开或收起详情是阅读动作：停止跟随，避免展开的内容被自动滚走。
    const onInspect = (event: MouseEvent) => {
      if (event.target instanceof Element && event.target.closest('[data-slot="collapsible-trigger"]'))
        following = false
    }
    const resize = new ResizeObserver(() => {
      const height = element.clientHeight
      if (settleDistance !== null) holdReadingPosition(settleDistance)
      else if (following) scrollToLatest()
      else if (visibleHeight === 0 && height > 0) {
        element.scrollTop = element.scrollHeight - distance
        previousTop = element.scrollTop
      }
      visibleHeight = height
      remember()
    })
    mutation = new MutationObserver((records) => {
      if (settleDistance !== null) {
        holdReadingPosition(settleDistance)
        return
      }
      if (records.every(ownActivity)) return
      if (following) scrollToLatest()
      else hasNewActivity = true
    })
    scrollToLatest()
    resize.observe(content)
    resize.observe(element)
    mutation.observe(content, { childList: true, subtree: true, characterData: true })
    element.addEventListener('scroll', onScroll, { passive: true })
    element.addEventListener('wheel', onWheel, { passive: true })
    element.addEventListener('keydown', onKey)
    element.addEventListener('click', onInspect, { capture: true })
    return () => {
      resize.disconnect()
      mutation?.disconnect()
      mutation = undefined
      element.removeEventListener('scroll', onScroll)
      element.removeEventListener('wheel', onWheel)
      element.removeEventListener('keydown', onKey)
      element.removeEventListener('click', onInspect, { capture: true })
      cancelAnimationFrame(settleFrame)
      settleDistance = null
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

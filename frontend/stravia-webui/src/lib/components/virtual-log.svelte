<script lang="ts" generics="R">
import { tick, untrack, type Snippet } from 'svelte'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import * as m from '#lib/paraglide/messages.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import VirtualList from '#lib/components/virtual-list.svelte'
import type { VirtualRow } from '#lib/virtual-list.js'

// 加载行占列表顶端的固定高度：转圈与失败不改变其下方内容的位置，消失时随列表的起始内边距一并回收。
const LOADER_HEIGHT = 40
const PRELOAD_MARGIN = 80
// 贴底判定留一点余量，吸收小数像素。
const END_SLACK = 2

let {
  label,
  rows,
  rowKey,
  estimateSize,
  row,
  group,
  segment,
  rowTag,
  olderCursor,
  olderLoading = false,
  onolder,
  activityKey,
  paddingStart = 0,
  paddingEnd = 0,
  class: contentClass,
}: {
  label: string
  rows: readonly R[]
  rowKey: (row: R) => string
  estimateSize: (row: R, index: number) => number
  row: Snippet<[row: R, index: number]>
  group?: (row: R) => string | undefined
  segment?: Snippet<[group: string, rows: VirtualRow<R>[], measured: Snippet<[VirtualRow<R>]>]>
  rowTag?: string
  /** 非空表示列表顶端之前还有未加载的记录，顶端保留加载行。 */
  olderCursor: number | null
  olderLoading?: boolean
  onolder?: () => Promise<void>
  /** 来源数据的修订信号；仅挂载或回收行不是新活动。 */
  activityKey?: unknown
  paddingStart?: number
  paddingEnd?: number
  class?: string
} = $props()

let viewport = $state<HTMLElement>()
let list = $state<{
  scrollToEnd(): void
  scrollToKey(key: string, align?: 'start' | 'center' | 'end' | 'auto'): boolean
  scrollToOffset(offset: number): void
}>()
// 贴底：到达底部或回到最新时为真；读者向上翻阅、展开详情时为假，此后的新内容只提示不跟随。
let pinned = $state(true)
let initialized = false
let previousTop = 0
let loading = $state(false)
// 读取失败时游标不前进：记住失败的游标，改为显式重试，避免停在顶部反复请求。
let failedCursor = $state<number | null>(null)
const failed = $derived(failedCursor !== null && failedCursor === olderCursor)
let hasNewActivity = $state(false)
let activityCursor = untrack(() => olderCursor)

// Tab 隐藏时视口尺寸归零、滚动位置丢失；重新显示时按隐藏前到底部的距离还原阅读位置。
let visibleHeight = 0
let distance = 0

function atEnd(element: HTMLElement): boolean {
  return element.scrollHeight - element.clientHeight - element.scrollTop <= END_SLACK
}

function returnToLatest(): void {
  pinned = true
  hasNewActivity = false
  list?.scrollToEnd()
}

// 首次定位完成前收到的目标：改为停在目标行，而不是默认的最新内容。
let positioned = false
let pendingReveal: string | undefined

/** 滚到指定行并停止贴底，让读者停在该处；列表尚未首次定位时登记目标，定位时采用。 */
export function reveal(key: string): boolean {
  if (!positioned) {
    pendingReveal = key
    return true
  }
  if (!list?.scrollToKey(key, 'center')) return false
  pinned = false
  return true
}

// 首次有内容时停在最新：最新行的高度在测量中收敛，滚动命令会随之重新定位。
// 等一个 tick：列表在滚动容器挂载后才拿到它，命令发给尚未接管容器的虚拟化器会被丢弃。
$effect(() => {
  if (initialized || !viewport || !list || rows.length === 0) return
  initialized = true
  untrack(
    () =>
      void tick().then(() => {
        positioned = true
        const target = pendingReveal
        pendingReveal = undefined
        if (target === undefined || !reveal(target)) list?.scrollToEnd()
        previousTop = viewport?.scrollTop ?? 0
        void loadOlder()
      }),
  )
})

// 贴底时内容变化继续留在底部。虚拟化器自带的跟随只认「最后一行换了」，而新内容常插在末尾的时间行之前。
$effect(() => {
  void rows
  untrack(() => {
    if (!initialized || !pinned || !viewport || viewport.clientHeight === 0) return
    void tick().then(() => {
      if (pinned) list?.scrollToEnd()
    })
  })
})

// 来源更新才是新活动；补入历史或回收行不是。
$effect(() => {
  const key = activityKey
  const cursor = olderCursor
  untrack(() => {
    const prepended = cursor !== activityCursor
    activityCursor = cursor
    if (key === undefined || prepended || !viewport) return
    if (!pinned) hasNewActivity = true
  })
})

// 补入历史后列表可能仍短于视口，继续向前读取；游标变化也要重新判断。
$effect(() => {
  void olderCursor
  untrack(() => void tick().then(loadOlder))
})

function nearTop(): boolean {
  if (!viewport || viewport.clientHeight === 0 || !initialized) return false
  return viewport.scrollTop <= LOADER_HEIGHT + PRELOAD_MARGIN
}

async function loadOlder(): Promise<void> {
  if (!onolder || loading || failed || olderLoading || olderCursor === null || !nearTop()) return
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

function onscroll(): void {
  if (!viewport || viewport.clientHeight === 0) return
  const top = viewport.scrollTop
  if (atEnd(viewport)) {
    pinned = true
    hasNewActivity = false
  } else if (top < previousTop) pinned = false
  previousTop = top
  distance = viewport.scrollHeight - top
  void loadOlder()
}

function onwheel(event: WheelEvent): void {
  if (event.deltaY < 0) pinned = false
}

function onkeydown(event: KeyboardEvent): void {
  if (['ArrowUp', 'PageUp'].includes(event.key)) pinned = false
  // 首尾行的高度未量过：原生跳转会被逐行测量打断，改由虚拟化器逐步收敛到真正的端点。
  else if (event.key === 'Home' && event.target === event.currentTarget) {
    event.preventDefault()
    pinned = false
    list?.scrollToOffset(0)
  } else if (event.key === 'End' && event.target === event.currentTarget) {
    event.preventDefault()
    returnToLatest()
  }
}

// 展开或收起详情是阅读动作：停止贴底，避免展开的内容被自动滚走。
function oninspect(event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('[aria-expanded]')) pinned = false
}

$effect(() => {
  const element = viewport
  if (!element) return
  visibleHeight = element.clientHeight
  const observer = new ResizeObserver(() => {
    const height = element.clientHeight
    if (visibleHeight === 0 && height > 0 && initialized) {
      if (pinned) list?.scrollToEnd()
      else list?.scrollToOffset(Math.max(element.scrollHeight - distance, 0))
    } else if (height > 0) distance = element.scrollHeight - element.scrollTop
    visibleHeight = height
  })
  observer.observe(element)
  return () => observer.disconnect()
})
</script>

{#snippet loader()}
  <div class="older-loader" data-older-loader style:height="{LOADER_HEIGHT}px">
    {#if failed}
      <Button variant="ghost" onclick={retryOlder}>{m.observation_older_retry()}</Button>
    {:else}
      <Spinner aria-label={m.observation_older_loading()} class="text-muted-foreground" />
    {/if}
  </div>
{/snippet}

<div class="log-shell">
  <!-- 独立滚动的记录需要参与 Tab 顺序，支持键盘翻阅。 -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
  <div
    bind:this={viewport}
    class="log-viewport"
    data-log-viewport
    role="log"
    aria-label={label}
    aria-live="off"
    aria-busy={loading || olderLoading}
    tabindex="0"
    {onscroll}
    {onwheel}
    {onkeydown}
    onclickcapture={oninspect}>
    <div class={['log-content', contentClass]}>
      <VirtualList
        bind:this={list}
        items={rows}
        itemKey={rowKey}
        {estimateSize}
        {group}
        {segment}
        {rowTag}
        {row}
        header={olderCursor === null ? undefined : loader}
        scroller={viewport}
        paddingStart={paddingStart + (olderCursor === null ? 0 : LOADER_HEIGHT)}
        {paddingEnd}
        endThreshold={pinned ? END_SLACK : -1} />
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
  /* 阅读位置由虚拟化器按行锚点显式维护；浏览器自带锚定会与之叠加产生二次位移。 */
  overflow-anchor: none;
  scrollbar-gutter: stable;
}
.log-content {
  min-width: 0;
}
.older-loader {
  display: flex;
  align-items: center;
  justify-content: center;
}
.latest-action {
  display: flex;
  justify-content: center;
  padding: 0.5rem 1rem max(0.5rem, env(safe-area-inset-bottom));
}
</style>

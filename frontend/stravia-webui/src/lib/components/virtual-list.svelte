<script lang="ts" generics="T">
import { untrack, type Snippet } from 'svelte'
import {
  Virtualizer,
  defaultRangeExtractor,
  elementScroll,
  observeElementOffset,
  observeElementRect,
  type VirtualItem,
} from '@tanstack/svelte-virtual'
import { virtualSegments, type VirtualRow } from '#lib/virtual-list.js'

let {
  items,
  itemKey,
  estimateSize,
  scroller,
  row,
  header,
  rowTag = 'div',
  group,
  segment,
  overscan = 6,
  paddingStart = 0,
  paddingEnd = 0,
  endThreshold = 2,
  class: className,
}: {
  items: readonly T[]
  /** 稳定的行标识：测量结果、阅读位置锚点和分组段都按它跟随行，下标变化（如补入更早的历史）不影响。 */
  itemKey: (item: T) => string
  /** 仅用于尚未测量的行；挂载后以实测高度为准。 */
  estimateSize: (item: T, index: number) => number
  scroller: HTMLElement | null | undefined
  row: Snippet<[item: T, index: number]>
  /** 铺在起始内边距里的内容（如加载行），高度由调用方保证不超过 `paddingStart`。 */
  header?: Snippet
  /** 行包裹层的标签：行内容是 `<li>` 一类有父级要求的元素时，由调用方在 `segment` 里放匹配的列表并改用 `li`。 */
  rowTag?: string
  /** 返回同一值且相邻的已挂载行共用一个 `segment` 包裹层，包裹层内行按文档流排列。 */
  group?: (item: T) => string | undefined
  segment?: Snippet<[group: string, rows: VirtualRow<T>[], measured: Snippet<[VirtualRow<T>]>]>
  overscan?: number
  paddingStart?: number
  paddingEnd?: number
  /** 视口距末端不超过该距离时，追加与行高变化都保持贴底；负数表示不贴底。 */
  endThreshold?: number
  class?: string
} = $props()

// 与框架适配器的区别：行位置与容器总高在渲染提交之后才交给虚拟化器改写滚动位置。
// 适配器的 `setOptions` 会立即写滚动条，此时容器还是旧高度，补入历史的锚点写入会被浏览器夹到旧的最大值。
let virtualItems = $state.raw<VirtualItem[]>([])
let total = $state(0)
const virtualizer = new Virtualizer<HTMLElement, HTMLElement>({
  count: 0,
  getScrollElement: () => null,
  estimateSize: () => 0,
  observeElementRect,
  observeElementOffset,
  scrollToFn: elementScroll,
  // 默认实现把行高四舍五入成整数，而文档流里的行高是小数；逐行累积的误差会让阅读位置在补入历史后偏移。
  // 挂载时的同步测量不可信：子组件的 effect 晚于动作执行，此刻行内容常还是空的，按 0 高度记录会让上方行的锚点补偿把阅读位置整段拽走。
  // 首次尺寸交给 ResizeObserver（绘制前送达），同步调用沿用已知高度。
  measureElement: (element, entry, instance) =>
    entry
      ? (entry.borderBoxSize?.[0]?.blockSize ?? element.getBoundingClientRect().height)
      : (instance.measurementsCache[Number(element.dataset.index)]?.size ?? 0),
  anchorTo: 'end',
  followOnAppend: true,
  onChange: (instance) => publish(instance),
})

function publish(instance: Virtualizer<HTMLElement, HTMLElement>): void {
  virtualItems = instance.getVirtualItems()
  total = instance.getTotalSize()
}

let container = $state<HTMLElement>()
// 列表在滚动内容里的起点：页面滚动容器里列表上方还有其他内容时，虚拟化器需要这段偏移。
let margin = $state(0)

/** 持有焦点、选区或打开着的图片对话框的行不能被回收，否则操作会被打断。 */
function retainedIndexes(): number[] {
  if (!container) return []
  const active = document.activeElement
  const selection = document.getSelection()
  const range = selection && !selection.isCollapsed && selection.rangeCount > 0 ? selection.getRangeAt(0) : null
  const indexes: number[] = []
  for (const element of container.querySelectorAll<HTMLElement>('[data-virtual-row]')) {
    if (
      element.contains(active) ||
      element.querySelector('[data-keep-mounted]') !== null ||
      range?.intersectsNode(element)
    )
      indexes.push(Number(element.dataset.index))
  }
  return indexes
}

$effect.pre(() => {
  const snapshot = items
  const element = scroller ?? null
  const keyOf = itemKey
  const estimate = estimateSize
  const options = { overscan, paddingStart, paddingEnd, scrollMargin: margin, scrollEndThreshold: endThreshold }
  untrack(() => {
    // 每次换成新的闭包，虚拟化器才会按新的键重建行位置缓存。
    virtualizer.setOptions({
      ...virtualizer.options,
      ...options,
      count: snapshot.length,
      getScrollElement: () => element,
      getItemKey: (index) => {
        const item = snapshot[index]
        return item === undefined ? index : keyOf(item)
      },
      estimateSize: (index) => {
        const item = snapshot[index]
        return item === undefined ? 0 : estimate(item, index)
      },
      rangeExtractor: (range) => {
        const indexes = defaultRangeExtractor(range)
        const retained = retainedIndexes().filter((index) => index < range.count && !indexes.includes(index))
        return retained.length ? [...indexes, ...retained].sort((a, b) => a - b) : indexes
      },
    })
    publish(virtualizer)
  })
})

// 渲染提交后再让虚拟化器接管滚动容器、改写滚动位置。
$effect(() => {
  void virtualItems
  void total
  virtualizer._willUpdate()
})

$effect(() => virtualizer._didMount())

function measureMargin(): void {
  if (!container || !scroller) return
  const next = container.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop
  if (Math.abs(next - margin) >= 0.5) margin = next
}

$effect(() => {
  const element = scroller
  const box = container
  if (!element || !box) return
  const observer = new ResizeObserver(measureMargin)
  observer.observe(element)
  observer.observe(box)
  element.addEventListener('scroll', measureMargin, { passive: true })
  measureMargin()
  return () => {
    observer.disconnect()
    element.removeEventListener('scroll', measureMargin)
  }
})

const segments = $derived(virtualSegments(virtualItems, items, itemKey, group))

/** 行下标随补入历史而变；每次下标变化后都要重新登记。 */
function measure(node: HTMLElement, _index: number) {
  virtualizer.measureElement(node)
  return {
    update() {
      virtualizer.measureElement(node)
    },
  }
}

export function scrollToEnd(): void {
  virtualizer.scrollToEnd()
}

/** 滚到指定行；行不存在返回 `false`。 */
export function scrollToKey(key: string, align: 'start' | 'center' | 'end' | 'auto' = 'center'): boolean {
  const index = items.findIndex((item) => itemKey(item) === key)
  if (index < 0) return false
  virtualizer.scrollToIndex(index, { align })
  return true
}

export function scrollToOffset(offset: number): void {
  virtualizer.scrollToOffset(offset)
}
</script>

{#snippet measured(entry: VirtualRow<T>)}
  <svelte:element this={rowTag} class="virtual-row" data-virtual-row data-index={entry.index} use:measure={entry.index}>
    {@render row(entry.item, entry.index)}
  </svelte:element>
{/snippet}

<div bind:this={container} class={['virtual-list', className]} style:height="{total}px">
  {#if header}<div class="virtual-header">{@render header()}</div>{/if}
  {#each segments as part (part.key)}
    <div class="virtual-segment" style:transform="translateY({part.start - margin}px)">
      {#if part.group !== undefined && segment}
        {@render segment(part.group, part.rows, measured)}
      {:else}
        {#each part.rows as entry (entry.key)}
          {@render measured(entry)}
        {/each}
      {/if}
    </div>
  {/each}
</div>

<style>
.virtual-list,
.virtual-segment {
  /* 减少动效时全局规则会给所有属性套上 0.01ms 过渡，容器高度与行位置会晚一帧生效，补入历史的锚点写入就会被旧高度夹住。 */
  transition-property: none;
}
.virtual-list {
  position: relative;
  width: 100%;
}
.virtual-header {
  position: absolute;
  top: 0;
  left: 0;
  width: 100%;
}
.virtual-segment {
  position: absolute;
  top: 0;
  left: 0;
  display: flex;
  width: 100%;
  flex-direction: column;
}
.virtual-row {
  width: 100%;
  min-width: 0;
}
</style>

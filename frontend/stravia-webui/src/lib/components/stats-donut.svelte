<script lang="ts">
import { onMount, type Snippet } from 'svelte'

import { followTween, CHART_FOCUS_MS } from '#lib/chart-motion.svelte.js'
import { formatPercent } from '#lib/format.js'
import type { DonutSlice } from '#lib/stats-chart.js'

interface Props {
  slices: readonly DonutSlice[]
  /** 每个槽位的填充色，长度即槽位数。 */
  palette: readonly string[]
  ariaLabel: string
  /** 图例标签的附加样式。 */
  labelClass?: string
  /** 环中心的内容；悬停某个分项时收到该分项。 */
  center?: Snippet<[DonutSlice | null]>
}

let { slices, palette, ariaLabel, labelClass, center }: Props = $props()

const TAU = Math.PI * 2
// viewBox 以圆心为原点、半径 50；悬停的扇区向外伸出 EXPAND。
const INNER_RADIUS = 30
const OUTER_RADIUS = 44
const EXPAND = 4
// 扇区之间的间隔与圆角（沿用原先 padAngle / cornerRadius 的观感）。
const GAP = 0.9
const CORNER = 2

let activeKey = $state<string | null>(null)
const activeSlice = $derived(slices.find((slice) => slice.key === activeKey) ?? null)
const total = $derived(slices.reduce((sum, slice) => sum + slice.value, 0))

// 首次挂载时从零展开，之后的数据切换从当前形状缓动到新形状。
let ready = $state(false)
onMount(() => {
  ready = true
})

// 每个槽位 [起点, 终点]（占整圈的比例）。
const fractions = $derived.by(() => {
  const values = palette.map((_, slot) => slices.find((slice) => slice.slot === slot)?.value ?? 0)
  const sum = values.reduce((acc, value) => acc + value, 0)
  let accumulated = 0
  return values.flatMap((value) => {
    const start = sum > 0 ? accumulated / sum : 0
    accumulated += value
    return [start, sum > 0 ? accumulated / sum : 0]
  })
})
const shape = followTween(() => (ready ? fractions : fractions.map(() => 0)))

// 每个槽位的强调程度：0 为常态，1 为被悬停。
const focusTarget = $derived(palette.map((_, slot) => (activeSlice != null && activeSlice.slot === slot ? 1 : 0)))
const focus = followTween(() => focusTarget, CHART_FOCUS_MS)

function point(radius: number, angle: number): string {
  return `${(radius * Math.cos(angle)).toFixed(3)},${(radius * Math.sin(angle)).toFixed(3)}`
}

// 圆角用“内缩后描边”实现：形状向内缩 CORNER，再用同色 2×CORNER 的圆角描边补回。
function sectorPath(start: number, end: number, outer: number): string {
  const inner = INNER_RADIUS + CORNER
  const outside = outer - CORNER
  if (end - start >= 0.9999) {
    const ring = (radius: number, sweep: 0 | 1) =>
      `M${point(radius, -Math.PI / 2)}A${radius},${radius} 0 1 ${sweep} ${point(radius, Math.PI / 2)}A${radius},${radius} 0 1 ${sweep} ${point(radius, -Math.PI / 2)}Z`
    return `${ring(outside, 1)}${ring(inner, 0)}`
  }
  const margin = GAP / 2 + CORNER
  const outerStart = start * TAU - Math.PI / 2 + margin / outside
  const outerEnd = end * TAU - Math.PI / 2 - margin / outside
  const innerStart = start * TAU - Math.PI / 2 + margin / inner
  const innerEnd = end * TAU - Math.PI / 2 - margin / inner
  if (outerEnd <= outerStart || innerEnd <= innerStart) return ''
  const outerLarge = outerEnd - outerStart > Math.PI ? 1 : 0
  const innerLarge = innerEnd - innerStart > Math.PI ? 1 : 0
  return `M${point(outside, outerStart)}A${outside},${outside} 0 ${outerLarge} 1 ${point(outside, outerEnd)}L${point(inner, innerEnd)}A${inner},${inner} 0 ${innerLarge} 0 ${point(inner, innerStart)}Z`
}
</script>

<div class="flex flex-wrap items-center gap-x-4 gap-y-3">
  <div class="relative size-40 shrink-0">
    <svg viewBox="-50 -50 100 100" class="size-full" role="img" aria-label={ariaLabel}>
      {#each palette as color, slot (slot)}
        {@const emphasis = focus.current[slot] ?? 0}
        {@const dim = activeSlice == null ? 0 : 1 - emphasis}
        {@const slice = slices.find((candidate) => candidate.slot === slot)}
        {@const d = sectorPath(shape.current[slot * 2], shape.current[slot * 2 + 1], OUTER_RADIUS + EXPAND * emphasis)}
        {#if d}
          <path
            {d}
            fill={color}
            stroke={color}
            stroke-width={CORNER * 2}
            stroke-linejoin="round"
            fill-rule="evenodd"
            opacity={1 - 0.55 * dim}
            data-slice={slice?.key}
            role="presentation"
            onpointerenter={() => slice && (activeKey = slice.key)}
            onpointerleave={() => (activeKey = null)} />
        {/if}
      {/each}
    </svg>
    {#if center}
      <div class="pointer-events-none absolute inset-0 grid place-items-center">
        {@render center(activeSlice)}
      </div>
    {/if}
  </div>
  <ul class="min-w-36 flex-1 space-y-1 text-sm">
    {#each slices as slice (slice.key)}
      <li
        class="-mx-1.5 flex items-center gap-2 rounded-sm px-1.5 py-0.5 transition-colors duration-[140ms] data-[active=true]:bg-accent"
        data-active={activeKey === slice.key}
        onpointerenter={() => (activeKey = slice.key)}
        onpointerleave={() => (activeKey = null)}>
        <span class="size-2.5 shrink-0 rounded-[2px]" style:background={palette[slice.slot]}></span>
        <span class={['truncate', labelClass]}>{slice.label}</span>
        <span class="font-technical ml-auto text-muted-foreground tabular-nums"
          >{formatPercent(total > 0 ? slice.value / total : 0)}</span>
      </li>
    {/each}
  </ul>
</div>

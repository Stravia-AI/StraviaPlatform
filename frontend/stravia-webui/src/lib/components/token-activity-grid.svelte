<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import * as Tooltip from '$lib/components/ui/tooltip'
import { formatCompactCount, formatMinuteTime, formatMonthDay } from '$lib/format'
import type { ActivityCell, ActivityGridModel } from '$lib/stats-chart'

interface Props {
  model: ActivityGridModel
  /** 方格边长（px），由容器可用高度决定。 */
  cellPx?: number
}

let { model, cellPx = 14 }: Props = $props()

const SIX_HOURS_MS = 21_600_000
const DAY_MS = 86_400_000

const dayCells = $derived(model.bucketMs >= DAY_MS)

// 窗口延伸跨越多个自然日后，亚日粒度列只标首列与各自然日首列，
// 否则重复的时刻无法区分。
const multiDay = $derived.by(() => {
  const first = new Date(model.colStarts[0] ?? 0)
  const last = new Date(model.colStarts.at(-1) ?? 0)
  return first.toDateString() !== last.toDateString()
})

// 列起点恰为本地 00:00（小时列取 0 时列、6h 段列取 00–06 段）。
function isDayStart(start: number): boolean {
  const d = new Date(start)
  return d.getHours() === 0 && d.getMinutes() === 0 && d.getSeconds() === 0 && d.getMilliseconds() === 0
}

// 稀疏列标签：列多时跳格标注，避免相邻文字互相重叠。
// 亚日粒度的多日窗口只标注首列与每个自然日首列（本地 00:00），
// 日期唯一且不与相邻文字重叠；单日窗口沿用时刻标注。
const colLabels = $derived.by(() => {
  const step = Math.max(model.colCount > 4 ? 2 : 1, Math.ceil(model.colCount / 10))
  return model.colStarts.map((start, index) => {
    if (model.bucketMs >= SIX_HOURS_MS) {
      return index % step === 0 ? formatMonthDay(start) : null
    }
    if (multiDay) {
      return index === 0 || isDayStart(start) ? formatMonthDay(start) : null
    }
    return index % step === 0 ? formatMinuteTime(start) : null
  })
})

function cellTime(start: number): string {
  return dayCells ? formatMonthDay(start) : `${formatMonthDay(start)} ${formatMinuteTime(start)}`
}

function cellText(cell: ActivityCell): string {
  const time = cellTime(cell.start)
  return cell.tokens == null
    ? m.stats_activity_cell_unknown({ time })
    : m.stats_activity_cell_usage({ time, count: formatCompactCount(cell.tokens) })
}
</script>

<div class="flex min-w-0 flex-col items-end">
  <div
    class="grid w-fit gap-1"
    style="grid-template-columns:repeat({model.colCount},{cellPx}px);grid-auto-rows:{cellPx}px"
    role="group"
    aria-label={m.stats_token_activity()}>
    {#each model.cells as cell (cell.start)}
      <Tooltip.Root>
        <Tooltip.Trigger
          type="button"
          class="token-activity-cell cursor-default rounded-[3px] p-0 data-[level=0]:bg-secondary data-[level=1]:bg-chart-4 data-[level=2]:bg-chart-3 data-[level=3]:bg-chart-2 data-[level=4]:bg-chart-1"
          style="grid-column:{cell.col + 1};grid-row:{cell.row + 1}"
          data-level={cell.level}
          aria-label={cellText(cell)} />
        <Tooltip.Content side="top" sideOffset={4}>{cellText(cell)}</Tooltip.Content>
      </Tooltip.Root>
    {/each}
  </div>
  {#if colLabels.length > 0}
    <div
      class="mt-1.5 grid w-fit gap-1 overflow-hidden text-[0.625rem] leading-3 whitespace-nowrap text-muted-foreground"
      style="grid-template-columns:repeat({model.colCount},{cellPx}px)">
      {#each colLabels as label, index (index)}
        {#if label}
          <span style="grid-column-start:{index + 1};grid-row:1">{label}</span>
        {/if}
      {/each}
    </div>
  {/if}
  <div class="mt-2 flex w-fit items-center gap-1 text-[0.625rem] leading-3 text-muted-foreground">
    <span class="mr-1">{m.stats_activity_less()}</span>
    {#each [0, 1, 2, 3, 4] as level (level)}
      <span
        class="size-3 rounded-[3px] data-[level=0]:bg-secondary data-[level=1]:bg-chart-4 data-[level=2]:bg-chart-3 data-[level=3]:bg-chart-2 data-[level=4]:bg-chart-1"
        data-level={level}></span>
    {/each}
    <span class="ml-1">{m.stats_activity_more()}</span>
  </div>
</div>

<style>
/* Trigger 的实际 DOM 由 Bits UI 组件渲染，scoped 选择器够不到，必须用 :global。 */
:global(.token-activity-cell:hover),
:global(.token-activity-cell:focus-visible) {
  outline: 1px solid var(--foreground);
  outline-offset: 0;
}
</style>

<script lang="ts">
import { Axis, Chart, Circle, Path, Svg, Tooltip, type ChartState } from 'layerchart'

import { chartMotion } from '#lib/chart-motion.svelte.js'
import * as m from '#lib/paraglide/messages.js'
import { formatCompactCount, formatNumber, formatPercent, formatTime } from '#lib/format.js'
import type { AvailabilityChartPoint } from '#lib/stats-chart.js'

type AvailabilityChartContext = ChartState<AvailabilityChartPoint>

let {
  points,
  formatBucket = formatTime,
}: { points: AvailabilityChartPoint[]; formatBucket?: (value: Date) => string } = $props()

// 窗口、筛选或悬停区间切换时折线、差值带与圆点一起缓动；layerchart 只在创建时读取该配置。
const motion = chartMotion()

let context = $state<AvailabilityChartContext>()
let keyboardIndex = $state(0)
let touchPointer = false
const selectedIndex = $derived(
  Math.max(
    0,
    Math.min(
      context?.tooltip.data ? points.indexOf(context.tooltip.data as AvailabilityChartPoint) : keyboardIndex,
      points.length - 1,
    ),
  ),
)
const selectedPoint = $derived(points[selectedIndex])
const descriptionId = $props.id()

// 两条线共用固定的 0–100% 轴，跨窗口与时间桶保持同一刻度，不随数据起伏缩放。
const yDomain: [number, number] = [0, 1]

// Path 直接使用真实 scale；恢复未知或分母为零的桶是 null，必须重新起笔，不能跨空桶连接。
function linePath(chart: AvailabilityChartContext, series: 'final' | 'firstPass'): string {
  const scale = chart.yScale
  if (!scale) return ''
  let path = ''
  let connected = false
  for (const point of points) {
    const value = point[series]
    if (value == null) {
      connected = false
      continue
    }
    path += `${connected ? 'L' : 'M'}${chart.xScale(point.bucket)},${scale(value)}`
    connected = true
  }
  return path
}

/**
 * 两线之间的自动恢复差值带。缺桶、恢复未知或分母为零时结束当前子路径，
 * 下一个有效桶重新起笔，绝不跨缺口闭合填充。
 */
function bandPath(chart: AvailabilityChartContext): string {
  const scale = chart.yScale
  if (!scale) return ''
  let path = ''
  let run: Array<{ bucket: Date; final: number; firstPass: number }> = []
  const flush = () => {
    if (run.length === 0) return
    const first = run[0]
    const last = run[run.length - 1]
    let segment = `M${chart.xScale(first.bucket)},${scale(first.final)}`
    for (const point of run.slice(1)) segment += `L${chart.xScale(point.bucket)},${scale(point.final)}`
    segment += `L${chart.xScale(last.bucket)},${scale(last.firstPass)}`
    for (let index = run.length - 2; index >= 0; index -= 1) {
      const point = run[index]
      segment += `L${chart.xScale(point.bucket)},${scale(point.firstPass)}`
    }
    path += `${segment}Z`
    run = []
  }
  for (const point of points) {
    const final = point.final
    const firstPass = point.firstPass
    if (final == null || firstPass == null) flush()
    else run.push({ bucket: point.bucket, final, firstPass })
  }
  flush()
  return path
}

function rateText(value: number | null): string {
  return value == null ? '—' : formatPercent(value)
}

/** 百分点读数：零恢复显示 0，正值带 +，未知显示破折号。 */
function gainText(value: number | null): string {
  if (value == null) return '—'
  return `${value > 0 ? '+' : ''}${formatNumber(value)}`
}

function readout(point: AvailabilityChartPoint): string {
  const final = `${m.stats_final_success_rate()}: ${rateText(point.final)} (${m.stats_availability_final_reading({ failed: formatCompactCount(point.failedRequests), total: formatCompactCount(point.terminalRequests) })})`
  const firstPass =
    point.firstPass == null
      ? `${m.stats_first_pass_success_rate()}: —${point.final == null ? '' : ` · ${m.stats_recovery_unknown_reading()}`}`
      : `${m.stats_first_pass_success_rate()}: ${rateText(point.firstPass)} (${m.stats_availability_first_pass_reading({ passed: formatCompactCount(point.terminalRequests - point.failedRequests - point.recoveredRequests), total: formatCompactCount(point.terminalRequests) })}) · ${m.stats_recovery_gain({ count: formatCompactCount(point.recoveredRequests), points: gainText(point.recoveryGainPoints) })}`
  const upstream = `${m.stats_upstream_attempts()}: ${formatCompactCount(point.upstreamRequests)} · ${m.stats_upstream_attempt_success_rate()}: ${rateText(point.upstreamAttemptSuccess)} · ${m.stats_availability_upstream_reading({ errors: formatCompactCount(point.upstreamErrors), total: formatCompactCount(point.upstreamRequests) })}`
  return `${formatBucket(point.bucket)} · ${final} · ${firstPass} · ${upstream}`
}

function showBucket(index: number) {
  if (!context || points.length === 0) return
  keyboardIndex = Math.max(0, Math.min(index, points.length - 1))
  context.tooltip.show({
    data: points[keyboardIndex],
    point: {
      x: Number(context.xScale(points[keyboardIndex].bucket)) + context.padding.left,
      y: context.height / 2 + context.padding.top,
    },
  })
}

function onTouchClick(event: MouseEvent & { currentTarget: HTMLDivElement }) {
  if (!touchPointer || !context) return
  touchPointer = false
  // 原生触摸离开可能先清空 data；点击按横坐标重新解析，不依赖 hide 与 click 的任务顺序。
  context.tooltip.show({
    point: {
      x: event.clientX - event.currentTarget.getBoundingClientRect().left,
      y: context.height / 2 + context.padding.top,
    },
  })
  const point = context.tooltip.data as AvailabilityChartPoint | undefined
  if (point) keyboardIndex = Math.max(0, points.indexOf(point))
}

function onkeydown(event: KeyboardEvent) {
  const next =
    event.key === 'Home'
      ? 0
      : event.key === 'End'
        ? points.length - 1
        : event.key === 'ArrowLeft'
          ? selectedIndex - 1
          : event.key === 'ArrowRight'
            ? selectedIndex + 1
            : undefined
  if (next != null) {
    event.preventDefault()
    showBucket(next)
  } else if (event.key === 'Escape') {
    context?.tooltip.hide()
  }
}
</script>

<!-- 指针已选中触点所在桶，不能在随后聚焦时重置为首桶。 -->
<div
  class="h-full min-w-0"
  role="slider"
  tabindex="0"
  aria-label={m.stats_availability_chart()}
  aria-describedby={descriptionId}
  aria-valuemin={0}
  aria-valuemax={Math.max(0, points.length - 1)}
  aria-valuenow={selectedIndex}
  aria-valuetext={selectedPoint ? readout(selectedPoint) : undefined}
  onfocus={(event) => {
    if (event.currentTarget.matches(':focus-visible')) showBucket(0)
  }}
  onpointerdown={(event) => {
    touchPointer = event.pointerType === 'touch'
  }}
  onpointercancel={() => {
    touchPointer = false
  }}
  onclick={onTouchClick}
  onblur={() => context?.tooltip.hide()}
  {onkeydown}>
  <span id={descriptionId} class="sr-only">{m.stats_chart_keyboard_help()}</span>
  <Chart
    data={points}
    x="bucket"
    y="final"
    {yDomain}
    padding={{ top: 20, right: 16, bottom: 28, left: 44 }}
    tooltipContext={{ mode: 'bisect-x' }}
    bind:context>
    {#snippet children({ context: chart }: { context: AvailabilityChartContext })}
      <Svg>
        <Axis
          placement="left"
          role="group"
          aria-label={m.stats_availability_axis()}
          grid
          ticks={3}
          format={(value: number) => formatPercent(value)}
          tickLabelProps={{ fontSize: 11 }} />
        <Axis
          placement="bottom"
          ticks={4}
          format={formatBucket}
          tickOcclusion={{ priority: 'start-end', padding: 8 }}
          tickLabelProps={{ fontSize: 11 }} />
        <!-- 差值带先铺底，恢复未知或缺失的桶断开子路径；随后一次通过虚线、最终成功实线。 -->
        <Path
          pathData={bandPath(chart)}
          data-slot="recovery-band"
          aria-hidden="true"
          fill="var(--chart-2)"
          fillOpacity={0.14}
          stroke="none"
          {motion} />
        <Path
          pathData={linePath(chart, 'firstPass')}
          aria-label={m.stats_first_pass_success_rate()}
          fill="none"
          stroke="var(--chart-2)"
          strokeWidth={2}
          stroke-dasharray="6 4"
          {motion} />
        <Path
          pathData={linePath(chart, 'final')}
          aria-label={m.stats_final_success_rate()}
          fill="none"
          stroke="var(--chart-1)"
          strokeWidth={2}
          {motion} />
        <!-- 圆点按序号对位，才能随折线一起移动；按时间作 key 会在切换区间时整批消失再出现。 -->
        {#each points as point, index (index)}
          {#if point.firstPass != null && chart.yScale}
            <Circle
              cx={Number(chart.xScale(point.bucket))}
              cy={Number(chart.yScale(point.firstPass))}
              r={2}
              fill="var(--chart-2)"
              aria-hidden="true"
              {motion} />
          {/if}
          {#if point.final != null && chart.yScale}
            <Circle
              cx={Number(chart.xScale(point.bucket))}
              cy={Number(chart.yScale(point.final))}
              r={2}
              fill="var(--chart-1)"
              aria-hidden="true"
              {motion} />
          {/if}
        {/each}
      </Svg>
      <Tooltip.Root context={chart} motion="none" fadeDuration={0} props={{ root: { role: 'tooltip' } }}>
        {#snippet children({ data: point }: { data: AvailabilityChartPoint })}
          <div class="max-w-[calc(100vw-2rem)] font-technical text-xs whitespace-normal tabular-nums">
            <p class="mb-1">{formatBucket(point.bucket)}</p>
            <dl class="grid grid-cols-[minmax(0,1fr)_minmax(0,2fr)] gap-x-3 gap-y-1">
              <dt>{m.stats_final_success_rate()}</dt>
              <dd class="text-right">
                <span class="block">{rateText(point.final)}</span>
                <span class="block text-muted-foreground"
                  >{m.stats_availability_final_reading({
                    failed: formatCompactCount(point.failedRequests),
                    total: formatCompactCount(point.terminalRequests),
                  })}</span>
              </dd>
              <dt>{m.stats_first_pass_success_rate()}</dt>
              <dd class="text-right">
                <span class="block">{rateText(point.firstPass)}</span>
                {#if point.firstPass != null}
                  <span class="block text-muted-foreground"
                    >{m.stats_availability_first_pass_reading({
                      passed: formatCompactCount(
                        point.terminalRequests - point.failedRequests - point.recoveredRequests,
                      ),
                      total: formatCompactCount(point.terminalRequests),
                    })}</span>
                {/if}
              </dd>
              <dt>{m.stats_recovered_request_count()}</dt>
              <dd class="text-right">
                {#if point.firstPass == null}
                  <span class="block">—</span>
                {:else}
                  <span class="block"
                    >{m.stats_recovery_gain({
                      count: formatCompactCount(point.recoveredRequests),
                      points: gainText(point.recoveryGainPoints),
                    })}</span>
                {/if}
              </dd>
              <dt>{m.stats_upstream_attempts()}</dt>
              <dd class="text-right">
                <span class="block">{formatCompactCount(point.upstreamRequests)}</span>
                <span class="block text-muted-foreground"
                  >{m.stats_upstream_attempt_success_rate()}: {rateText(point.upstreamAttemptSuccess)} ·
                  {m.stats_availability_upstream_reading({
                    errors: formatCompactCount(point.upstreamErrors),
                    total: formatCompactCount(point.upstreamRequests),
                  })}</span>
              </dd>
            </dl>
            {#if point.firstPass == null && point.final != null}
              <p class="mt-1 max-w-64 text-pretty text-muted-foreground">{m.stats_recovery_unknown_reading()}</p>
            {/if}
          </div>
        {/snippet}
      </Tooltip.Root>
    {/snippet}
  </Chart>
</div>

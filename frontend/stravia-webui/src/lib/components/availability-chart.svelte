<script lang="ts">
import { Axis, Chart, Path, Svg, Tooltip, type ChartState } from 'layerchart'

import * as m from '#lib/paraglide/messages.js'
import { formatCompactCount, formatPercent, formatTime } from '#lib/format.js'
import type { AvailabilityChartPoint } from '#lib/stats-chart.js'

type AvailabilityChartContext = ChartState<AvailabilityChartPoint>

let {
  points,
  formatBucket = formatTime,
}: { points: AvailabilityChartPoint[]; formatBucket?: (value: Date) => string } = $props()

let context = $state<AvailabilityChartContext>()
let keyboardIndex = $state(0)
const selectedPoint = $derived(points[Math.min(keyboardIndex, points.length - 1)])
const descriptionId = $props.id()

// 单一百分比轴：下限贴合两条线的实际最低值，上限固定 100%，不让低幅波动被压成直线。
const yDomain = $derived.by((): [number, number] => {
  let minimum = 1
  for (const point of points) {
    if (point.client != null) minimum = Math.min(minimum, point.client)
    if (point.upstream != null) minimum = Math.min(minimum, point.upstream)
  }
  if (minimum >= 1) return [0.95, 1]
  const padding = Math.max(0.005, (1 - minimum) * 0.2)
  return [Math.max(0, Math.floor((minimum - padding) * 100) / 100), 1]
})

// Path 直接使用真实 scale；分母为零的桶是 null，必须重新起笔，不能跨空桶连接。
function linePath(chart: AvailabilityChartContext, series: 'client' | 'upstream'): string {
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

function availabilityText(value: number | null): string {
  return value == null ? '—' : formatPercent(value)
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

function onkeydown(event: KeyboardEvent) {
  const next =
    event.key === 'Home'
      ? 0
      : event.key === 'End'
        ? points.length - 1
        : event.key === 'ArrowLeft'
          ? keyboardIndex - 1
          : event.key === 'ArrowRight'
            ? keyboardIndex + 1
            : undefined
  if (next != null) {
    event.preventDefault()
    showBucket(next)
  } else if (event.key === 'Escape') {
    context?.tooltip.hide()
  }
}
</script>

<div
  class="h-full min-w-0"
  role="slider"
  tabindex="0"
  aria-label={m.stats_availability_chart()}
  aria-describedby={descriptionId}
  aria-valuemin={0}
  aria-valuemax={Math.max(0, points.length - 1)}
  aria-valuenow={Math.min(keyboardIndex, Math.max(0, points.length - 1))}
  aria-valuetext={selectedPoint
    ? `${formatBucket(selectedPoint.bucket)} · ${m.stats_client_availability()}: ${availabilityText(selectedPoint.client)} (${m.stats_availability_client_reading({ errors: formatCompactCount(selectedPoint.failedRequests), total: formatCompactCount(selectedPoint.terminalRequests) })}) · ${m.stats_upstream_availability()}: ${availabilityText(selectedPoint.upstream)} (${m.stats_availability_upstream_reading({ errors: formatCompactCount(selectedPoint.upstreamErrors), total: formatCompactCount(selectedPoint.upstreamRequests) })})`
    : undefined}
  onfocus={() => showBucket(0)}
  onblur={() => context?.tooltip.hide()}
  {onkeydown}>
  <span id={descriptionId} class="sr-only">{m.stats_chart_keyboard_help()}</span>
  <Chart
    data={points}
    x="bucket"
    y="client"
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
        <!-- 上游先画、客户端后画：重叠时实线压在上方，图例顺序也与之一致。 -->
        <Path
          pathData={linePath(chart, 'upstream')}
          aria-label={m.stats_upstream_availability()}
          fill="none"
          stroke="var(--chart-1)"
          strokeWidth={2}
          stroke-dasharray="6 4" />
        <Path
          pathData={linePath(chart, 'client')}
          aria-label={m.stats_client_availability()}
          fill="none"
          stroke="var(--chart-2)"
          strokeWidth={2} />
        {#each points as point (point.bucket.getTime())}
          {#if point.upstream != null && chart.yScale}
            <circle
              cx={chart.xScale(point.bucket)}
              cy={Number(chart.yScale(point.upstream))}
              r="2"
              fill="var(--chart-1)"
              aria-hidden="true" />
          {/if}
          {#if point.client != null && chart.yScale}
            <circle
              cx={chart.xScale(point.bucket)}
              cy={Number(chart.yScale(point.client))}
              r="2"
              fill="var(--chart-2)"
              aria-hidden="true" />
          {/if}
        {/each}
      </Svg>
      <Tooltip.Root context={chart} motion="none" fadeDuration={0} props={{ root: { role: 'tooltip' } }}>
        {#snippet children({ data: point }: { data: AvailabilityChartPoint })}
          <div class="font-technical text-xs tabular-nums">
            <p class="mb-1">{formatBucket(point.bucket)}</p>
            <dl class="grid grid-cols-[auto_auto] gap-x-3 gap-y-1">
              <dt>{m.stats_client_availability()}</dt>
              <dd class="text-right">
                <span class="block">{availabilityText(point.client)}</span>
                <span class="block text-muted-foreground"
                  >{m.stats_availability_client_reading({
                    errors: formatCompactCount(point.failedRequests),
                    total: formatCompactCount(point.terminalRequests),
                  })}</span>
              </dd>
              <dt>{m.stats_upstream_availability()}</dt>
              <dd class="text-right">
                <span class="block">{availabilityText(point.upstream)}</span>
                <span class="block text-muted-foreground"
                  >{m.stats_availability_upstream_reading({
                    errors: formatCompactCount(point.upstreamErrors),
                    total: formatCompactCount(point.upstreamRequests),
                  })}</span>
              </dd>
            </dl>
          </div>
        {/snippet}
      </Tooltip.Root>
    {/snippet}
  </Chart>
</div>

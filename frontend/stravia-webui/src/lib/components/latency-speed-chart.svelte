<script lang="ts">
import { Axis, Chart, Path, Svg, Tooltip, type ChartState } from 'layerchart'

import * as m from '$lib/paraglide/messages.js'
import { formatNumber, formatTime } from '$lib/format'
import type { LatencyChartPoint } from '$lib/stats-chart'

type LatencyChartContext = ChartState<LatencyChartPoint>

let { data, formatBucket = formatTime }: { data: LatencyChartPoint[]; formatBucket?: (value: Date) => string } =
  $props()

let context = $state<LatencyChartContext>()
let keyboardIndex = $state(0)
const selectedPoint = $derived(data[Math.min(keyboardIndex, data.length - 1)])
const descriptionId = $props.id()
const limits = $derived.by(() => {
  let firstToken = 0
  let outputTps = 0
  for (const point of data) {
    firstToken = Math.max(firstToken, point.firstToken ?? 0)
    outputTps = Math.max(outputTps, point.outputTps ?? 0)
  }
  return { firstToken: firstToken > 0 ? firstToken * 1.1 : 1, outputTps: outputTps > 0 ? outputTps * 1.1 : 1 }
})

// Path 直接使用各指标的真实 scale；缺值重新起笔，不能跨空桶连接。
function linePath(chart: LatencyChartContext, metric: 'firstToken' | 'outputTps'): string {
  const scale = metric === 'firstToken' ? chart.yScale : chart.y1Scale
  if (!scale) return ''
  let path = ''
  let connected = false
  for (const point of data) {
    const value = point[metric]
    if (value == null) {
      connected = false
      continue
    }
    path += `${connected ? 'L' : 'M'}${chart.xScale(point.bucket)},${scale(value)}`
    connected = true
  }
  return path
}

function showBucket(index: number) {
  if (!context || data.length === 0) return
  keyboardIndex = Math.max(0, Math.min(index, data.length - 1))
  context.tooltip.show({
    data: data[keyboardIndex],
    point: {
      x: Number(context.xScale(data[keyboardIndex].bucket)) + context.padding.left,
      y: context.height / 2 + context.padding.top,
    },
  })
}

function onkeydown(event: KeyboardEvent) {
  const next =
    event.key === 'Home'
      ? 0
      : event.key === 'End'
        ? data.length - 1
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

function metricValue(value: number | null, unit: string): string {
  return value == null ? '—' : `${formatNumber(value)} ${unit}`
}
</script>

<div
  class="h-full min-w-0"
  role="slider"
  tabindex="0"
  aria-label={m.stats_latency_speed_chart()}
  aria-describedby={descriptionId}
  aria-valuemin={0}
  aria-valuemax={Math.max(0, data.length - 1)}
  aria-valuenow={Math.min(keyboardIndex, Math.max(0, data.length - 1))}
  aria-valuetext={selectedPoint
    ? `${formatBucket(selectedPoint.bucket)} · ${m.stats_first_token_latency()}: ${metricValue(selectedPoint.firstToken, 's')} · ${m.stats_tps()}: ${metricValue(selectedPoint.outputTps, 'tok/s')}`
    : undefined}
  onfocus={() => showBucket(0)}
  onblur={() => context?.tooltip.hide()}
  {onkeydown}>
  <span id={descriptionId} class="sr-only">{m.stats_chart_keyboard_help()}</span>
  <Chart
    {data}
    x="bucket"
    y="firstToken"
    y1="outputTps"
    yDomain={[0, limits.firstToken]}
    y1Domain={[0, limits.outputTps]}
    y1Range={({ height }) => [height, 0]}
    padding={{ top: 20, right: 48, bottom: 28, left: 40 }}
    tooltipContext={{ mode: 'bisect-x' }}
    bind:context>
    {#snippet children({ context: chart }: { context: LatencyChartContext })}
      <Svg>
        <Axis
          placement="left"
          role="group"
          aria-label={m.stats_first_token_axis()}
          grid
          ticks={3}
          tickLabelProps={{ fontSize: 11 }} />
        <Axis
          placement="right"
          role="group"
          aria-label={m.stats_tps_axis()}
          scale={chart.y1Scale}
          ticks={3}
          tickLabelProps={{ fontSize: 11 }} />
        <text x="-8" y="-8" text-anchor="end" class="fill-muted-foreground text-[11px]">s</text>
        <text x={chart.width + 8} y="-8" class="fill-muted-foreground text-[11px]">tok/s</text>
        <Axis
          placement="bottom"
          ticks={4}
          format={formatBucket}
          tickOcclusion={{ priority: 'start-end', padding: 8 }}
          tickLabelProps={{ fontSize: 11 }} />
        <Path
          pathData={linePath(chart, 'firstToken')}
          aria-label={m.stats_first_token_latency()}
          fill="none"
          stroke="var(--chart-2)"
          strokeWidth={2} />
        <Path
          pathData={linePath(chart, 'outputTps')}
          aria-label={m.stats_tps()}
          fill="none"
          stroke="var(--chart-1)"
          strokeWidth={2}
          stroke-dasharray="6 4" />
        {#each data as point (point.bucket.getTime())}
          {#if point.firstToken != null}
            <circle
              cx={chart.xScale(point.bucket)}
              cy={chart.yScale(point.firstToken)}
              r="2"
              fill="var(--chart-2)"
              aria-hidden="true" />
          {/if}
          {#if point.outputTps != null && chart.y1Scale}
            <circle
              cx={chart.xScale(point.bucket)}
              cy={Number(chart.y1Scale(point.outputTps))}
              r="2"
              fill="var(--chart-1)"
              aria-hidden="true" />
          {/if}
        {/each}
      </Svg>
      <!-- 窄屏绘图区不足以在指针两侧容纳读数；水平居中避免翻转后越出视口。 -->
      <Tooltip.Root
        context={chart}
        x={chart.containerWidth / 2}
        xOffset={0}
        anchor="top"
        motion="none"
        fadeDuration={0}
        props={{ root: { role: 'tooltip' } }}>
        {#snippet children({ data: point }: { data: LatencyChartPoint })}
          <div class="font-technical text-xs tabular-nums">
            <p class="mb-1">{formatBucket(point.bucket)}</p>
            <dl class="grid grid-cols-[auto_auto] gap-x-3 gap-y-1">
              <dt>{m.stats_first_token_latency()}</dt>
              <dd class="text-right">{metricValue(point.firstToken, 's')}</dd>
              <dt>{m.stats_tps()}</dt>
              <dd class="text-right">{metricValue(point.outputTps, 'tok/s')}</dd>
            </dl>
          </div>
        {/snippet}
      </Tooltip.Root>
    {/snippet}
  </Chart>
</div>

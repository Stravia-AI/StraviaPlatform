<script lang="ts">
import { Axis, Chart, Path, Svg, Tooltip, type ChartState } from 'layerchart'

import * as m from '#lib/paraglide/messages.js'
import { formatAmount, formatMoneyCoverage } from '#lib/money.js'
import { formatTime } from '#lib/format.js'
import type { DisplayCurrency } from '#lib/types.js'
import type { CostChartPoint } from '#lib/stats-chart.js'

type CostChartContext = ChartState<CostChartPoint>

let {
  points,
  currency,
  formatBucket = formatTime,
}: { points: CostChartPoint[]; currency: DisplayCurrency; formatBucket?: (value: Date) => string } = $props()

let context = $state<CostChartContext>()
let keyboardIndex = $state(0)
const selectedPoint = $derived(points[Math.min(keyboardIndex, points.length - 1)])
const descriptionId = $props.id()

const yDomain = $derived.by((): [number, number] => {
  let maximum = 0
  for (const point of points) if (point.amount != null) maximum = Math.max(maximum, point.amount)
  return [0, maximum > 0 ? maximum * 1.1 : 1]
})

function linePath(chart: CostChartContext): string {
  const scale = chart.yScale
  if (!scale) return ''
  let path = ''
  let connected = false
  for (const point of points) {
    if (point.amount == null) {
      connected = false
      continue
    }
    path += `${connected ? 'L' : 'M'}${chart.xScale(point.bucket)},${scale(point.amount)}`
    connected = true
  }
  return path
}

function axisAmount(value: number): string {
  return formatAmount(String(value), currency.symbol)
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
  aria-label={m.cost_series_chart()}
  aria-describedby={descriptionId}
  aria-valuemin={0}
  aria-valuemax={Math.max(0, points.length - 1)}
  aria-valuenow={Math.min(keyboardIndex, Math.max(0, points.length - 1))}
  aria-valuetext={selectedPoint
    ? `${formatBucket(selectedPoint.bucket)} · ${m.cost_actual()}: ${selectedPoint.amount == null ? m.billing_unknown() : formatAmount(selectedPoint.amountLabel, currency.symbol)} · ${m.cost_coverage()}: ${formatMoneyCoverage(selectedPoint.cost)}`
    : undefined}
  onfocus={() => showBucket(0)}
  onblur={() => context?.tooltip.hide()}
  {onkeydown}>
  <span id={descriptionId} class="sr-only">{m.stats_chart_keyboard_help()}</span>
  <Chart
    data={points}
    x="bucket"
    y="amount"
    {yDomain}
    padding={{ top: 20, right: 16, bottom: 28, left: 58 }}
    tooltipContext={{ mode: 'bisect-x' }}
    bind:context>
    {#snippet children({ context: chart }: { context: CostChartContext })}
      <Svg>
        <Axis
          placement="left"
          role="group"
          aria-label={m.cost_series_amount_axis()}
          grid
          ticks={3}
          format={axisAmount}
          tickLabelProps={{ fontSize: 11 }} />
        <Axis
          placement="bottom"
          ticks={4}
          format={formatBucket}
          tickOcclusion={{ priority: 'start-end', padding: 8 }}
          tickLabelProps={{ fontSize: 11 }} />
        <Path
          pathData={linePath(chart)}
          aria-label={m.cost_actual()}
          fill="none"
          stroke="var(--chart-1)"
          strokeWidth={2} />
        {#each points as point (point.bucket.getTime())}
          {#if point.amount != null && chart.yScale}
            <circle
              cx={chart.xScale(point.bucket)}
              cy={Number(chart.yScale(point.amount))}
              r="2"
              fill="var(--chart-1)"
              aria-hidden="true" />
          {/if}
        {/each}
      </Svg>
      <Tooltip.Root context={chart} motion="none" fadeDuration={0} props={{ root: { role: 'tooltip' } }}>
        {#snippet children({ data: point }: { data: CostChartPoint })}
          <div class="font-technical text-xs tabular-nums">
            <p class="mb-1">{formatBucket(point.bucket)}</p>
            <dl class="grid grid-cols-[auto_auto] gap-x-3 gap-y-1">
              <dt>{m.cost_actual()}</dt>
              <dd class="text-right">
                {point.amount == null ? m.billing_unknown() : formatAmount(point.amountLabel, currency.symbol)}
              </dd>
              <dt>{m.cost_coverage()}</dt>
              <dd class="text-right">{formatMoneyCoverage(point.cost)}</dd>
            </dl>
          </div>
        {/snippet}
      </Tooltip.Root>
    {/snippet}
  </Chart>
</div>

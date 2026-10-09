<script lang="ts">
import CircleHelpIcon from '@lucide/svelte/icons/circle-help'

import * as m from '$lib/paraglide/messages.js'
import { formatNumber } from '$lib/format'
import type { StatsOverview } from '$lib/types'
import * as Tooltip from '$lib/components/ui/tooltip'

let { overview }: { overview?: Pick<StatsOverview, 'avg_first_token_ms' | 'avg_output_tps'> } = $props()
const hintId = $props.id()
const firstToken = $derived(overview?.avg_first_token_ms)
const outputTps = $derived(overview?.avg_output_tps)
</script>

<div class="flex min-w-0 items-center gap-1">
  <dl class="font-technical grid grid-cols-[minmax(0,auto)_auto] gap-x-2 gap-y-1 text-xs tabular-nums">
    <dt class="flex items-center gap-2 text-muted-foreground">
      <span class="w-4 shrink-0 border-t-2 border-chart-2" aria-hidden="true"></span>
      {m.stats_first_token_latency()}
    </dt>
    <dd class="text-right whitespace-nowrap">{firstToken == null ? '—' : `${formatNumber(firstToken / 1000)} s`}</dd>
    <dt class="flex items-center gap-2 text-muted-foreground">
      <span class="w-4 shrink-0 border-t-2 border-dashed border-chart-1" aria-hidden="true"></span>
      {m.stats_tps()}
    </dt>
    <dd class="text-right whitespace-nowrap">{outputTps == null ? '—' : `${formatNumber(outputTps)} tok/s`}</dd>
  </dl>
  <Tooltip.Root delayDuration={0}>
    <Tooltip.Trigger
      type="button"
      class="inline-flex size-10 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:text-foreground"
      aria-label={m.stats_latency_speed_help()}
      aria-describedby={hintId}>
      <CircleHelpIcon class="size-4" />
    </Tooltip.Trigger>
    <Tooltip.Content id={hintId} role="tooltip" class="max-w-[min(20rem,calc(100vw-2rem))] text-pretty">
      {m.stats_latency_speed_definition()}
    </Tooltip.Content>
  </Tooltip.Root>
</div>

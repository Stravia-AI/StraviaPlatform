<script lang="ts">
import CircleHelpIcon from '@lucide/svelte/icons/circle-help'

import * as m from '#lib/paraglide/messages.js'
import { formatPercent } from '#lib/format.js'
import type { AvailabilitySummary as AvailabilitySummaryData } from '#lib/stats-chart.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'

let { summary }: { summary: AvailabilitySummaryData } = $props()
const hintId = $props.id()
</script>

<div class="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
  <dl class="font-technical grid grid-cols-[minmax(0,auto)_auto] gap-x-2 gap-y-1 text-xs tabular-nums">
    <dt class="flex items-center gap-2 text-muted-foreground">
      <span class="w-4 shrink-0 border-t-2 border-chart-1" aria-hidden="true"></span>
      {m.stats_final_success_rate()}
    </dt>
    <dd class="text-right whitespace-nowrap">{summary.final == null ? '—' : formatPercent(summary.final)}</dd>
    <dt class="flex items-center gap-2 text-muted-foreground">
      <span class="w-4 shrink-0 border-t-2 border-dashed border-chart-2" aria-hidden="true"></span>
      {m.stats_first_pass_success_rate()}
    </dt>
    <dd class="text-right whitespace-nowrap">{summary.firstPass == null ? '—' : formatPercent(summary.firstPass)}</dd>
  </dl>
  <Tooltip.Root delayDuration={0} disableCloseOnTriggerClick>
    <Tooltip.Trigger
      type="button"
      class="inline-flex shrink-0 items-center justify-center rounded-md text-muted-foreground hover:text-foreground"
      aria-label={m.stats_availability_help()}
      aria-describedby={hintId}>
      <CircleHelpIcon class="size-4" />
    </Tooltip.Trigger>
    <Tooltip.Content id={hintId} role="tooltip" class="max-w-[min(24rem,calc(100vw-2rem))] text-pretty">
      {m.stats_availability_definition()}
    </Tooltip.Content>
  </Tooltip.Root>
</div>

<script lang="ts">
import { Progress } from '$lib/components/ui/progress'
import * as m from '$lib/paraglide/messages.js'
import { startupPhaseLabel } from '$lib/startup-progress'
import type { StartupProgress } from '$lib/startup-progress'

let { progress }: { progress: StartupProgress } = $props()
const label = $derived(startupPhaseLabel(progress))
const total = $derived(progress.total !== null && progress.total > 0 ? progress.total : null)
const percentage = $derived(total === null ? null : Math.min(100, Math.max(0, (progress.completed / total) * 100)))
</script>

<div class="flex min-w-0 flex-col gap-3">
  <div aria-live="polite" aria-atomic="true">
    <p class="text-xs font-medium text-muted-foreground">{m.startup_current_phase()}</p>
    <p class="mt-1 break-words text-base font-medium">{label}</p>
  </div>
  <Progress value={percentage} aria-label={`${m.startup_current_phase()}: ${label}`} />
  {#if total !== null && percentage !== null}
    <p
      class="flex flex-wrap justify-between gap-2 font-mono text-xs tabular-nums text-muted-foreground"
      aria-live="off">
      <span>{progress.completed} / {total}</span>
      <span>{Math.floor(percentage)}%</span>
    </p>
  {/if}
</div>

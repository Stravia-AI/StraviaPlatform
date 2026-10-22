<script lang="ts">
import { Progress } from '#lib/components/ui/progress/index.js'
import { startupPhaseLabel } from '#lib/startup-progress.js'
import type { StartupProgress } from '#lib/startup-progress.js'

let { progress }: { progress: StartupProgress } = $props()
const label = $derived(startupPhaseLabel(progress))
const total = $derived(progress.total !== null && progress.total > 0 ? progress.total : null)
const percentage = $derived(total === null ? null : Math.min(100, Math.max(0, (progress.completed / total) * 100)))
</script>

<div class="flex min-w-0 flex-col gap-3">
  <p class="text-base font-medium break-words" aria-live="polite" aria-atomic="true">{label}</p>
  <Progress value={percentage} aria-label={label} />
  {#if total !== null && percentage !== null}
    <p
      class="flex flex-wrap justify-between gap-2 font-mono text-xs tabular-nums text-muted-foreground"
      aria-live="off">
      <span>{progress.completed} / {total}</span>
      <span>{Math.floor(percentage)}%</span>
    </p>
  {/if}
</div>

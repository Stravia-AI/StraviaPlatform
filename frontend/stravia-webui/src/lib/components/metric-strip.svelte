<script lang="ts">
import { Skeleton } from '$lib/components/ui/skeleton'

export interface MetricItem {
  label: string
  value: string
  detail?: string
  tone?: 'default' | 'error'
}

interface Props {
  metrics?: MetricItem[]
  label?: string
  loading?: boolean
  loadingLabel?: string
  placeholderCount?: number
}

let { metrics = [], label, loading = false, loadingLabel, placeholderCount = 6 }: Props = $props()
</script>

<div class="route-metric-strip" aria-label={loading ? loadingLabel : label} aria-busy={loading}>
  {#if loading}
    {#each Array(placeholderCount) as _, index (index)}
      <div class="route-metric-strip__item"><Skeleton class="h-4 w-24" /><Skeleton class="mt-2 h-7 w-20" /></div>
    {/each}
  {:else}
    {#each metrics as metric (metric.label)}
      <div class="route-metric-strip__item">
        <p class="text-xs leading-4 font-medium text-muted-foreground">{metric.label}</p>
        <p
          class={[
            'font-technical mt-1 text-xl leading-6 font-medium tabular-nums sm:text-2xl',
            metric.tone === 'error' && 'text-destructive',
          ]}>
          {metric.value}
        </p>
        {#if metric.detail}<p class="mt-1 text-xs text-muted-foreground">{metric.detail}</p>{/if}
      </div>
    {/each}
  {/if}
</div>

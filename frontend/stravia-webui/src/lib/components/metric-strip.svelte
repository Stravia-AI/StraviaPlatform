<script lang="ts">
export interface MetricItem {
  label: string
  value: string
  detail?: string
  tone?: 'default' | 'error'
}

interface Props {
  metrics: MetricItem[]
  label: string
}

let { metrics, label }: Props = $props()
</script>

<div class="route-metric-strip" aria-label={label}>
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
</div>

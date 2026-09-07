<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { Handle, Position, type NodeProps } from '@xyflow/svelte'

import { formatCompactCount, formatLogTime } from '$lib/format'
import { observationDebugStatusLabel, observationStatusLabel } from '$lib/observation-labels'
import type { InteractionNodeData } from '$lib/types'
import { Badge } from '$lib/components/ui/badge'

type InteractionNode = import('@xyflow/svelte').Node<InteractionNodeData, 'interaction'>
let { data, selected }: NodeProps<InteractionNode> = $props()

const interaction = $derived(data.interaction)
const title = $derived(interaction.first_model_display_name?.trim() || interaction.first_route_id)
const statusLabel = $derived(observationStatusLabel(interaction.status))
const usage = $derived([
  [m.observation_usage_input(), interaction.usage.input_tokens],
  [m.observation_usage_output(), interaction.usage.output_tokens],
  [m.observation_usage_cache_read(), interaction.usage.cache_read_tokens],
  [m.observation_usage_cache_write(), interaction.usage.cache_write_tokens],
  [m.observation_usage_reasoning(), interaction.usage.reasoning_tokens],
] as const)
</script>

<Handle type="target" position={Position.Top} class="observation-handle" aria-hidden="true" />
<article
  class={[
    'interaction-card',
    selected && 'node-selected',
    data.onSelectedPath && 'node-spine',
    data.subdued && 'node-subdued',
  ]}
  aria-label={m.observation_interaction_card_label({ title, status: statusLabel })}>
  <header class="flex min-w-0 items-start justify-between gap-3">
    <div class="min-w-0">
      <time
        class="font-technical block text-[11px] text-muted-foreground"
        datetime={new Date(interaction.started_at).toISOString()}>
        {formatLogTime(interaction.started_at)}
      </time>
      <h3 class="font-structural mt-1 truncate text-base font-semibold">{title}</h3>
    </div>
    <span class="status-label" data-status={interaction.status}>
      <span class="status-dot" aria-hidden="true"></span>{statusLabel}
    </span>
  </header>

  <div class="usage-grid" aria-label={m.observation_confirmed_usage()}>
    {#each usage as item (item[0])}
      <span class="usage-item" title={item[0]}>
        <span>{item[0]}</span>
        <strong title={item[1] == null ? m.observation_usage_unknown() : undefined}
          >{formatCompactCount(item[1])}</strong>
      </span>
    {/each}
  </div>

  <p class={['tail-preview', !interaction.visible_tail && 'text-muted-foreground']}>
    {interaction.visible_tail || m.observation_no_visible_output()}
  </p>

  <footer class="flex items-center justify-between gap-2">
    <Badge variant={interaction.debug_status === 'partial' ? 'destructive' : 'outline'}>
      {m.observation_debug_capture({ status: observationDebugStatusLabel(interaction.debug_status) })}
    </Badge>
    {#if interaction.observation_gap}
      <span class="text-xs font-medium text-destructive">{m.observation_gap()}</span>
    {:else if interaction.matched}
      <span class="text-xs font-medium text-primary">{m.observation_filter_match()}</span>
    {:else}
      <span class="text-xs text-muted-foreground">{m.observation_context_node()}</span>
    {/if}
  </footer>
</article>
<Handle type="source" position={Position.Bottom} class="observation-handle" aria-hidden="true" />

<style>
:global(.observation-handle) {
  width: 1px;
  height: 1px;
  border: 0;
  opacity: 0;
}
.interaction-card {
  width: 18rem;
  height: 11.875rem;
  display: flex;
  flex-direction: column;
  gap: 0.65rem;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--card);
  padding: 0.85rem;
  color: var(--card-foreground);
  box-shadow: var(--shadow-sm);
  transition:
    border-color 120ms ease,
    opacity 120ms ease,
    box-shadow 120ms ease;
}
.node-selected,
.node-spine {
  border-color: var(--primary);
  box-shadow:
    0 0 0 1px color-mix(in oklab, var(--primary) 28%, transparent),
    var(--shadow);
}
.node-subdued {
  opacity: 0.42;
}
.status-label {
  display: inline-flex;
  flex: none;
  align-items: center;
  gap: 0.35rem;
  font-size: 0.7rem;
  font-weight: 600;
}
.status-dot {
  width: 0.48rem;
  height: 0.48rem;
  border-radius: 999px;
  background: var(--muted-foreground);
}
[data-status='running'] .status-dot {
  background: var(--success);
  animation: observation-breathe 1.8s ease-in-out infinite;
}
[data-status='waiting_client'] .status-dot {
  background: var(--warning);
}
.usage-grid {
  display: grid;
  grid-template-columns: repeat(5, minmax(0, 1fr));
  border-block: 1px solid var(--border);
  padding-block: 0.4rem;
}
.usage-item {
  min-width: 0;
  border-inline-start: 1px solid var(--border);
  padding-inline: 0.3rem;
  text-align: center;
  font-family: var(--font-technical);
  font-size: 0.58rem;
  color: var(--muted-foreground);
}
.usage-item:first-child {
  border-inline-start: 0;
}
.usage-item strong {
  display: block;
  overflow: hidden;
  color: var(--foreground);
  font-size: 0.68rem;
  font-weight: 500;
  text-overflow: ellipsis;
}
.tail-preview {
  display: -webkit-box;
  min-height: 3.75rem;
  flex: 1;
  overflow: hidden;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 4;
  line-clamp: 4;
  font-size: 0.75rem;
  line-height: 0.94rem;
  white-space: pre-wrap;
}
@keyframes observation-breathe {
  50% {
    opacity: 0.3;
    transform: scale(0.72);
  }
}
@media (prefers-reduced-motion: reduce) {
  [data-status='running'] .status-dot {
    animation: none;
  }
  .interaction-card {
    transition: none;
  }
}
</style>

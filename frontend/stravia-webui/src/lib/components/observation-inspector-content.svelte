<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onDestroy } from 'svelte'
import { SvelteMap } from 'svelte/reactivity'
import CopyIcon from '@lucide/svelte/icons/copy'
import DownloadIcon from '@lucide/svelte/icons/download'
import XIcon from '@lucide/svelte/icons/x'
import { toast } from 'svelte-sonner'

import { formatDuration, formatLogTime, formatTokenCount } from '$lib/format'
import {
  observationContextStatusLabel,
  observationDebugStatusLabel,
  observationStatusLabel,
} from '$lib/observation-labels'
import type { InteractionDetail, ObservationEvent, RejectionDetail, RunDetail } from '$lib/types'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Empty from '$lib/components/ui/empty'
import * as Tabs from '$lib/components/ui/tabs'
import * as Alert from '$lib/components/ui/alert'

interface Props {
  interaction?: InteractionDetail
  rejection?: RejectionDetail
  loading?: boolean
  activeTab: string
  onclose: () => void
  onbundle: () => void
  onlatest?: () => void
}

let { interaction, rejection, loading = false, activeTab = $bindable(), onclose, onbundle, onlatest }: Props = $props()
const orderedRuns = $derived([...(interaction?.runs ?? [])].sort((a, b) => a.started_at - b.started_at))
const runIds = $derived(new Set(orderedRuns.map((run) => run.id)))

const title = $derived(
  interaction
    ? interaction.interaction.first_model_display_name?.trim() || interaction.interaction.first_route_id
    : rejection
      ? `${rejection.rejection.method} ${rejection.rejection.path}`
      : m.observation_details(),
)
const hasDebug = $derived(
  interaction ? interaction.runs.some((run) => run.debug_enabled) : Boolean(rejection?.rejection.debug_enabled),
)
const debugRecords = $derived.by(() => {
  if (interaction)
    return interaction.runs.flatMap((run) =>
      run.debug_enabled ? run.debug_events.map((value) => ({ runId: run.id, value })) : [],
    )
  return rejection?.rejection.debug_enabled
    ? rejection.debug_events.map((value) => ({ runId: rejection.rejection.id, value }))
    : []
})

interface TimelineNode {
  event: ObservationEvent
  children: TimelineNode[]
}

function eventTree(events: ObservationEvent[]): TimelineNode[] {
  const nodes = [...events]
    .sort((a, b) => a.sequence - b.sequence)
    .map((event) => ({ event, children: [] as TimelineNode[] }))
  const turns = new SvelteMap<string, TimelineNode>()
  const attempts = new SvelteMap<string, TimelineNode>()
  const tools = new SvelteMap<string, TimelineNode>()
  const payloadOf = (event: ObservationEvent): Record<string, unknown> =>
    event.payload && typeof event.payload === 'object' ? (event.payload as Record<string, unknown>) : {}
  for (const node of nodes) {
    const payload = payloadOf(node.event)
    if (node.event.kind === 'model_turn_started') turns.set(String(payload.model_turn_id), node)
    if (node.event.kind === 'target_attempt_started') attempts.set(String(payload.attempt_id), node)
    if (node.event.kind === 'platform_tool_started') tools.set(String(payload.tool_id), node)
  }
  const roots: TimelineNode[] = []
  for (const node of nodes) {
    const payload = payloadOf(node.event)
    const attempt = typeof payload.attempt_id === 'string' ? attempts.get(payload.attempt_id) : undefined
    const tool =
      node.event.kind.startsWith('platform_tool') && typeof payload.tool_id === 'string'
        ? tools.get(payload.tool_id)
        : undefined
    const turn = typeof payload.model_turn_id === 'string' ? turns.get(payload.model_turn_id) : undefined
    const parent = [attempt, tool, turn].find((candidate) => candidate && candidate !== node)
    if (parent) parent.children.push(node)
    else roots.push(node)
  }
  return roots
}

const timelines = $derived(new Map(orderedRuns.map((run) => [run.id, eventTree(run.events)])))

function eventTitle(kind: string): string {
  if (kind === 'compaction_operation') return m.observation_compaction_operation()
  if (kind === 'native_compaction_associated') return m.observation_ancestry_native()
  if (kind === 'retained_tail_associated') return m.observation_retained_tail()
  if (kind === 'generation_associated') return m.observation_ancestry_confirmed()
  return kind
    .split('_')
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ')
}

function usageRows(run: RunDetail): ReadonlyArray<readonly [string, number | null]> {
  return [
    [m.observation_usage_input(), run.usage.input_tokens],
    [m.observation_usage_output(), run.usage.output_tokens],
    [m.observation_usage_cache_read(), run.usage.cache_read_tokens],
    [m.observation_usage_cache_write(), run.usage.cache_write_tokens],
    [m.observation_usage_reasoning(), run.usage.reasoning_tokens],
  ]
}

let mounted = true
onDestroy(() => {
  mounted = false
})

async function copyRecord(value: unknown): Promise<void> {
  try {
    await navigator.clipboard.writeText(JSON.stringify(value, null, 2))
    if (mounted) toast.success(m.observation_event_copied())
  } catch {
    if (mounted) toast.error(m.common_not_copy_clipboard())
  }
}

function downloadRecord(value: unknown, index: number): void {
  const url = URL.createObjectURL(new Blob([JSON.stringify(value, null, 2)], { type: 'application/json' }))
  const anchor = document.createElement('a')
  anchor.href = url
  anchor.download = `observation-event-${index + 1}.json`
  anchor.click()
  URL.revokeObjectURL(url)
}
</script>

{#snippet timeline(nodes: TimelineNode[])}
  <ol class="event-list">
    {#each nodes as node (node.event.sequence)}
      {@const contextStatus = observationContextStatusLabel(node.event.kind, node.event.payload)}
      <li>
        <span class="event-mark" aria-hidden="true"></span>
        <div class="min-w-0 flex-1">
          <div class="flex items-baseline justify-between gap-3">
            <strong>{eventTitle(node.event.kind)}</strong><time class="font-technical text-[11px] text-muted-foreground"
              >{formatLogTime(node.event.occurred_at)}</time>
          </div>
          {#if contextStatus}<Badge variant="outline">{contextStatus}</Badge>{/if}
          {#if node.event.kind === 'retained_tail_associated'}
            <p class="text-xs text-muted-foreground">{m.observation_diagnostic_only()}</p>
          {:else if node.event.kind === 'compaction_operation'}
            <p class="text-xs text-muted-foreground">{m.observation_compaction_unknown_usage()}</p>
          {/if}
          {#if node.event.payload != null}<pre>{JSON.stringify(node.event.payload, null, 2)}</pre>{/if}
          {#if node.children.length}{@render timeline(node.children)}{/if}
        </div>
      </li>
    {/each}
  </ol>
{/snippet}

<header class="flex items-start justify-between gap-3 border-b p-4">
  <div class="min-w-0">
    <p class="font-structural text-xs font-semibold tracking-wider text-primary uppercase">
      {interaction ? m.observation_interaction_details() : m.observation_rejection_details()}
    </p>
    <h2 class="font-structural mt-1 truncate text-xl font-semibold">{title}</h2>
    {#if interaction}<p class="font-technical mt-1 truncate text-xs text-muted-foreground">
        {interaction.interaction.id}
      </p>{/if}
  </div>
  <Button variant="ghost" size="icon" aria-label={m.common_close()} onclick={onclose}><XIcon /></Button>
</header>

{#if onlatest}
  <div class="border-b px-4 py-3">
    <Button variant="outline" class="h-auto w-full justify-start whitespace-normal text-start" onclick={onlatest}
      >{m.observation_moved_to_latest()}</Button>
  </div>
{/if}

{#if loading}
  <div class="grid flex-1 place-items-center">
    <p class="text-sm text-muted-foreground">{m.observation_loading_details()}</p>
  </div>
{:else if !interaction && !rejection}
  <Empty.Root class="flex-1"
    ><Empty.Header><Empty.Title>{m.observation_details_unavailable()}</Empty.Title></Empty.Header></Empty.Root>
{:else}
  <Tabs.Root class="flex min-h-0 flex-1 flex-col" bind:value={activeTab}>
    <div class="flex items-center justify-between gap-3 border-b px-4 py-2">
      <Tabs.List>
        <Tabs.Trigger value="timeline">{m.observation_timeline()}</Tabs.Trigger>
        <Tabs.Trigger value="debug" disabled={!hasDebug}>{m.observation_debug_records()}</Tabs.Trigger>
      </Tabs.List>
      <Button variant="outline" size="sm" onclick={onbundle}
        ><DownloadIcon data-icon="inline-start" />{m.observation_bundle()}</Button>
    </div>
    <Tabs.Content value="timeline" class="min-h-0 flex-1 overflow-y-auto p-4">
      {#if interaction}
        <div class="mb-4 grid grid-cols-2 gap-3 border-b pb-4 text-sm sm:grid-cols-4">
          <div>
            <dt class="text-xs text-muted-foreground">{m.common_status()}</dt>
            <dd class="font-medium">{observationStatusLabel(interaction.interaction.status)}</dd>
          </div>
          <div>
            <dt class="text-xs text-muted-foreground">{m.observation_started()}</dt>
            <dd class="font-technical text-xs">{formatLogTime(interaction.interaction.started_at)}</dd>
          </div>
          <div>
            <dt class="text-xs text-muted-foreground">{m.observation_last_activity()}</dt>
            <dd class="font-technical text-xs">{formatLogTime(interaction.interaction.last_active_at)}</dd>
          </div>
          <div>
            <dt class="text-xs text-muted-foreground">{m.observation_debug()}</dt>
            <dd class="font-medium">{observationDebugStatusLabel(interaction.interaction.debug_status)}</dd>
          </div>
        </div>
        <div class="timeline">
          {#snippet runBranch(parentId: string | null)}
            {#each orderedRuns.filter( (run) => (parentId === null ? !run.parent_run_id || !runIds.has(run.parent_run_id) : run.parent_run_id === parentId) ) as run (run.id)}
              <section class="run-record">
                <header class="run-heading">
                  <div class="min-w-0">
                    <div class="flex flex-wrap items-center gap-2">
                      <h3 class="font-structural font-semibold">{m.observation_inference_run()}</h3>
                      <Badge variant="outline">{observationStatusLabel(run.status)}</Badge>
                      {#if run.user_interrupted}<Badge variant="destructive">{m.observation_user_interrupted()}</Badge
                        >{/if}
                      <Badge variant={run.trace?.status === 'partial' ? 'destructive' : 'secondary'}
                        >{observationDebugStatusLabel(
                          run.debug_enabled ? (run.trace?.status ?? 'missing') : 'none',
                        )}</Badge>
                    </div>
                    <p class="font-technical mt-1 truncate text-xs text-muted-foreground">{run.id}</p>
                    {#if run.parent_run_id}<p class="font-technical mt-1 text-[11px] text-muted-foreground">
                        {m.observation_child_of({ id: run.parent_run_id })}
                      </p>{/if}
                  </div>
                  <time class="font-technical text-xs text-muted-foreground">{formatLogTime(run.started_at)}</time>
                </header>
                <dl class="run-facts">
                  <div>
                    <dt>{m.observation_route()}</dt>
                    <dd>{run.model_display_name || run.route_id}</dd>
                  </div>
                  <div>
                    <dt>{m.observation_protocol()}</dt>
                    <dd>{run.ingress_protocol}</dd>
                  </div>
                  <div>
                    <dt>{m.observation_delivery()}</dt>
                    <dd>{run.client_output_committed ? m.observation_committed() : m.observation_not_committed()}</dd>
                  </div>
                  <div>
                    <dt>{m.observation_duration()}</dt>
                    <dd>{formatDuration(run.finished_at == null ? null : run.finished_at - run.started_at)}</dd>
                  </div>
                </dl>
                <div class="usage-line" aria-label={m.observation_confirmed_usage()}>
                  {#each usageRows(run) as item (item[0])}<span
                      ><small>{item[0]}</small>{item[1] == null
                        ? m.observation_usage_unknown()
                        : formatTokenCount(item[1])}</span
                    >{/each}
                </div>
                {#if run.trace?.status === 'partial' || (run.debug_enabled && !run.trace)}
                  <Alert.Root variant="warning" role="status" class="mx-3 mt-3 w-auto"
                    ><Alert.Description>
                      {m.observation_partial_trace({
                        reasons: run.trace?.reasons.join(', ') || m.observation_trace_missing(),
                      })}
                    </Alert.Description></Alert.Root>
                {/if}
                {@render timeline(timelines.get(run.id) ?? [])}
                <div class="run-children">{@render runBranch(run.id)}</div>
              </section>
            {/each}
          {/snippet}
          {@render runBranch(null)}
        </div>
      {:else if rejection}
        <dl class="rejection-facts">
          <div>
            <dt>{m.observation_request()}</dt>
            <dd class="font-technical">{rejection.rejection.method} {rejection.rejection.path}</dd>
          </div>
          <div>
            <dt>{m.observation_rejected_stage()}</dt>
            <dd>{rejection.rejection.stage}</dd>
          </div>
          <div>
            <dt>{m.observation_error_code()}</dt>
            <dd class="font-technical">{rejection.rejection.code}</dd>
          </div>
          <div>
            <dt>{m.observation_http_status()}</dt>
            <dd>{rejection.rejection.status_code}</dd>
          </div>
          <div>
            <dt>{m.observation_protocol()}</dt>
            <dd>{rejection.rejection.ingress_protocol}</dd>
          </div>
          <div>
            <dt>{m.observation_debug()}</dt>
            <dd>{observationDebugStatusLabel(rejection.rejection.debug_status)}</dd>
          </div>
        </dl>
        {#if rejection.trace?.status === 'partial' || (rejection.rejection.debug_enabled && !rejection.trace)}
          <Alert.Root variant="warning" role="status" class="mx-3 mt-3 w-auto"
            ><Alert.Description>
              {m.observation_partial_trace({
                reasons: rejection.trace?.reasons.join(', ') || m.observation_trace_missing(),
              })}
            </Alert.Description></Alert.Root>
        {/if}
        <ol class="event-list mt-5">
          {#each rejection.events as event (event.sequence)}<li>
              <span class="event-mark" aria-hidden="true"></span>
              <div class="min-w-0 flex-1">
                <div class="flex justify-between gap-3">
                  <strong>{eventTitle(event.kind)}</strong><time class="font-technical text-[11px]"
                    >{formatLogTime(event.occurred_at)}</time>
                </div>
                {#if event.payload != null}<pre>{JSON.stringify(event.payload, null, 2)}</pre>{/if}
              </div>
            </li>{/each}
        </ol>
      {/if}
    </Tabs.Content>
    <Tabs.Content value="debug" class="min-h-0 flex-1 overflow-y-auto p-4">
      <p class="mb-4 text-sm text-muted-foreground">{m.observation_debug_fidelity()}</p>
      <div class="flex flex-col gap-3">
        {#each debugRecords as record, index (`${record.runId}-${index}`)}
          <article class="debug-record">
            <header class="flex items-center justify-between gap-2 border-b px-3 py-2">
              <span class="font-technical truncate text-xs">{record.runId} · {index + 1}</span>
              <div class="flex gap-1">
                <Button
                  variant="ghost"
                  size="icon-sm"
                  aria-label={m.common_copy()}
                  onclick={() => void copyRecord(record.value)}><CopyIcon /></Button
                ><Button
                  variant="ghost"
                  size="icon-sm"
                  aria-label={m.observation_download_event()}
                  onclick={() => downloadRecord(record.value, index)}><DownloadIcon /></Button>
              </div>
            </header>
            <pre>{JSON.stringify(record.value, null, 2)}</pre>
          </article>
        {:else}
          <Empty.Root
            ><Empty.Header><Empty.Title>{m.observation_no_debug_records()}</Empty.Title></Empty.Header></Empty.Root>
        {/each}
      </div>
    </Tabs.Content>
  </Tabs.Root>
{/if}

<style>
.timeline {
  display: flex;
  flex-direction: column;
  gap: 1rem;
}
.run-record {
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--card);
}
.run-children {
  display: grid;
  gap: 0.75rem;
  margin-inline-start: 0.75rem;
}
.run-heading {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 1rem;
  border-bottom: 1px solid var(--border);
  padding: 0.8rem;
}
.run-facts,
.rejection-facts {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 0.65rem 1rem;
  padding: 0.8rem;
  font-size: 0.75rem;
}
.run-facts dt,
.rejection-facts dt {
  color: var(--muted-foreground);
}
.run-facts dd,
.rejection-facts dd {
  overflow-wrap: anywhere;
  font-weight: 500;
}
.usage-line {
  display: grid;
  grid-template-columns: repeat(5, minmax(0, 1fr));
  border-block: 1px solid var(--border);
  padding: 0.55rem 0.8rem;
  font-family: var(--font-technical);
  font-size: 0.7rem;
}
.usage-line span {
  border-inline-start: 1px solid var(--border);
  padding-inline: 0.45rem;
}
.usage-line span:first-child {
  border-inline-start: 0;
  padding-inline-start: 0;
}
.usage-line small {
  display: block;
  color: var(--muted-foreground);
}
.event-list {
  padding: 0.8rem;
}
.event-list li {
  display: flex;
  margin-inline-start: calc(var(--event-depth, 0) * 1.25rem);
  gap: 0.65rem;
  border-inline-start: 1px solid var(--border);
  padding: 0 0 1rem 0.75rem;
  font-size: 0.75rem;
}
.event-mark {
  width: 0.45rem;
  height: 0.45rem;
  flex: none;
  transform: translate(-1rem, 0.32rem);
  border: 1px solid var(--primary);
  border-radius: 999px;
  background: var(--background);
}
pre {
  max-height: 18rem;
  overflow: auto;
  margin-top: 0.4rem;
  border-radius: calc(var(--radius) - 2px);
  background: var(--muted);
  padding: 0.6rem;
  font-family: var(--font-technical);
  font-size: 0.68rem;
  line-height: 1.4;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.debug-record {
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
.debug-record > pre {
  max-height: 32rem;
  margin: 0;
  border-radius: 0 0 calc(var(--radius) - 1px) calc(var(--radius) - 1px);
}
@media (max-width: 767px) {
  .usage-line {
    grid-template-columns: repeat(3, minmax(0, 1fr));
    row-gap: 0.55rem;
  }
}
</style>

<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onDestroy } from 'svelte'
import CopyIcon from '@lucide/svelte/icons/copy'
import DownloadIcon from '@lucide/svelte/icons/download'
import XIcon from '@lucide/svelte/icons/x'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import { toast } from 'svelte-sonner'

import { formatDuration, formatLogTime, formatTime, formatTokenCount } from '$lib/format'
import ObservationConversation from '$lib/components/observation-conversation.svelte'
import { observationDebugStatusLabel, observationStatusLabel } from '$lib/observation-labels'
import { observationAttemptOutputTokens, observationEventSummary } from '$lib/observation-event-summary'
import type { InteractionDetail, ObservationEvent, RejectionDetail, RunDetail } from '$lib/types'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Empty from '$lib/components/ui/empty'
import * as Tabs from '$lib/components/ui/tabs'
import * as Alert from '$lib/components/ui/alert'
import * as Collapsible from '$lib/components/ui/collapsible'

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
    ? orderedRuns.at(-1)?.model_display_name?.trim() ||
        orderedRuns.at(-1)?.route_id ||
        interaction.interaction.first_model_display_name?.trim() ||
        interaction.interaction.first_route_id
    : rejection
      ? m.observation_request_failed()
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

function orderedEvents(events: ObservationEvent[]): ObservationEvent[] {
  // 因果子树会把晚发生的完成事件提前；阅读时间线按时间排序，原始关联仍保留在 payload。
  return events.toSorted((a, b) => a.occurred_at - b.occurred_at || a.sequence - b.sequence)
}

const timelines = $derived(new Map(orderedRuns.map((run) => [run.id, orderedEvents(run.events)])))
const attemptOutputs = $derived(new Map(orderedRuns.map((run) => [run.id, observationAttemptOutputTokens(run.events)])))

function toolName(event: ObservationEvent): string | null {
  const payload = event.payload
  if (event.kind !== 'client_tool_handoff' || !payload || typeof payload !== 'object' || !('name' in payload))
    return null
  return typeof payload.name === 'string' && payload.name.trim() ? payload.name : null
}

function eventGroups(events: ObservationEvent[], compact: boolean): ObservationEvent[][] {
  const groups: ObservationEvent[][] = []
  for (const event of events) {
    const previous = groups.at(-1)
    // 只合并时间线上相邻的输出增量或同名工具交接，不跨越其他事件。
    const name = toolName(event)
    if (
      compact &&
      previous?.[0].kind === event.kind &&
      (event.kind === 'client_visible_content_delta' || (name !== null && name === toolName(previous[0])))
    ) {
      previous.push(event)
    } else {
      groups.push([event])
    }
  }
  return groups
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

{#snippet timeline(events: ObservationEvent[], compact = true, outputs?: ReadonlyMap<string, number | null>)}
  <ol class="event-list">
    {#each eventGroups(events, compact) as group (group[0].sequence)}
      {#if group.length > 1}
        <li>
          <span class="event-mark" aria-hidden="true"></span>
          <Collapsible.Root class="min-w-0 flex-1">
            <div class="event-heading">
              <Collapsible.Trigger class="diagnostic-trigger event-group-trigger">
                <ChevronRightIcon size={14} aria-hidden="true" />
                {group[0].kind === 'client_visible_content_delta'
                  ? m.observation_response_updates({ count: group.length })
                  : m.observation_tool_handoffs({
                      tool: toolName(group[0]) ?? m.observation_event_tool(),
                      count: group.length,
                    })}
              </Collapsible.Trigger>
              <time class="font-technical text-xs text-muted-foreground">
                {formatTime(group[0].occurred_at)}–{formatTime(group[group.length - 1].occurred_at)}
              </time>
            </div>
            <Collapsible.Content>
              {@render timeline(group, false, outputs)}
            </Collapsible.Content>
          </Collapsible.Root>
        </li>
      {:else}
        {@const event = group[0]}
        {@const summary = observationEventSummary(event, outputs)}
        <li>
          <span class="event-mark" data-tone={summary.tone} aria-hidden="true"></span>
          <div class="min-w-0 flex-1">
            <Collapsible.Root>
              <div class="event-heading">
                <strong>{summary.title}</strong>
                <div class="event-actions">
                  <time class="font-technical text-xs text-muted-foreground" title={formatLogTime(event.occurred_at)}
                    >{formatTime(event.occurred_at)}</time>
                  <Collapsible.Trigger
                    class="diagnostic-trigger event-raw-trigger"
                    aria-label={m.observation_raw_event()}
                    title={m.observation_raw_event()}>
                    <ChevronRightIcon size={14} aria-hidden="true" />
                  </Collapsible.Trigger>
                </div>
              </div>
              {#if summary.facts.length}
                <dl class="event-facts">
                  {#each summary.facts as fact (fact.label)}
                    <div>
                      <dt>{fact.label}</dt>
                      <dd>{fact.value}</dd>
                    </div>
                  {/each}
                </dl>
              {/if}
              {#if summary.note}<p class="event-note">{summary.note}</p>{/if}
              <Collapsible.Content>
                <p class="event-kind">{m.observation_event_type()}: <code>{event.kind}</code></p>
                <pre>{JSON.stringify(event.payload, null, 2)}</pre>
              </Collapsible.Content>
            </Collapsible.Root>
          </div>
        </li>
      {/if}
    {/each}
  </ol>
{/snippet}

<header class="flex items-start justify-between gap-3 border-b p-4">
  <div class="min-w-0">
    <p class="font-structural text-xs font-semibold tracking-wider text-primary uppercase">
      {interaction ? m.observation_interaction_details() : m.observation_rejection_details()}
    </p>
    <h2 class="font-structural mt-1 truncate text-xl font-semibold">{title}</h2>
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
    <div class="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-2">
      <Tabs.List class="h-auto flex-wrap">
        <Tabs.Trigger value="timeline">{m.observation_conversation()}</Tabs.Trigger>
        <Tabs.Trigger value="diagnostics">{m.observation_diagnostics()}</Tabs.Trigger>
        {#if hasDebug}
          <Tabs.Trigger value="debug">{m.observation_debug_records()}</Tabs.Trigger>
        {/if}
      </Tabs.List>
      <Button variant="outline" size="sm" onclick={onbundle}
        ><DownloadIcon data-icon="inline-start" />{m.observation_bundle()}</Button>
    </div>
    <Tabs.Content value="timeline" class="min-h-0 flex-1 overflow-hidden">
      {#if interaction}
        {#key interaction.interaction.id}
          <ObservationConversation detail={interaction} />
        {/key}
      {:else if rejection}
        <div class="p-4">
          <Alert.Root variant="destructive">
            <Alert.Title>{m.observation_request_failed()}</Alert.Title>
            <Alert.Description
              >{m.observation_rejection_summary({ status: rejection.rejection.status_code })}</Alert.Description>
          </Alert.Root>
        </div>
      {/if}
    </Tabs.Content>
    <Tabs.Content value="diagnostics" class="min-h-0 flex-1 overflow-y-auto p-4">
      {#if interaction}
        <dl class="diagnostic-overview">
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
        </dl>
        <Collapsible.Root class="mb-4">
          <Collapsible.Trigger class="diagnostic-trigger">
            <ChevronRightIcon size={14} aria-hidden="true" />
            {m.observation_identifiers()}
          </Collapsible.Trigger>
          <Collapsible.Content>
            <dl class="identifier-facts">
              <div>
                <dt>{m.observation_interaction_id()}</dt>
                <dd>{interaction.interaction.id}</dd>
              </div>
            </dl>
          </Collapsible.Content>
        </Collapsible.Root>
        <div class="timeline">
          {#snippet runBranch(parentId: string | null)}
            {#each orderedRuns.filter( (run) => (parentId === null ? !run.parent_run_id || !runIds.has(run.parent_run_id) : run.parent_run_id === parentId) ) as run (run.id)}
              <section class="run-record">
                <header class="run-heading">
                  <div class="min-w-0">
                    <div class="flex flex-wrap items-center gap-2">
                      <h3 class="font-structural font-semibold">{run.model_display_name || run.route_id}</h3>
                      <Badge variant="outline">{observationStatusLabel(run.status)}</Badge>
                      {#if run.user_interrupted}<Badge variant="destructive">{m.observation_user_interrupted()}</Badge
                        >{/if}
                      {#if run.debug_enabled}
                        <Badge variant={run.trace?.status === 'partial' ? 'destructive' : 'secondary'}
                          >{observationDebugStatusLabel(run.trace?.status ?? 'missing')}</Badge>
                      {/if}
                    </div>
                  </div>
                  <time class="font-technical text-xs text-muted-foreground">{formatLogTime(run.started_at)}</time>
                </header>
                <dl class="run-facts">
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
                <Collapsible.Root class="px-3">
                  <Collapsible.Trigger class="diagnostic-trigger">
                    <ChevronRightIcon size={14} aria-hidden="true" />
                    {m.observation_identifiers()}
                  </Collapsible.Trigger>
                  <Collapsible.Content>
                    <dl class="identifier-facts">
                      <div>
                        <dt>{m.observation_run_id()}</dt>
                        <dd>{run.id}</dd>
                      </div>
                      <div>
                        <dt>{m.observation_route()}</dt>
                        <dd>{run.route_id}</dd>
                      </div>
                    </dl>
                    {#if run.parent_run_id}
                      <p class="event-kind">{m.observation_child_of({ id: run.parent_run_id })}</p>
                    {/if}
                  </Collapsible.Content>
                </Collapsible.Root>
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
                {@render timeline(timelines.get(run.id) ?? [], true, attemptOutputs.get(run.id))}
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
        {@render timeline(orderedEvents(rejection.events))}
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
.diagnostic-overview {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(8rem, 1fr));
  gap: 0.75rem;
  border-bottom: 1px solid var(--border);
  padding-bottom: 1rem;
  font-size: 0.875rem;
}
:global(.diagnostic-trigger) {
  display: inline-flex;
  min-height: 40px;
  align-items: center;
  gap: 0.375rem;
  border-radius: var(--radius-sm);
  padding: 0.25rem 0.375rem;
  color: var(--muted-foreground);
  font-size: 0.75rem;
  text-align: start;
  cursor: pointer;
}
:global(.diagnostic-trigger:hover) {
  background: var(--accent);
  color: var(--accent-foreground);
}
:global(.diagnostic-trigger svg) {
  flex: none;
  transition: transform 140ms cubic-bezier(0.2, 0, 0, 1);
}
:global(.diagnostic-trigger[data-state='open'] svg) {
  transform: rotate(90deg);
}
.event-heading {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 0.25rem 0.75rem;
}
.event-actions {
  display: flex;
  align-items: center;
  gap: 0.25rem;
}
:global(.event-raw-trigger) {
  width: 40px;
  justify-content: center;
}
:global(.event-group-trigger) {
  min-width: 0;
  color: var(--foreground);
  font-size: 0.875rem;
  font-weight: 600;
}
.event-heading strong {
  font-size: 0.875rem;
  overflow-wrap: anywhere;
}
.event-heading time {
  flex: none;
  font-variant-numeric: tabular-nums;
}
.event-facts {
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 1rem;
  margin-top: 0.375rem;
}
.event-facts > div {
  display: flex;
  align-items: baseline;
  gap: 0.5rem;
  min-width: 0;
}
.event-facts dt,
.identifier-facts dt {
  flex: none;
  color: var(--muted-foreground);
}
.event-facts dd,
.identifier-facts dd {
  min-width: 0;
  overflow-wrap: anywhere;
}
.event-note {
  margin-top: 0.375rem;
  color: var(--muted-foreground);
  line-height: 1.6;
  text-wrap: pretty;
  overflow-wrap: anywhere;
}
.event-kind,
.identifier-facts {
  margin-bottom: 0.5rem;
  font-size: 0.75rem;
  overflow-wrap: anywhere;
}
.identifier-facts {
  display: grid;
  gap: 0.5rem;
}
.identifier-facts dd,
.event-kind code {
  font-family: var(--font-technical);
}
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
  flex-wrap: wrap;
  align-items: flex-start;
  justify-content: space-between;
  gap: 1rem;
  border-bottom: 1px solid var(--border);
  padding: 0.8rem;
}
.run-facts,
.rejection-facts {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 8rem), 1fr));
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
  gap: 0.65rem;
  border-inline-start: 1px solid var(--border);
  padding: 0 0 0.5rem 0.75rem;
  font-size: 0.75rem;
}
.event-list .event-list {
  padding: 0.75rem 0 0;
}
.event-mark {
  width: 0.45rem;
  height: 0.45rem;
  flex: none;
  transform: translate(-1rem, 1rem);
  border: 1px solid var(--primary);
  border-radius: 999px;
  background: var(--background);
}
.event-mark[data-tone='success'] {
  border-color: var(--success);
  background: var(--success);
}
.event-mark[data-tone='warning'] {
  height: 0.2rem;
  border-color: var(--warning);
  border-radius: 0;
  background: var(--warning);
}
.event-mark[data-tone='error'] {
  transform: translate(-1rem, 1rem) rotate(45deg);
  border-color: var(--destructive);
  border-radius: 0;
  background: var(--destructive);
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

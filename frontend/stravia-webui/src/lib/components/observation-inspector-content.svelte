<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import DownloadIcon from '@lucide/svelte/icons/download'
import XIcon from '@lucide/svelte/icons/x'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'

import { formatDuration, formatList, formatLogTime, formatNumber, formatTime, formatTokenCount } from '$lib/format'
import ObservationConversation from '$lib/components/observation-conversation.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import TechnicalValue from '$lib/components/technical-value.svelte'
import { observationDebugStatusLabel, observationStatusLabel } from '$lib/observation-labels'
import {
  observationAttemptOutputTokens,
  observationEventSummary,
  observationStatusTone,
} from '$lib/observation-event-summary'
import type {
  InteractionDetail,
  LiveContentBlock,
  ObservationEvent,
  RunDetail,
  FailedRequestDetail,
} from '$lib/types'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Empty from '$lib/components/ui/empty'
import * as Tabs from '$lib/components/ui/tabs'
import * as Alert from '$lib/components/ui/alert'
import * as Collapsible from '$lib/components/ui/collapsible'

interface Props {
  interaction?: InteractionDetail
  failure?: FailedRequestDetail
  error?: string
  onretry?: () => void
  oninteraction?: () => void
  liveBlocks?: LiveContentBlock[]
  liveGap?: boolean
  liveCapacity?: boolean
  olderLoading?: boolean
  onolder?: () => Promise<void>
  loading?: boolean
  activeTab: string
  onclose: () => void
  onbundle: () => void
  onlatest?: () => void
}

let {
  interaction,
  failure,
  error,
  onretry,
  oninteraction,
  liveBlocks = [],
  liveGap = false,
  liveCapacity = false,
  olderLoading = false,
  onolder,
  loading = false,
  activeTab = $bindable(),
  onclose,
  onbundle,
  onlatest,
}: Props = $props()
const orderedRuns = $derived([...(interaction?.runs ?? [])].sort((a, b) => a.started_at - b.started_at))
const runIds = $derived(new Set(orderedRuns.map((run) => run.id)))

const title = $derived(
  interaction
    ? orderedRuns.at(-1)?.model_display_name?.trim() ||
        orderedRuns.at(-1)?.route_id ||
        interaction.interaction.first_model_display_name?.trim() ||
        interaction.interaction.first_route_id
    : failure
      ? m.observation_request_failed()
      : m.observation_details(),
)
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

// 过程层事件：诊断关联、捕获与增量记录不承载主流程叙事；带成败或警告语义时仍留在主干。
const PROCESS_KINDS = new Set([
  'generation_associated',
  'retained_tail_associated',
  'native_compaction_associated',
  'input_preview_recorded',
  'credential_mappings_created',
  'checkpoint',
  'wire',
  'trace_manifest_updated',
  'usage_confirmed',
  'client_visible_content_delta',
  'model_thinking_delta',
])

type StreamItem =
  | { type: 'event'; event: ObservationEvent }
  | { type: 'tools'; name: string | null; events: ObservationEvent[] }
  | { type: 'process'; events: ObservationEvent[] }

function streamItems(
  events: readonly ObservationEvent[],
  outputs?: ReadonlyMap<string, number | null>,
): StreamItem[] {
  const items: StreamItem[] = []
  for (const event of events) {
    const process =
      PROCESS_KINDS.has(event.kind) && observationEventSummary(event, outputs).tone === 'neutral'
    const name = toolName(event)
    const previous = items.at(-1)
    if (process) {
      if (previous?.type === 'process') previous.events.push(event)
      else items.push({ type: 'process', events: [event] })
    } else if (name !== null) {
      if (previous?.type === 'tools' && previous.name === name) previous.events.push(event)
      else items.push({ type: 'tools', name, events: [event] })
    } else {
      items.push({ type: 'event', event })
    }
  }
  return items
}

const streams = $derived(
  new Map(
    orderedRuns.map((run) => [run.id, streamItems(timelines.get(run.id) ?? [], attemptOutputs.get(run.id))]),
  ),
)
const failureItems = $derived(failure ? streamItems(orderedEvents(failure.events)) : [])

const baseTime = $derived(interaction?.interaction.started_at ?? failure?.request.started_at ?? null)

function offsetLabel(at: number): string {
  if (baseTime == null) return formatTime(at)
  const delta = at - baseTime
  return delta >= 0 ? `+${formatDuration(delta)}` : `−${formatDuration(-delta)}`
}

function childRuns(parentId: string | null): RunDetail[] {
  return orderedRuns.filter((run) =>
    parentId === null ? !run.parent_run_id || !runIds.has(run.parent_run_id) : run.parent_run_id === parentId,
  )
}

function itemKey(item: StreamItem): number {
  return item.type === 'event' ? item.event.sequence : item.events[0].sequence
}

// 同种过程事件合并为「标题 × N」，混合过程事件显示计数并附种类名帮助扫描。
function processGroup(events: readonly ObservationEvent[]): { label: string; titles: string | null } {
  const titles = [...new Set(events.map((event) => observationEventSummary(event).title))]
  if (titles.length === 1) {
    return { label: m.observation_event_group({ title: titles[0], count: events.length }), titles: null }
  }
  return {
    label: m.observation_process_events({ count: events.length }),
    titles: titles.length <= 3 ? formatList(titles) : `${formatList(titles.slice(0, 3))}…`,
  }
}

function usageText(value: number | null): string {
  return value == null ? m.observation_usage_unknown() : formatNumber(value)
}

function usageRows(run: RunDetail): ReadonlyArray<readonly [string, number | null]> {
  return [
    [m.observation_event_tokens_input(), run.usage.input_tokens],
    [m.observation_event_tokens_output(), run.usage.output_tokens],
    [m.observation_event_tokens_cache_read(), run.usage.cache_read_tokens],
    [m.observation_event_tokens_cache_write(), run.usage.cache_write_tokens],
  ]
}
</script>

{#snippet eventRow(event: ObservationEvent, outputs?: ReadonlyMap<string, number | null>)}
  {@const summary = observationEventSummary(event, outputs)}
  <li class="stream-item">
    <Collapsible.Root>
      <Collapsible.Trigger class="stream-row" data-tone={summary.tone} data-sequence={event.sequence}>
        <span class="stream-text">
          <strong>{summary.title}</strong>
          {#each summary.facts as fact (fact.label)}
            <span class="fact"><span class="fact-label">{fact.label}</span><span class="fact-value">{fact.value}</span></span>
          {/each}
        </span>
        <time class="stream-time" title={formatLogTime(event.occurred_at)}>{offsetLabel(event.occurred_at)}</time>
        <ChevronRightIcon size={14} class="stream-chev" aria-hidden="true" />
      </Collapsible.Trigger>
      {#if summary.note}<p class="event-note">{summary.note}</p>{/if}
      <Collapsible.Content>
        <div class="row-detail">
          <dl class="detail-grid">
            <div>
              <dt>{m.observation_event_type()}</dt>
              <dd class="event-kind"><code>{event.kind}</code></dd>
            </div>
            <div>
              <dt>{m.observation_event_sequence()}</dt>
              <dd class="font-technical">{event.sequence}</dd>
            </div>
            <div>
              <dt>{m.failed_request_time()}</dt>
              <dd>{formatLogTime(event.occurred_at)}</dd>
            </div>
          </dl>
          <pre>{JSON.stringify(event.payload, null, 2)}</pre>
        </div>
      </Collapsible.Content>
    </Collapsible.Root>
  </li>
{/snippet}

{#snippet streamItem(item: StreamItem, outputs?: ReadonlyMap<string, number | null>)}
  {#if item.type === 'event'}
    {@render eventRow(item.event, outputs)}
  {:else if item.events.length === 1}
    {@render eventRow(item.events[0], outputs)}
  {:else}
    {@const group = item.type === 'process' ? processGroup(item.events) : null}
    <li class="stream-item stream-group" data-group={item.type}>
      <Collapsible.Root>
        <Collapsible.Trigger class="stream-row" data-tone="neutral" data-group={item.type}>
          <span class="stream-text">
            <strong
              >{item.type === 'tools'
                ? m.observation_tool_handoffs({
                    tool: item.name ?? m.observation_event_tool(),
                    count: item.events.length,
                  })
                : group?.label}</strong>
            {#if group?.titles}<span class="group-titles">{group.titles}</span>{/if}
          </span>
          <time
            class="stream-time"
            title="{formatLogTime(item.events[0].occurred_at)} – {formatLogTime(
              item.events[item.events.length - 1].occurred_at,
            )}">{offsetLabel(item.events[0].occurred_at)}</time>
          <ChevronRightIcon size={14} class="stream-chev" aria-hidden="true" />
        </Collapsible.Trigger>
        <Collapsible.Content>
          <ol class="branch">
            {#each item.events as event (event.sequence)}
              {@render eventRow(event, outputs)}
            {/each}
          </ol>
        </Collapsible.Content>
      </Collapsible.Root>
    </li>
  {/if}
{/snippet}

{#snippet runBlock(run: RunDetail)}
  {@const items = streams.get(run.id) ?? []}
  {@const children = childRuns(run.id)}
  {@const duration = run.finished_at == null ? null : run.finished_at - run.started_at}
  <li class="stream-run">
    <Collapsible.Root>
      <Collapsible.Trigger
        class="stream-row run-head"
        data-tone={observationStatusTone(run.status)}
        data-run={run.id}>
        <span class="stream-text">
          <strong class="font-structural">{run.model_display_name?.trim() || run.route_id}</strong>
          <Badge variant="outline">{observationStatusLabel(run.status)}</Badge>
          {#if run.user_interrupted}<Badge variant="destructive">{m.observation_user_interrupted()}</Badge>{/if}
          {#if run.debug_enabled}
            <Badge variant={run.trace?.status === 'partial' ? 'destructive' : 'secondary'}
              >{observationDebugStatusLabel(run.trace?.status ?? 'missing')}</Badge>
          {/if}
          <span class="run-stats"
            >{#if duration != null}{formatDuration(duration)} · {/if}{m.observation_usage_input()}
            {formatTokenCount(run.usage.input_tokens)} · {m.observation_usage_output()}
            {formatTokenCount(run.usage.output_tokens)}</span>
        </span>
        <time class="stream-time" title={formatLogTime(run.started_at)}>{offsetLabel(run.started_at)}</time>
        <ChevronRightIcon size={14} class="stream-chev" aria-hidden="true" />
      </Collapsible.Trigger>
      <Collapsible.Content>
        <dl class="row-detail detail-grid">
          <div>
            <dt>{m.observation_run_id()}</dt>
            <dd><TechnicalValue value={run.id} copyable /></dd>
          </div>
          <div>
            <dt>{m.observation_route()}</dt>
            <dd><TechnicalValue value={run.route_id} copyable /></dd>
          </div>
          <div>
            <dt>{m.failed_request_time()}</dt>
            <dd>{formatLogTime(run.started_at)}</dd>
          </div>
          <div>
            <dt>{m.observation_duration()}</dt>
            <dd>{formatDuration(duration)}</dd>
          </div>
          <div>
            <dt>{m.observation_protocol()}</dt>
            <dd class="font-technical">{run.ingress_protocol}</dd>
          </div>
          <div>
            <dt>{m.observation_delivery()}</dt>
            <dd>{run.client_output_committed ? m.observation_committed() : m.observation_not_committed()}</dd>
          </div>
          {#each usageRows(run) as row (row[0])}
            <div>
              <dt>{row[0]}</dt>
              <dd class="font-technical">{usageText(row[1])}</dd>
            </div>
          {/each}
          {#if run.parent_run_id}
            <div>
              <dt>{m.observation_parent_run()}</dt>
              <dd><TechnicalValue value={run.parent_run_id} copyable /></dd>
            </div>
          {/if}
        </dl>
      </Collapsible.Content>
    </Collapsible.Root>
    {#if run.trace?.status === 'partial' || (run.debug_enabled && !run.trace)}
      <Alert.Root variant="warning" role="status" class="stream-alert"
        ><Alert.Description>
          {m.observation_partial_trace({
            reasons: run.trace?.reasons.join(', ') || m.observation_trace_missing(),
          })}
        </Alert.Description></Alert.Root>
    {/if}
    {#if items.length || children.length}
      <ol class="branch">
        {#each items as item (itemKey(item))}
          {@render streamItem(item, attemptOutputs.get(run.id))}
        {/each}
        {#each children as child (child.id)}
          {@render runBlock(child)}
        {/each}
      </ol>
    {/if}
  </li>
{/snippet}

<header class="flex items-start justify-between gap-3 border-b p-4">
  <div class="min-w-0">
    <p class="font-structural text-xs font-semibold tracking-wider text-primary uppercase">
      {interaction ? m.observation_interaction_details() : m.observation_failed_request_details()}
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

{#if liveCapacity}
  <Alert.Root><Alert.Title>{m.observation_live_capacity()}</Alert.Title></Alert.Root>
{/if}

{#if liveGap}
  <Alert.Root variant="destructive"><Alert.Title>{m.observation_live_save_failed()}</Alert.Title></Alert.Root>
{/if}

{#if loading}
  <div class="grid flex-1 place-items-center">
    <p class="text-sm text-muted-foreground">{m.observation_loading_details()}</p>
  </div>
{:else if error}
  <RequestFailure message={error} retry={onretry} />
{:else if failure}
  <div class="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-4">
    <Alert.Root variant="destructive">
      <Alert.Title>{failure.request.error.code ?? m.observation_request_failed()}</Alert.Title>
      <Alert.Description>
        <p class="whitespace-pre-wrap break-words">
          {failure.request.error.message ?? m.observation_failure_diagnostic_missing()}
        </p>
        {#if failure.request.error.status_code !== null}<p>HTTP {failure.request.error.status_code}</p>{/if}
      </Alert.Description>
    </Alert.Root>
    <dl class="detail-grid failure-facts">
      <div>
        <dt>{m.failed_request_request_id()}</dt>
        <dd>{failure.request.request_id}</dd>
      </div>
      <div>
        <dt>{m.failed_request_origin()}</dt>
        <dd>
          {failure.request.error.source === 'platform'
            ? m.failed_request_platform()
            : failure.request.error.source === 'upstream'
              ? m.failed_request_upstream()
              : '—'}
        </dd>
      </div>
      <div>
        <dt>{m.failed_request_time()}</dt>
        <dd>{formatLogTime(failure.request.started_at)}</dd>
      </div>
      <div>
        <dt>{m.observation_duration()}</dt>
        <dd>{failure.request.duration_ms === null ? '—' : formatDuration(failure.request.duration_ms)}</dd>
      </div>
      <div>
        <dt>{m.observation_debug()}</dt>
        <dd>{observationDebugStatusLabel(failure.request.debug_status)}</dd>
      </div>
    </dl>
    {#if failure.request.observation_gap}
      <p role="status">{m.observation_failure_diagnostic_missing()}</p>
    {/if}
    {#each failure.trace?.reasons ?? [] as reason (reason)}
      <p class="whitespace-pre-wrap break-words">{reason}</p>
    {/each}
    <div class="flex flex-wrap gap-3">
      {#if oninteraction}<Button variant="outline" onclick={oninteraction}>{m.observation_open_interaction()}</Button
        >{/if}
      <Button variant="outline" onclick={onbundle}
        ><DownloadIcon data-icon="inline-start" />{m.observation_bundle()}</Button>
    </div>
    <h3 class="font-structural font-semibold">{m.observation_diagnostics()}</h3>
    <ol class="stream">
      {#each failureItems as item (itemKey(item))}
        {@render streamItem(item)}
      {/each}
    </ol>
  </div>
{:else if !interaction}
  <Empty.Root class="flex-1"
    ><Empty.Header><Empty.Title>{m.observation_details_unavailable()}</Empty.Title></Empty.Header></Empty.Root>
{:else}
  <Tabs.Root class="flex min-h-0 flex-1 flex-col" bind:value={activeTab}>
    <div class="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-2">
      <Tabs.List class="h-auto flex-wrap">
        <Tabs.Trigger value="timeline">{m.observation_conversation()}</Tabs.Trigger>
        <Tabs.Trigger value="diagnostics">{m.observation_diagnostics()}</Tabs.Trigger>
      </Tabs.List>
      <Button variant="outline" size="sm" onclick={onbundle}
        ><DownloadIcon data-icon="inline-start" />{m.observation_bundle()}</Button>
    </div>
    <Tabs.Content value="timeline" class="min-h-0 flex-1 overflow-hidden">
      {#if interaction}
        {#key interaction.interaction.id}
          <ObservationConversation detail={interaction} {liveBlocks} {olderLoading} {onolder} />
        {/key}
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
          <div>
            <dt class="text-xs text-muted-foreground">{m.observation_interaction_id()}</dt>
            <dd><TechnicalValue value={interaction.interaction.id} copyable /></dd>
          </div>
        </dl>
        <ol class="stream">
          {#each childRuns(null) as run (run.id)}
            {@render runBlock(run)}
          {/each}
        </ol>
      {/if}
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
  margin-bottom: 1rem;
  font-size: 0.875rem;
}
.diagnostic-overview dd {
  min-width: 0;
}
/* 诊断流是一条带主干的列表：每个嵌套 ol 的左边线是该层级的引导线，
   行内标记通过 ::before 落在各自层级的引导线上。 */
.stream,
.branch {
  display: flex;
  flex-direction: column;
  margin: 0;
  margin-inline-start: 0.4rem;
  border-inline-start: 1px solid var(--border);
  padding: 0;
  padding-inline-start: 1rem;
  list-style: none;
}
.stream {
  gap: 0.9rem;
}
.branch {
  gap: 0.15rem;
  margin-block-start: 0.15rem;
}
.stream-item,
.stream-run {
  min-width: 0;
}
.branch > .stream-run {
  margin-block-start: 0.5rem;
}
:global(.stream-row) {
  position: relative;
  display: flex;
  width: 100%;
  min-height: 40px;
  align-items: center;
  gap: 0.5rem;
  border-radius: var(--radius-md);
  padding: 0.3rem 0.5rem;
  color: var(--foreground);
  font-size: 0.8125rem;
  line-height: 1.45;
  text-align: start;
  cursor: pointer;
}
:global(.stream-row:hover) {
  background: var(--accent);
}
:global(.stream-row)::before {
  content: '';
  position: absolute;
  inset-inline-start: calc(-1rem - 1px - 0.25rem);
  top: 50%;
  width: 0.5rem;
  height: 0.5rem;
  translate: 0 -50%;
  border: 1px solid var(--primary);
  border-radius: 999px;
  background: var(--background);
}
:global(.stream-row[data-tone='success'])::before {
  border-color: var(--success);
  background: var(--success);
}
:global(.stream-row[data-tone='warning'])::before {
  height: 0.2rem;
  border-color: var(--warning);
  border-radius: 0;
  background: var(--warning);
}
:global(.stream-row[data-tone='error'])::before {
  rotate: 45deg;
  border-color: var(--destructive);
  border-radius: 0;
  background: var(--destructive);
}
:global(.run-head)::before {
  inset-inline-start: calc(-1rem - 1px - 0.3rem);
  width: 0.6rem;
  height: 0.6rem;
}
.stream-item[data-group='process'] > :global(.stream-row)::before {
  border-color: var(--muted-foreground);
}
.stream-text {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 0.15rem 0.6rem;
  overflow-wrap: anywhere;
}
.stream-text strong {
  font-weight: 600;
}
.fact {
  display: inline-flex;
  gap: 0.3rem;
  font-size: 0.75rem;
}
.fact-label {
  color: var(--muted-foreground);
}
.group-titles {
  min-width: 0;
  overflow: hidden;
  color: var(--muted-foreground);
  font-size: 0.75rem;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.run-stats {
  color: var(--muted-foreground);
  font-family: var(--font-technical);
  font-size: 0.72rem;
  font-variant-numeric: tabular-nums;
}
.stream-time {
  flex: none;
  margin-inline-start: auto;
  color: var(--muted-foreground);
  font-family: var(--font-technical);
  font-size: 0.72rem;
  font-variant-numeric: tabular-nums;
}
:global(.stream-chev) {
  flex: none;
  color: var(--muted-foreground);
  transition: transform 140ms cubic-bezier(0.2, 0, 0, 1);
}
:global(.stream-row[data-state='open'] .stream-chev) {
  transform: rotate(90deg);
}
.event-note {
  margin: 0.1rem 0.5rem 0.4rem;
  color: var(--muted-foreground);
  font-size: 0.75rem;
  line-height: 1.6;
  text-wrap: pretty;
  overflow-wrap: anywhere;
}
.row-detail {
  padding: 0.15rem 0.5rem 0.6rem;
}
.detail-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 14rem), 1fr));
  gap: 0.4rem 1rem;
  font-size: 0.75rem;
}
.detail-grid > div {
  display: flex;
  min-width: 0;
  align-items: baseline;
  gap: 0.5rem;
}
.detail-grid dt {
  flex: none;
  color: var(--muted-foreground);
}
.detail-grid dd {
  min-width: 0;
  overflow-wrap: anywhere;
}
.detail-grid code {
  font-family: var(--font-technical);
}
.failure-facts {
  font-size: 0.8rem;
}
:global(.stream-alert) {
  width: auto;
  margin: 0.25rem 0.5rem 0.5rem;
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
</style>

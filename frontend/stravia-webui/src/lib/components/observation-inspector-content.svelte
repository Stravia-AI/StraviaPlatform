<script lang="ts">
import type { Snippet } from 'svelte'
import * as m from '$lib/paraglide/messages.js'
import DownloadIcon from '@lucide/svelte/icons/download'
import XIcon from '@lucide/svelte/icons/x'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'

import { formatDuration, formatLogTime, formatNumber, formatTps } from '$lib/format'
import ObservationConversation from '$lib/components/observation-conversation.svelte'
import ObservationLogViewport from '$lib/components/observation-log-viewport.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import TechnicalValue from '$lib/components/technical-value.svelte'
import { failureOriginLabel, observationDebugStatusLabel, observationStatusLabel } from '$lib/observation-labels'
import { interactionDisplayStatus } from '$lib/observation-chain-visibility'
import { observationEventSummary, observationStatusTone } from '$lib/observation-event-summary'
import { deriveTimeline, itemEvents, itemKey, processGroup, type StreamItem } from '$lib/observation-timeline'
import type { InteractionDetail, LiveContentBlock, ObservationEvent, RunDetail, FailedRequestDetail } from '$lib/types'
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

const timeline = $derived(deriveTimeline(interaction, failure))

let flashRun = $state<string | null>(null)
let flashTimer: ReturnType<typeof setTimeout> | undefined
function jumpToRun(runId: string | null) {
  if (!runId) return
  document.getElementById(`obs-run-${runId}`)?.scrollIntoView({ block: 'nearest' })
  flashRun = runId
  clearTimeout(flashTimer)
  flashTimer = setTimeout(() => {
    if (flashRun === runId) flashRun = null
  }, 1600)
}
</script>

{#snippet eventRow(event: ObservationEvent)}
  {@const summary = observationEventSummary(event)}
  <li class="stream-item">
    <Collapsible.Root>
      <Collapsible.Trigger class="stream-row" data-tone={summary.tone} data-sequence={event.sequence}>
        <span class="stream-text">
          <strong>{summary.title}</strong>
          {#each summary.facts as fact (fact.label)}
            <span class="fact"
              ><span class="fact-label">{fact.label}</span><span class="fact-value">{fact.value}</span></span>
          {/each}
        </span>
        <time class="stream-time" title={formatLogTime(event.occurred_at)}
          >{timeline.offsetLabel(event.occurred_at)}</time>
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

{#snippet streamItem(item: StreamItem)}
  {@const events = itemEvents(item)}
  {#if item.type === 'event'}
    {@render eventRow(item.event)}
  {:else if events.length === 1}
    {@render eventRow(events[0])}
  {:else}
    {@const group = item.type === 'process' ? processGroup(events) : null}
    <li class="stream-item stream-group" data-group={item.type}>
      <Collapsible.Root>
        <Collapsible.Trigger class="stream-row" data-tone="neutral" data-group={item.type}>
          <span class="stream-text">
            <strong
              >{item.type === 'tools'
                ? m.observation_tool_handoffs({ tool: item.name ?? m.observation_event_tool(), count: events.length })
                : group?.label}</strong>
            {#if group?.titles}<span class="group-titles">{group.titles}</span>{/if}
          </span>
          <time
            class="stream-time"
            title="{formatLogTime(events[0].occurred_at)} – {formatLogTime(events[events.length - 1].occurred_at)}"
            >{timeline.offsetLabel(events[0].occurred_at)}</time>
          <ChevronRightIcon size={14} class="stream-chev" aria-hidden="true" />
        </Collapsible.Trigger>
        <Collapsible.Content>
          <ol class="branch sub">
            {#each events as event (event.sequence)}
              {@render eventRow(event)}
            {/each}
          </ol>
        </Collapsible.Content>
      </Collapsible.Root>
    </li>
  {/if}
{/snippet}

{#snippet runBlock(run: RunDetail)}
  {@const ordinal = timeline.runIndex.get(run.id)}
  {@const items = timeline.streams.get(run.id) ?? []}
  {@const duration = run.finished_at == null ? null : run.finished_at - run.started_at}
  {@const parentIndex = run.parent_run_id ? (timeline.runIndex.get(run.parent_run_id) ?? null) : null}
  <!-- 父 Run 仍在未加载的更早区间时只显示编号，不提供跳转。 -->
  {@const parentShown = parentIndex !== null && parentIndex > timeline.orderedRuns.length - timeline.visibleRuns.length}
  {@const metrics = timeline.metrics.get(run.id)}
  <li class="stream-run" id="obs-run-{run.id}" data-flash={flashRun === run.id || null}>
    <Collapsible.Root>
      <div class="run-band">
        <Collapsible.Trigger
          class="stream-row run-head"
          data-tone={observationStatusTone(run.status)}
          data-run={run.id}>
          <span class="run-summary">
            <span class="run-title">
              <span class="stream-text">
                <span class="run-idx font-technical">R{ordinal}</span>
                <strong class="font-structural">{run.model_display_name?.trim() || run.route_id}</strong>
                <!-- 标题行不放标签：状态由主干圆点形状与颜色表达，文字只留给读屏；中断与记录不完整在事件流和告警中呈现。 -->
                <span class="sr-only">{observationStatusLabel(run.status)}</span>
              </span>
              <time class="stream-time" title={formatLogTime(run.started_at)}
                >{timeline.offsetLabel(run.started_at)}</time>
              <ChevronRightIcon size={14} class="stream-chev" aria-hidden="true" />
            </span>
            <!-- 首行回答「谁在服务、多快」，次行回答「用了多少」；未报告的值显示中性破折号。 -->
            <span class="run-metrics">
              <span class="run-metric-line">
                {#each metrics?.upstream ?? [] as target (`${target.model}\u0000${target.provider ?? ''}`)}
                  <span class="run-metric">
                    <span class="run-metric-value">{target.model}</span>
                    {#if target.provider}<span class="run-metric-label">· {target.provider}</span>{/if}
                  </span>
                {/each}
                {@render runMetric(m.logs_duration_short(), formatDuration(duration))}
                {@render runMetric(m.logs_first_token_short(), formatDuration(metrics?.firstTokenMs))}
                <!-- 单位 tok/s 已说明含义，省去标签让首行在窄检查器中少换行。 -->
                <span class="run-metric" title={m.logs_token_speed()}
                  ><span class="run-metric-value">{metrics?.tps == null ? '– tok/s' : formatTps(metrics.tps)}</span
                  ></span>
              </span>
              <span class="run-metric-line">
                {@render tokenMetric(m.observation_usage_input(), run.usage.input_tokens)}
                {@render tokenMetric(m.observation_usage_output(), run.usage.output_tokens)}
                {@render tokenMetric(m.observation_usage_cache_read(), run.usage.cache_read_tokens)}
                {@render tokenMetric(m.observation_usage_cache_write(), run.usage.cache_write_tokens)}
              </span>
            </span>
          </span>
        </Collapsible.Trigger>
        {#if run.parent_run_id}
          {#if parentShown}
            <button
              type="button"
              class="run-parent"
              title={m.observation_run_jump_to({ index: `R${parentIndex}` })}
              onclick={() => jumpToRun(run.parent_run_id)}
              >{m.observation_run_continued_from({ index: `R${parentIndex}` })}</button>
          {:else if parentIndex !== null}
            <span class="run-parent" data-muted>{m.observation_run_continued_from({ index: `R${parentIndex}` })}</span>
          {:else}
            <span class="run-parent" data-muted>{m.observation_run_continued_external()}</span>
          {/if}
        {/if}
      </div>
      <!-- 记录不完整是判断依据，不随事件流一起折叠。 -->
      {#if run.trace?.status === 'partial' || (run.debug_enabled && !run.trace)}
        <Alert.Root variant="warning" role="status" class="stream-alert"
          ><Alert.Description>
            {m.observation_partial_trace({ reasons: run.trace?.reasons.join(', ') || m.observation_trace_missing() })}
          </Alert.Description></Alert.Root>
      {/if}
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
            <dt>{m.observation_protocol()}</dt>
            <dd class="font-technical">{run.ingress_protocol}</dd>
          </div>
          <div>
            <dt>{m.observation_delivery()}</dt>
            <dd>{run.client_output_committed ? m.observation_committed() : m.observation_not_committed()}</dd>
          </div>
          {#if run.parent_run_id}
            <div>
              <dt>{m.observation_parent_run()}</dt>
              <dd><TechnicalValue value={run.parent_run_id} copyable /></dd>
            </div>
          {/if}
        </dl>
        {#if items.length}
          <ol class="branch">
            {#each items as item (itemKey(item))}
              {@render streamItem(item)}
            {/each}
          </ol>
        {/if}
      </Collapsible.Content>
    </Collapsible.Root>
  </li>
{/snippet}

{#snippet runMetric(label: string, value: string)}
  <span class="run-metric"
    ><span class="run-metric-label">{label}</span> <span class="run-metric-value">{value}</span></span>
{/snippet}

{#snippet tokenMetric(label: string, value: number | null)}
  <span class="run-metric" title={value == null ? m.observation_usage_unknown() : undefined}
    ><span class="run-metric-label">{label}</span>
    <span class="run-metric-value">{value == null ? '–' : formatNumber(value)}</span></span>
{/snippet}

<header class="flex items-start justify-between gap-3 border-b p-4">
  <div class="min-w-0">
    <p class="font-structural text-xs font-semibold tracking-wider text-primary uppercase">
      {interaction ? m.observation_interaction_details() : m.observation_failed_request_details()}
    </p>
    <h2 class="font-structural mt-1 truncate text-xl font-semibold">{timeline.title}</h2>
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
        <dd>{failureOriginLabel(failure.request.error.source) ?? '—'}</dd>
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
      {#each timeline.failureItems as item (itemKey(item))}
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
    <Tabs.Content value="diagnostics" class="min-h-0 flex-1 overflow-hidden">
      {#if interaction}
        <div class="flex h-full min-h-0 flex-col">
          <dl class="diagnostic-overview">
            <div>
              <dt class="text-xs text-muted-foreground">{m.common_status()}</dt>
              <dd class="font-medium">{observationStatusLabel(interactionDisplayStatus(interaction.interaction))}</dd>
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
          {#key interaction.interaction.id}
            <div class="min-h-0 flex-1">
              <ObservationLogViewport
                label={m.observation_diagnostics()}
                olderCursor={interaction.older_events_cursor}
                {olderLoading}
                {onolder}>
                {#snippet children(older: Snippet)}
                  <div class="diagnostic-log">
                    {@render older()}
                    <ol class="stream">
                      {#each timeline.visibleRuns as run, index (run.id)}
                        {#if index > 0}
                          {@const gap = timeline.gapLabel(timeline.visibleRuns[index - 1], run)}
                          {#if gap}<li class="stream-gap" role="separator" aria-label={gap}><span>{gap}</span></li>{/if}
                        {/if}
                        {@render runBlock(run)}
                      {/each}
                    </ol>
                  </div>
                {/snippet}
              </ObservationLogViewport>
            </div>
          {/key}
        </div>
      {/if}
    </Tabs.Content>
  </Tabs.Root>
{/if}

<style>
.diagnostic-overview {
  display: grid;
  flex: none;
  grid-template-columns: repeat(auto-fit, minmax(8rem, 1fr));
  gap: 0.75rem;
  border-bottom: 1px solid var(--border);
  padding: 1rem;
  font-size: 0.875rem;
}
.diagnostic-log {
  padding: 1rem;
}
.diagnostic-overview dd {
  min-width: 0;
}
/* 诊断流是一条带主干的列表：Run 按时间拍平为并列分段，左边线是唯一主干；
   分组事件的下级列表使用虚线，层级最多两层。 */
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
.branch.sub {
  margin-inline-start: 0.15rem;
  padding-inline-start: 0.85rem;
  border-inline-start-style: dashed;
}
.branch.sub :global(.stream-row)::before {
  inset-inline-start: calc(-0.85rem - 1px - 0.2rem);
  width: 0.4rem;
  height: 0.4rem;
}
.stream-item,
.stream-run {
  min-width: 0;
}
/* 相邻请求之间的等待间隔 */
.stream-gap {
  display: flex;
  align-items: center;
  gap: 0.6rem;
  color: var(--muted-foreground);
  font-family: var(--font-technical);
  font-size: 0.72rem;
  font-variant-numeric: tabular-nums;
}
.stream-gap::before,
.stream-gap::after {
  content: '';
  flex: 1;
  border-top: 1px dashed var(--border);
}
.stream-gap span {
  white-space: nowrap;
}
/* Run 分段带：触发区占满，续接跳转片是右侧独立命中区（不与触发器嵌套）。 */
.run-band {
  display: flex;
  align-items: stretch;
  gap: 0.25rem;
  min-width: 0;
}
.run-band > :global(.stream-row) {
  flex: 1;
  width: auto;
  min-width: 0;
}
.run-idx {
  flex: none;
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 0 0.3rem;
  color: var(--muted-foreground);
  font-size: 0.68rem;
  line-height: 1.6;
}
.run-parent {
  flex: none;
  align-self: stretch;
  display: inline-flex;
  align-items: center;
  min-height: 40px;
  padding: 0 0.6rem;
  border: 0;
  border-inline-start: 1px dashed var(--border);
  background: transparent;
  color: var(--primary);
  font-family: inherit;
  font-size: 0.72rem;
  white-space: nowrap;
  cursor: pointer;
}
button.run-parent:hover {
  background: var(--accent);
}
button.run-parent:focus-visible {
  outline: 2px solid var(--primary);
  outline-offset: -2px;
}
.run-parent[data-muted] {
  color: var(--muted-foreground);
  cursor: default;
}
.stream-run[data-flash] .run-band :global(.stream-row) {
  background: var(--accent);
  box-shadow: inset 2px 0 0 var(--primary);
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
  /* 与标题行垂直居中：行内边距 0.3rem + 标题行最小高度的一半。 */
  top: calc(0.3rem + 0.875rem);
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
/* Run 摘要：标题行与两行小字指标；时间与展开箭头只跟随标题行。 */
.run-summary {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-direction: column;
  gap: 0.1rem;
}
.run-title {
  display: flex;
  min-width: 0;
  min-height: 1.75rem;
  align-items: center;
  gap: 0.5rem;
}
.run-metrics {
  display: flex;
  flex-direction: column;
  padding-block-end: 0.1rem;
  font-size: 0.72rem;
  line-height: 1.5;
}
.run-metric-line {
  display: flex;
  min-width: 0;
  flex-wrap: wrap;
  gap: 0 0.9rem;
}
.run-metric {
  min-width: 0;
  overflow-wrap: anywhere;
}
.run-metric-label {
  color: var(--muted-foreground);
}
.run-metric-value {
  font-family: var(--font-technical);
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

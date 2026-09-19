import { formatDuration, formatList, formatNumber, formatTime } from '$lib/format'
import { observationAttemptOutputTokens, observationEventSummary } from '$lib/observation-event-summary'
import { payloadRecord, payloadString } from '$lib/observation-payload'
import * as m from '$lib/paraglide/messages.js'
import type { FailedRequestDetail, InteractionDetail, ObservationEvent, RunDetail } from '$lib/types/observation'

export type StreamItem =
  | { type: 'event'; event: ObservationEvent }
  | { type: 'tools'; name: string | null; events: ObservationEvent[] }
  | { type: 'process'; events: ObservationEvent[] }

export interface ObservationTimeline {
  title: string
  orderedRuns: RunDetail[]
  runIndex: Map<string, number>
  timelines: Map<string, ObservationEvent[]>
  attemptOutputs: Map<string, Map<string, number | null>>
  streams: Map<string, StreamItem[]>
  failureItems: StreamItem[]
  offsetLabel(at: number): string
  gapLabel(previous: RunDetail, next: RunDetail): string | null
}

function orderedEvents(events: ObservationEvent[]): ObservationEvent[] {
  // 因果子树会把晚发生的完成事件提前；阅读时间线按时间排序，原始关联仍保留在 payload。
  return events.toSorted((a, b) => a.occurred_at - b.occurred_at || a.sequence - b.sequence)
}

function toolName(event: ObservationEvent): string | null {
  if (event.kind !== 'client_tool_handoff') return null
  const name = payloadString(payloadRecord(event.payload).name)
  return name?.trim() ? name : null
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

function streamItems(events: readonly ObservationEvent[], outputs?: ReadonlyMap<string, number | null>): StreamItem[] {
  const items: StreamItem[] = []
  for (const event of events) {
    const process = PROCESS_KINDS.has(event.kind) && observationEventSummary(event, outputs).tone === 'neutral'
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

export function itemEvents(item: StreamItem): ObservationEvent[] {
  return item.type === 'event' ? [item.event] : item.events
}

export function itemKey(item: StreamItem): number {
  return item.type === 'event' ? item.event.sequence : item.events[0].sequence
}

// 相邻请求之间的等待是一等事实：小于后端 rapid_continuation 窗口（2s）的间隔属于流水线噪声。
const GAP_MIN_MS = 2_000

// 同种过程事件合并为「标题 × N」，混合过程事件显示计数并附种类名帮助扫描。
export function processGroup(events: readonly ObservationEvent[]): { label: string; titles: string | null } {
  const titles = [...new Set(events.map((event) => observationEventSummary(event).title))]
  if (titles.length === 1) {
    return { label: m.observation_event_group({ title: titles[0], count: events.length }), titles: null }
  }
  return {
    label: m.observation_process_events({ count: events.length }),
    titles: titles.length <= 3 ? formatList(titles) : `${formatList(titles.slice(0, 3))}…`,
  }
}

export function usageText(value: number | null): string {
  return value == null ? m.observation_usage_unknown() : formatNumber(value)
}

export function usageRows(run: RunDetail): ReadonlyArray<readonly [string, number | null]> {
  return [
    [m.observation_event_tokens_input(), run.usage.input_tokens],
    [m.observation_event_tokens_output(), run.usage.output_tokens],
    [m.observation_event_tokens_cache_read(), run.usage.cache_read_tokens],
    [m.observation_event_tokens_cache_write(), run.usage.cache_write_tokens],
  ]
}

export function deriveTimeline(
  interaction: InteractionDetail | undefined,
  failure: FailedRequestDetail | undefined,
): ObservationTimeline {
  const orderedRuns = [...(interaction?.runs ?? [])].sort((a, b) => a.started_at - b.started_at)
  // Run 的父子关系表达续接与因果，不是包含：续接链按时间拍平展示，父 Run 仅以编号引用。
  const runIndex = new Map(orderedRuns.map((run, index) => [run.id, index + 1]))

  const title = interaction
    ? orderedRuns.at(-1)?.model_display_name?.trim() ||
      orderedRuns.at(-1)?.route_id ||
      interaction.interaction.first_model_display_name?.trim() ||
      interaction.interaction.first_route_id
    : failure
      ? m.observation_request_failed()
      : m.observation_details()

  const timelines = new Map(orderedRuns.map((run) => [run.id, orderedEvents(run.events)]))
  const attemptOutputs = new Map(orderedRuns.map((run) => [run.id, observationAttemptOutputTokens(run.events)]))
  const streams = new Map(
    orderedRuns.map((run) => [run.id, streamItems(timelines.get(run.id) ?? [], attemptOutputs.get(run.id))]),
  )
  const failureItems = failure ? streamItems(orderedEvents(failure.events)) : []

  const baseTime = interaction?.interaction.started_at ?? failure?.request.started_at ?? null

  function offsetLabel(at: number): string {
    if (baseTime == null) return formatTime(at)
    const delta = at - baseTime
    return delta >= 0 ? `+${formatDuration(delta)}` : `−${formatDuration(-delta)}`
  }

  function runEndAt(run: RunDetail): number {
    if (run.finished_at != null) return run.finished_at
    return timelines.get(run.id)?.at(-1)?.occurred_at ?? run.started_at
  }

  function lastHandoffTool(run: RunDetail): string | null {
    const events = timelines.get(run.id) ?? []
    for (let i = events.length - 1; i >= 0; i--) {
      if (events[i].kind === 'client_tool_handoff') {
        const name = toolName(events[i])
        if (name) return name
      }
    }
    return null
  }

  function gapLabel(previous: RunDetail, next: RunDetail): string | null {
    const gap = next.started_at - runEndAt(previous)
    if (gap < GAP_MIN_MS) return null
    const tool = lastHandoffTool(previous)
    const duration = formatDuration(gap)
    return tool ? m.observation_gap_tool({ tool, duration }) : m.observation_gap_idle({ duration })
  }

  return { title, orderedRuns, runIndex, timelines, attemptOutputs, streams, failureItems, offsetLabel, gapLabel }
}

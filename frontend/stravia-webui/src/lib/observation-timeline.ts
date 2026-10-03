import { formatDuration, formatList, formatTime, generationMsOf } from '$lib/format'
import { attemptTpsInput, observationEventSummary } from '$lib/observation-event-summary'
import { payloadCount, payloadRecord, payloadString } from '$lib/observation-payload'
import * as m from '$lib/paraglide/messages.js'
import type { FailedRequestDetail, InteractionDetail, ObservationEvent, RunDetail } from '$lib/types/observation'

export type StreamItem =
  | { type: 'event'; event: ObservationEvent }
  | { type: 'tools'; name: string | null; events: ObservationEvent[] }
  | { type: 'process'; events: ObservationEvent[] }

export interface ObservationTimeline {
  title: string
  orderedRuns: RunDetail[]
  /** 已加载事件页覆盖的 Run；更早事件未加载时，排在最早已加载 Run 之前的 Run 暂不展示。 */
  visibleRuns: RunDetail[]
  runIndex: Map<string, number>
  timelines: Map<string, ObservationEvent[]>
  streams: Map<string, StreamItem[]>
  metrics: Map<string, RunMetrics>
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

// 过程层事件：诊断关联、捕获与按项收口的内容记录不承载主流程叙事；带成败或警告语义时仍留在主干。
const PROCESS_KINDS: Record<string, true> = {
  retained_tail_associated: true,
  native_compaction_associated: true,
  input_preview_recorded: true,
  credential_mappings_created: true,
  checkpoint: true,
  wire: true,
  client_visible_content: true,
  model_thinking: true,
}

function streamItems(events: readonly ObservationEvent[]): StreamItem[] {
  const items: StreamItem[] = []
  for (const event of events) {
    const process = event.kind in PROCESS_KINDS && observationEventSummary(event).tone === 'neutral'
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

export interface RunUpstream {
  model: string
  provider: string | null
}

export interface RunMetrics {
  upstream: RunUpstream[]
  firstTokenMs: number | null
  tps: number | null
}

/**
 * Run 摘要描述实际产出结果的上游尝试：有已完成尝试时取全部已完成尝试（平台工具循环会有多个模型轮次），
 * 否则取最近一次开始的尝试，避免回退前失败的尝试冒充服务方。同一尝试的修订完成事件以最后一条为准。
 * 首字取第一个服务尝试；速度为服务尝试的输出合计除以净生成耗时合计。
 */
export function runMetrics(events: readonly ObservationEvent[]): RunMetrics {
  const started = new Map<string, RunUpstream>()
  const finished = new Map<string, Record<string, unknown>>()
  let lastStarted: string | null = null
  for (const event of events) {
    const payload = payloadRecord(event.payload)
    const id = payloadString(payload.attempt_id)
    if (!id) continue
    if (event.kind === 'target_attempt_started') {
      const model = payloadString(payload.upstream_model)?.trim()
      if (model) started.set(id, { model, provider: payloadString(payload.provider_name)?.trim() || null })
      lastStarted = id
    } else if (event.kind === 'target_attempt_finished') {
      finished.set(id, payload)
    }
  }
  const completed = [...finished].filter(([, payload]) => payload.status === 'completed').map(([id]) => id)
  const serving = completed.length ? completed : lastStarted ? [lastStarted] : [...finished.keys()].slice(-1)

  const upstream = new Map<string, RunUpstream>()
  let firstTokenMs: number | null = null
  let outputTokens = 0
  let generationMs = 0
  for (const id of serving) {
    const target = started.get(id)
    if (target) upstream.set(`${target.model}\u0000${target.provider ?? ''}`, target)
    const payload = finished.get(id)
    if (!payload) continue
    if (firstTokenMs === null && payloadCount(payload.first_token_ms)) firstTokenMs = payload.first_token_ms
    const input = attemptTpsInput(payload)
    const generation = generationMsOf(input)
    if (input.output_tokens && generation && generation > 0) {
      outputTokens += input.output_tokens
      generationMs += generation
    }
  }
  return {
    upstream: [...upstream.values()],
    firstTokenMs,
    tps: generationMs > 0 ? outputTokens / (generationMs / 1000) : null,
  }
}

export function deriveTimeline(
  interaction: InteractionDetail | undefined,
  failure: FailedRequestDetail | undefined,
): ObservationTimeline {
  const orderedRuns = [...(interaction?.runs ?? [])].sort((a, b) => a.started_at - b.started_at)
  // Run 的父子关系表达续接与因果，不是包含：续接链按时间拍平展示，父 Run 仅以编号引用。
  const runIndex = new Map(orderedRuns.map((run, index) => [run.id, index + 1]))
  // 详情只带最新一页事件：更早的 Run 事件尚未加载而非不存在，展示成空 Run 会被误读为没有事件。
  const firstLoaded =
    interaction?.older_events_cursor == null ? 0 : orderedRuns.findIndex((run) => run.events.length > 0)
  const visibleRuns = firstLoaded > 0 ? orderedRuns.slice(firstLoaded) : orderedRuns

  const title = interaction
    ? orderedRuns.at(-1)?.model_display_name?.trim() ||
      orderedRuns.at(-1)?.route_id ||
      interaction.interaction.first_model_display_name?.trim() ||
      interaction.interaction.first_route_id
    : failure
      ? m.observation_request_failed()
      : m.observation_details()

  const timelines = new Map(orderedRuns.map((run) => [run.id, orderedEvents(run.events)]))
  const streams = new Map(orderedRuns.map((run) => [run.id, streamItems(timelines.get(run.id) ?? [])]))
  const metrics = new Map(orderedRuns.map((run) => [run.id, runMetrics(timelines.get(run.id) ?? [])]))
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

  return { title, orderedRuns, visibleRuns, runIndex, timelines, streams, metrics, failureItems, offsetLabel, gapLabel }
}

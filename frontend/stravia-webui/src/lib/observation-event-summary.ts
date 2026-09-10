import { computeTps, formatDuration, formatNumber, formatTps } from '$lib/format'
import {
  observationContextStatusLabel,
  observationDebugStatusLabel,
  observationStatusLabel,
} from '$lib/observation-labels'
import * as m from '$lib/paraglide/messages.js'
import type { ObservationEvent } from '$lib/types/observation'

type EventSummary = {
  title: string
  facts: Array<{ label: string; value: string }>
  note?: string
  tone: 'neutral' | 'success' | 'warning' | 'error'
}

const TITLES: Record<string, () => string> = {
  run_admitted: m.observation_event_run_admitted,
  generation_associated: m.observation_event_generation_associated,
  retained_tail_associated: m.observation_event_retained_tail_associated,
  native_compaction_associated: m.observation_event_native_compaction_associated,
  model_turn_started: m.observation_event_model_turn_started,
  model_turn_finished: m.observation_event_model_turn_finished,
  target_attempt_started: m.observation_event_target_attempt_started,
  target_attempt_finished: m.observation_event_target_attempt_finished,
  platform_tool_started: m.observation_event_platform_tool_started,
  platform_tool_finished: m.observation_event_platform_tool_finished,
  client_tool_handoff: m.observation_event_client_tool_handoff,
  client_tool_result: m.observation_event_client_tool_result,
  model_thinking_delta: m.observation_event_model_thinking_delta,
  model_thinking_finished: m.observation_event_model_thinking_finished,
  compaction_operation: m.observation_event_compaction_operation,
  delivery_finished: m.observation_event_delivery_finished,
  run_finished: m.observation_event_run_finished,
  run_state_changed: m.observation_event_run_state_changed,
  request_rejected: m.observation_event_request_rejected,
  observation_gap: m.observation_event_observation_gap,
  usage_confirmed: m.observation_event_usage_confirmed,
  client_visible_content_delta: m.observation_event_client_visible_content_delta,
  client_output_committed: m.observation_event_client_output_committed,
  input_preview_recorded: m.observation_event_input_preview_recorded,
  credential_mappings_created: m.observation_event_credential_mappings_created,
  checkpoint: m.observation_event_checkpoint,
  wire: m.observation_event_wire,
  trace_manifest_updated: m.observation_event_trace_manifest_updated,
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : {}
}

function text(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() ? value : undefined
}

function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}

function statusLabel(status: string): string {
  switch (status) {
    case 'completed':
    case 'failed':
    case 'cancelled':
    case 'running':
    case 'interrupted':
    case 'user_interrupted':
    case 'waiting_client':
      return observationStatusLabel(status)
    default:
      return status
  }
}

function statusTone(status: unknown): EventSummary['tone'] {
  switch (status) {
    case 'completed':
      return 'success'
    case 'failed':
      return 'error'
    case 'cancelled':
    case 'interrupted':
    case 'user_interrupted':
      return 'warning'
    default:
      return 'neutral'
  }
}

/** 索引同一 Run 内各次尝试最后确认的输出用量；累计快照不能相加。 */
export function observationAttemptOutputTokens(events: readonly ObservationEvent[]): Map<string, number | null> {
  const latest = new Map<string, ObservationEvent>()
  for (const event of events) {
    if (event.kind !== 'usage_confirmed') continue
    const attempt = text(record(event.payload).attempt_id)
    if (!attempt) continue
    const previous = latest.get(attempt)
    if (!previous || previous.sequence < event.sequence) {
      latest.set(attempt, event)
    }
  }
  const outputs = new Map<string, number | null>()
  for (const [attempt, event] of latest) {
    const value = record(record(event.payload).usage).output_tokens
    outputs.set(attempt, count(value) ? value : null)
  }
  return outputs
}

export function observationEventSummary(
  event: ObservationEvent,
  attemptOutputTokens?: ReadonlyMap<string, number | null>,
): EventSummary {
  const known = Object.hasOwn(TITLES, event.kind)
  const summary: EventSummary = {
    title: known ? TITLES[event.kind]() : m.observation_event_unknown(),
    facts: [],
    tone: 'neutral',
  }
  if (!known) {
    summary.note = m.observation_event_unknown_note()
    return summary
  }
  const payload = record(event.payload)
  const add = (label: string, value: unknown) => {
    const content = text(value)
    if (content) summary.facts.push({ label, value: content })
  }
  const duration = (key: string, label: string) => {
    if (count(payload[key])) add(label, formatDuration(payload[key]))
  }
  const httpStatus = () => {
    if (count(payload.status_code) && payload.status_code >= 100 && payload.status_code <= 599) {
      add(m.observation_http_status(), String(payload.status_code))
    }
  }
  const result = () => {
    const status = text(payload.status)
    if (status) add(m.observation_event_result(), statusLabel(status))
    summary.tone = statusTone(payload.status)
  }
  switch (event.kind) {
    case 'run_admitted':
    case 'model_turn_started':
      add(m.observation_chat_model(), text(payload.model_display_name) ?? payload.route_id)
      break
    case 'target_attempt_started':
      add(m.observation_event_model_service(), payload.provider_name)
      add(m.observation_chat_model(), payload.upstream_model)
      add(m.observation_protocol(), payload.protocol)
      break
    case 'platform_tool_started':
    case 'client_tool_handoff':
      add(m.observation_event_tool(), payload.name)
      break
    case 'client_tool_result':
      add(m.observation_event_result(), observationStatusLabel(payload.is_error === true ? 'failed' : 'completed'))
      summary.tone = payload.is_error === true ? 'error' : 'success'
      break
    case 'model_turn_finished':
    case 'platform_tool_finished':
    case 'target_attempt_finished':
    case 'run_finished':
    case 'run_state_changed':
      result()
      if (event.kind === 'target_attempt_finished') {
        httpStatus()
        add(m.observation_error_code(), payload.error_code)
        duration('first_token_ms', m.observation_event_first_token())
      }
      if (event.kind === 'run_finished') add(m.observation_event_reason(), payload.terminal_reason)
      if (event.kind === 'run_state_changed') {
        add(
          m.observation_event_reason(),
          payload.reason === 'user_interrupted' ? m.observation_user_interrupted() : payload.reason,
        )
      }
      if (event.kind === 'target_attempt_finished' || event.kind === 'platform_tool_finished') {
        duration('duration_ms', m.observation_duration())
      }
      if (event.kind === 'target_attempt_finished') {
        const attempt = text(payload.attempt_id)
        const firstToken = count(payload.first_token_ms) ? payload.first_token_ms : null
        const speed = computeTps({
          output_tokens: attempt ? attemptOutputTokens?.get(attempt) : null,
          is_stream: firstToken !== null,
          latency_upstream_ms: count(payload.duration_ms) ? payload.duration_ms : null,
          stream_first_chunk_ms: firstToken,
        })
        add(m.logs_token_speed(), formatTps(speed))
      }
      break
    case 'delivery_finished':
      if (payload.status === 'delivered') {
        add(m.observation_event_result(), m.observation_event_delivered())
        summary.tone = 'success'
      } else if (payload.status === 'delivery_failed') {
        add(m.observation_event_result(), observationStatusLabel('failed'))
        summary.tone = 'error'
      } else if (payload.status === 'cancelled') {
        result()
      } else {
        add(m.observation_event_result(), payload.status)
      }
      add(m.observation_event_reason(), payload.reason)
      break
    case 'retained_tail_associated':
      // 匹配结果只解释诊断关联，不能代表请求成功或执行历史发生变化。
      summary.note = m.observation_diagnostic_only()
      if (
        ['inferred', 'no_match', 'ambiguous', 'index_unavailable', 'resource_limit'].includes(
          text(payload.status) ?? '',
        )
      ) {
        add(m.observation_event_result(), observationContextStatusLabel(event.kind, payload))
      } else {
        add(m.observation_event_result(), payload.status)
      }
      break
    case 'compaction_operation':
      summary.note = m.observation_compaction_unknown_usage()
      if (
        ['started', 'registered', 'published', 'delivery_unconfirmed', 'failed'].includes(text(payload.phase) ?? '')
      ) {
        add(m.observation_event_result(), observationContextStatusLabel(event.kind, payload))
      } else {
        add(m.observation_event_result(), payload.phase)
      }
      summary.tone =
        payload.phase === 'failed' ? 'error' : payload.phase === 'delivery_unconfirmed' ? 'warning' : 'neutral'
      add(m.observation_error_code(), payload.error_code)
      duration('duration_ms', m.observation_duration())
      break
    case 'request_rejected':
      summary.tone = 'error'
      httpStatus()
      add(m.observation_error_code(), payload.code)
      add(m.observation_rejected_stage(), payload.stage)
      break
    case 'observation_gap':
      summary.tone = 'warning'
      summary.note = m.observation_event_gap_note()
      add(m.observation_event_reason(), payload.reason)
      break
    case 'usage_confirmed': {
      const usage = record(payload.usage)
      const fields: Array<[string, () => string]> = [
        ['input_tokens', m.observation_event_tokens_input],
        ['output_tokens', m.observation_event_tokens_output],
        ['cache_read_tokens', m.observation_event_tokens_cache_read],
        ['cache_write_tokens', m.observation_event_tokens_cache_write],
        ['reasoning_tokens', m.observation_event_tokens_reasoning],
      ]
      for (const [key, label] of fields) {
        if (count(usage[key])) add(label(), formatNumber(usage[key]))
      }
      break
    }
    case 'credential_mappings_created':
      if (Array.isArray(payload.discoveries) && payload.discoveries.length > 0) {
        add(m.observation_event_credentials(), formatNumber(payload.discoveries.length))
      }
      break
    case 'checkpoint':
      add(m.observation_event_stage(), payload.stage)
      break
    case 'wire':
      add(m.observation_protocol(), payload.protocol)
      httpStatus()
      break
    case 'trace_manifest_updated':
      if (['complete', 'missing', 'none', 'partial', 'running', 'writing'].includes(text(payload.status) ?? '')) {
        add(m.observation_event_result(), observationDebugStatusLabel(text(payload.status) ?? ''))
      } else {
        add(m.observation_event_result(), payload.status)
      }
      if (payload.status === 'missing' || payload.status === 'partial') summary.tone = 'warning'
      if (Array.isArray(payload.reasons)) {
        add(
          m.observation_event_reason(),
          payload.reasons.filter((reason): reason is string => !!text(reason)).join(', '),
        )
      }
      break
  }
  return summary
}

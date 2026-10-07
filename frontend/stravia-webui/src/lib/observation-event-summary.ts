import { computeTps, formatDuration, formatNumber, formatTps, type TpsInput } from '$lib/format'
import { failureOriginLabel, observationContextStatusLabel, observationStatusLabel } from '$lib/observation-labels'
import { payloadCount, payloadRecord } from '$lib/observation-payload'
import * as m from '$lib/paraglide/messages.js'
import type { ObservationEvent } from '$lib/types/observation'

type EventSummary = {
  title: string
  facts: Array<{ label: string; value: string }>
  note?: string
  tone: 'neutral' | 'success' | 'warning' | 'error'
}

// 思考与可见正文按 Canonical Item 各落一行；确认用量由 target_attempt_finished 携带。
const TITLES: Record<string, () => string> = {
  run_admitted: m.observation_event_run_admitted,
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
  model_thinking: m.observation_event_model_thinking,
  compaction_operation: m.observation_event_compaction_operation,
  run_finished: m.observation_event_run_finished,
  run_state_changed: m.observation_event_run_state_changed,
  request_rejected: m.observation_event_request_rejected,
  request_failed: m.observation_request_failed,
  observation_gap: m.observation_event_observation_gap,
  client_visible_content: m.observation_event_client_visible_content,
  input_preview_recorded: m.observation_event_input_preview_recorded,
  credential_mappings_created: m.observation_event_credential_mappings_created,
  checkpoint: m.observation_event_checkpoint,
  wire: m.observation_event_wire,
}

const record = payloadRecord
const count = payloadCount

function text(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() ? value : undefined
}

function observationGapNote(reason: string | undefined): string {
  if (reason === 'generation_parent_observation_unavailable') {
    return m.observation_gap_generation_parent_unavailable()
  }
  if (reason === 'unfinished_observation_activity') return m.observation_gap_unfinished_activity()
  if (reason?.startsWith('settlement_generation_commit:')) {
    return m.observation_gap_delivered_history_not_saved()
  }
  return m.observation_event_gap_note()
}

function statusLabel(status: string): string {
  switch (status) {
    case 'completed':
    case 'failed':
    case 'cancelled':
    case 'running':
    case 'interrupted':
    case 'disconnected':
    case 'superseded':
    case 'user_interrupted':
    case 'waiting_client':
      return observationStatusLabel(status)
    default:
      return status
  }
}

/** `target_attempt_finished` 的输出除以该尝试完整耗时，与首字耗时无关。 */
export function attemptTpsInput(payload: Record<string, unknown>): TpsInput {
  const usage = record(payload.usage)
  return {
    output_tokens: count(usage.output_tokens) ? usage.output_tokens : null,
    duration_ms: count(payload.duration_ms) ? payload.duration_ms : null,
  }
}

export function observationStatusTone(status: unknown): EventSummary['tone'] {
  switch (status) {
    case 'completed':
      return 'success'
    case 'failed':
      return 'error'
    case 'cancelled':
    case 'interrupted':
    case 'disconnected':
    case 'user_interrupted':
      return 'warning'
    default:
      return 'neutral'
  }
}

export function observationEventSummary(event: ObservationEvent): EventSummary {
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
  const httpStatus = (from: Record<string, unknown> = payload) => {
    if (count(from.status_code) && from.status_code >= 100 && from.status_code <= 599) {
      add(m.observation_http_status(), String(from.status_code))
    }
  }
  const result = () => {
    const status = text(payload.status)
    if (status) add(m.observation_event_result(), statusLabel(status))
    summary.tone = observationStatusTone(payload.status)
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
        duration('duration_ms', m.observation_duration())
        // 确认用量随完成事件同载荷返回（含失败/中断的部分用量），管理面 input 已减缓存读。
        const usage = record(payload.usage)
        const usageFields: Array<[string, () => string]> = [
          ['input_tokens', m.observation_event_tokens_input],
          ['output_tokens', m.observation_event_tokens_output],
          ['cache_read_tokens', m.observation_event_tokens_cache_read],
          ['cache_write_tokens', m.observation_event_tokens_cache_write],
        ]
        for (const [key, label] of usageFields) {
          if (count(usage[key])) add(label(), formatNumber(usage[key]))
        }
        add(m.logs_token_speed(), formatTps(computeTps(attemptTpsInput(payload))))
      }
      if (event.kind === 'platform_tool_finished') {
        duration('duration_ms', m.observation_duration())
      }
      if (event.kind === 'run_finished') {
        add(m.observation_event_reason(), payload.terminal_reason)
        // 交付结果并入 run_finished：delivered/delivery_failed/cancelled 与附带原因。
        const delivery = record(payload.delivery)
        const deliveryStatus = text(delivery.status)
        if (deliveryStatus) {
          const deliveryLabel =
            deliveryStatus === 'delivered'
              ? m.observation_event_delivered()
              : deliveryStatus === 'delivery_failed'
                ? observationStatusLabel('failed')
                : deliveryStatus === 'cancelled'
                  ? observationStatusLabel('cancelled')
                  : deliveryStatus
          add(m.observation_delivery(), deliveryLabel)
          add(m.observation_event_delivery_reason(), delivery.reason)
          if (deliveryStatus === 'delivery_failed' && summary.tone === 'neutral') summary.tone = 'warning'
        }
      }
      if (event.kind === 'run_state_changed') {
        const reason =
          payload.reason === 'user_interrupted'
            ? m.observation_user_interrupted()
            : payload.reason === 'superseded'
              ? m.observation_status_superseded()
              : payload.reason === 'process_restarted'
                ? m.observation_event_reason_process_restarted()
                : payload.reason
        add(m.observation_event_reason(), reason)
      }
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
    case 'request_failed': {
      // 请求级终态失败：上游与平台诊断同源展示；来源或字段缺失不补造。
      const error = record(payload.error)
      summary.tone = 'error'
      add(m.failed_request_origin(), failureOriginLabel(error.source))
      httpStatus(error)
      add(m.observation_error_code(), error.code)
      add(m.observation_upstream_error_code(), error.upstream_code)
      add(m.failed_request_error(), error.message)
      break
    }
    case 'observation_gap':
      summary.tone = 'warning'
      summary.note = observationGapNote(text(payload.reason))
      add(m.observation_event_reason(), payload.reason)
      break
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
    case 'model_thinking':
    case 'client_visible_content':
      // 每个 Canonical Item 一行；complete=false 仅出现在已处理的取消/失败时保留部分正文。
      if (payload.complete === false) {
        add(m.observation_event_result(), m.observation_event_incomplete())
        summary.tone = 'warning'
        summary.note = m.observation_event_content_incomplete()
      }
      break
  }
  return summary
}

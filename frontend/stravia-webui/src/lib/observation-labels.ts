import { payloadRecord, payloadString } from '#lib/observation-payload.js'
import * as m from '#lib/paraglide/messages.js'

type LabelMessage = () => string

const STATUS_LABELS: Record<string, LabelMessage> = {
  cancelled: m.observation_status_cancelled,
  completed: m.observation_status_completed,
  disconnected: m.observation_status_disconnected,
  failed: m.observation_status_failed,
  interrupted: m.observation_status_interrupted,
  running: m.observation_status_running,
  superseded: m.observation_status_superseded,
  user_interrupted: m.observation_status_interrupted,
  waiting_client: m.observation_status_waiting_client,
}

const DEBUG_STATUS_LABELS: Record<string, LabelMessage> = {
  complete: m.observation_debug_status_complete,
  missing: m.observation_debug_status_missing,
  none: m.observation_debug_status_none,
  partial: m.observation_debug_status_partial,
  running: m.observation_debug_status_running,
  writing: m.observation_debug_status_writing,
}

export function observationStatusLabel(status: string): string {
  return (STATUS_LABELS[status] ?? m.observation_status_unknown)()
}

export function observationDebugStatusLabel(status: string): string {
  return (DEBUG_STATUS_LABELS[status] ?? m.observation_debug_status_unknown)()
}

const COMPACTION_PHASE_LABELS: Record<string, LabelMessage> = {
  started: m.observation_compaction_started,
  registered: m.observation_compaction_registered,
  published: m.observation_compaction_published,
  delivery_unconfirmed: m.observation_compaction_delivery_unconfirmed,
  failed: m.observation_status_failed,
}
const TAIL_STATUS_LABELS: Record<string, LabelMessage> = {
  inferred: m.observation_ancestry_inferred,
  no_match: m.observation_tail_no_match,
  ambiguous: m.observation_tail_ambiguous,
  index_unavailable: m.observation_tail_unavailable,
  resource_limit: m.observation_tail_resource_limit,
}

/** 未知来源返回 undefined 而非 '—'，不虚构 origin；需要占位符的展示方自行 `?? '—'`。 */
export function failureOriginLabel(source: unknown): string | undefined {
  if (source === 'platform') return m.failed_request_platform()
  if (source === 'upstream') return m.failed_request_upstream()
  const raw = payloadString(source)
  return raw?.trim() ? raw : undefined
}

export function clientErrorTypeLabel(type: string): string {
  if (type === 'failed') return m.observation_client_error_type_failed()
  if (type === 'rejected') return m.observation_client_error_type_rejected()
  return type
}

// 错误的请求不是失败的请求（GLOSSARY.md），详情标题不能沿用“请求失败”。
export function clientErrorTitle(type: string): string {
  return type === 'rejected' ? m.observation_client_error_type_rejected() : m.observation_request_failed()
}

export function clientErrorTypeTone(type: string): 'destructive' | 'outline' {
  return type === 'failed' ? 'destructive' : 'outline'
}

const UPSTREAM_OUTCOME_LABELS: Record<string, LabelMessage> = {
  recovered: m.observation_upstream_outcome_recovered,
  caused_failure: m.observation_upstream_outcome_caused_failure,
  in_progress: m.observation_upstream_outcome_in_progress,
  interrupted: m.observation_upstream_outcome_interrupted,
}

export function upstreamErrorOutcomeLabel(outcome: string): string {
  return (UPSTREAM_OUTCOME_LABELS[outcome] ?? (() => outcome))()
}

export function upstreamErrorOutcomeTone(outcome: string): 'destructive' | 'secondary' | 'outline' {
  if (outcome === 'caused_failure') return 'destructive'
  if (outcome === 'recovered') return 'secondary'
  return 'outline'
}

export function observationContextStatusLabel(kind: string, payload: unknown): string {
  if (!payload || typeof payload !== 'object') return ''
  const value = payloadRecord(payload)
  if (kind === 'compaction_operation') {
    const mode =
      value.mode === 'standalone'
        ? m.observation_compaction_standalone()
        : value.mode === 'inline'
          ? m.observation_compaction_inline()
          : m.observation_compaction_operation()
    const phase = typeof value.phase === 'string' ? COMPACTION_PHASE_LABELS[value.phase]?.() : undefined
    return phase ? `${mode} · ${phase}` : mode
  }
  if (kind === 'retained_tail_associated') {
    return typeof value.status === 'string' ? (TAIL_STATUS_LABELS[value.status]?.() ?? '') : ''
  }
  return ''
}

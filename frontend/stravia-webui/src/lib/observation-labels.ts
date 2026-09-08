import * as m from '$lib/paraglide/messages.js'

type LabelMessage = () => string

const STATUS_LABELS: Record<string, LabelMessage> = {
  cancelled: m.observation_status_cancelled,
  completed: m.observation_status_completed,
  failed: m.observation_status_failed,
  interrupted: m.observation_status_interrupted,
  running: m.observation_status_running,
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

export function observationContextStatusLabel(kind: string, payload: unknown): string {
  if (!payload || typeof payload !== 'object') return ''
  const value = payload as Record<string, unknown>
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

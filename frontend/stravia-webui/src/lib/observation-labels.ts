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

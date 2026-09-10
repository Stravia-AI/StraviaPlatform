export interface ConfirmedUsage {
  input_tokens: number | null
  output_tokens: number | null
  cache_read_tokens: number | null
  cache_write_tokens: number | null
  reasoning_tokens: number | null
}

export interface ObservationEvent {
  sequence: number
  occurred_at: number
  interaction_id: string | null
  run_id: string | null
  rejection_id: string | null
  kind: string
  payload: unknown
}

export interface ForestQuery {
  start_at?: number
  end_at?: number
  anchor_at?: number
  window_index?: number
  cursor?: string
  limit?: number
  provider?: string
  model?: string
  api_key?: string
  status?: string
}

export interface InteractionNodeData extends Record<string, unknown> {
  interaction: InteractionSummary
  onSelectedPath: boolean
  subdued: boolean
}

export interface InteractionSummary {
  context_events: ObservationEvent[]
  id: string
  root_id: string
  parent_interaction_id: string | null
  generation_root_id: string | null
  first_route_id: string
  first_model_display_name: string | null
  status: string
  started_at: number
  last_active_at: number
  input_preview: string | null
  visible_tail: string
  usage: ConfirmedUsage
  debug_status: string
  observation_gap: boolean
  matched: boolean
  last_event_sequence: number
}

export interface ForestRoot {
  id: string
  last_active_at: number
  interactions: InteractionSummary[]
}

export interface ForestPage {
  anchor_at: number
  window_index: number
  window_start: number
  window_end: number
  roots: ForestRoot[]
  root_total: number
  next_cursor: string | null
  snapshot_sequence: number
}

export interface TraceManifest {
  trace_id: string
  enabled: boolean
  status: string
  bytes_written: number
  event_count: number
  reasons: string[]
}

export interface RunDetail {
  id: string
  parent_run_id: string | null
  generation_node_id: string | null
  generation_parent_id: string | null
  route_id: string
  model_display_name: string | null
  ingress_protocol: string
  status: string
  terminal_reason: string | null
  user_interrupted: boolean
  debug_enabled: boolean
  client_output_committed: boolean
  started_at: number
  finished_at: number | null
  usage: ConfirmedUsage
  events: ObservationEvent[]
  trace: TraceManifest | null
  debug_events: unknown[]
}

export interface InteractionDetail {
  interaction: InteractionSummary
  root: ForestRoot
  runs: RunDetail[]
  snapshot_sequence: number
}

export interface RejectionQuery {
  start_at?: number
  end_at?: number
  anchor_at?: number
  window_index?: number
  cursor?: string
  limit?: number
}

export interface RejectionSummary {
  id: string
  occurred_at: number
  method: string
  path: string
  ingress_protocol: string
  stage: string
  code: string
  status_code: number
  debug_enabled: boolean
  debug_status: string
}

export interface RejectionPage {
  items: RejectionSummary[]
  total: number
  next_cursor: string | null
  snapshot_sequence: number
}

export interface RejectionDetail {
  rejection: RejectionSummary
  events: ObservationEvent[]
  trace: TraceManifest | null
  debug_events: unknown[]
  snapshot_sequence: number
}

export interface DebugState {
  enabled: boolean
  run_limit_bytes: number
  total_limit_bytes: number
  retained_bytes: number
  partial_trace_count: number
  retention_days: number
}

export interface ClearHistoryResult {
  deleted_interactions: number
  deleted_rejections: number
  skipped_active: number
}

export type BundleResourceKind = 'interaction' | 'rejected_request'

export interface BundleRequest {
  kind: BundleResourceKind
  resource_id: string
  through_sequence?: number
}

export interface DownloadTicket {
  download_url: string
  expires_at: number
  through_sequence: number
}

export type ObservationStreamUpdate =
  { type: 'event'; event: ObservationEvent } | { type: 'reset_required'; snapshot_sequence: number }

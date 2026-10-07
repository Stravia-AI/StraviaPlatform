export interface FailureDiagnostic {
  source: 'platform' | 'upstream' | null
  code: string | null
  message: string | null
  status_code: number | null
  upstream_code: string | null
}

export type FailedRequestQuery = Omit<ForestQuery, 'status'>

export interface FailedRequestSummary {
  id: string
  kind: 'rejection' | 'run'
  request_id: string
  started_at: number
  duration_ms: number | null
  api_key_id: string | null
  api_key_name: string | null
  client: string | null
  model: string | null
  model_display_name: string | null
  services: { id: string; name: string }[]
  error: FailureDiagnostic
  interaction_id: string | null
  root_id: string | null
  run_id: string | null
  debug_status: string
  observation_gap: boolean
}

export interface FailedRequestPage {
  items: FailedRequestSummary[]
  total: number
  next_cursor: string | null
  snapshot_sequence: number
}

export interface FailedRequestDetail {
  request: FailedRequestSummary
  events: ObservationEvent[]
  trace: TraceManifest | null
  snapshot_sequence: number
}

export interface ConfirmedUsage {
  input_tokens: number | null
  output_tokens: number | null
  cache_read_tokens: number | null
  cache_write_tokens: number | null
  reasoning_tokens: number | null
  coverage?: UsageCoverage
}

export interface UsageCoverage {
  attempt_count: number
  missing_input_tokens: number
  missing_output_tokens: number
  missing_cache_read_tokens: number
  missing_cache_write_tokens: number
  missing_reasoning_tokens: number
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
  live_window?: boolean
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
  min_tokens?: number
}

export interface ObservationOutputPreview {
  text: string
  /** 完整已接收输出中的 UTF-16 起点；尾窗滑动无需保留或复制前文。 */
  start: number
}

export interface InteractionNodeData extends Record<string, unknown> {
  interaction: InteractionSummary
  onSelectedPath: boolean
  subdued: boolean
  outputPreview?: ObservationOutputPreview
  outputActive?: boolean
  outputSnapshotKey?: number
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
  failed_request: boolean
  client_output_delivered: boolean
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

export interface RootChangesQuery {
  filters: ForestQuery
  roots: {
    root_id: string
    after_sequence: number
    known_interactions: Pick<InteractionSummary, 'id' | 'last_event_sequence' | 'matched' | 'debug_status'>[]
  }[]
}

export interface RootChangesPage {
  snapshot_sequence: number
  root_total: number
  reset_required: boolean
  changes: {
    root_id: string
    last_active_at: number
    interactions: InteractionSummary[]
    removed_interaction_ids: string[]
    removal_reason: 'deleted' | 'filter' | 'window' | null
  }[]
}

export interface ObservationChange {
  sequence: number
  occurred_at: number
  interaction_id: string | null
  root_id: string | null
  run_id: string | null
  rejection_id: string | null
  kind: string
  boundary: boolean
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
  delivery_completed_at: number | null
  usage: ConfirmedUsage
  events: ObservationEvent[]
  trace: TraceManifest | null
}

export interface LiveContentBlock {
  block_id: string
  interaction_id: string
  run_id: string
  kind: 'client_visible_content_delta' | 'model_thinking_delta'
  model_turn_id: string | null
  attempt_id: string | null
  occurred_at: number
  revision: number
  text: string
}

export interface InteractionEventsQuery {
  after_sequence?: number
  before_sequence?: number
  through_sequence?: number
  limit?: number
}

export interface InteractionEventsPage {
  runs: RunDetail[]
  snapshot_sequence: number
  next_cursor: number | null
}

export interface InteractionDetail {
  older_events_cursor: number | null
  interaction: InteractionSummary
  root: ForestRoot
  runs: RunDetail[]
  snapshot_sequence: number
}

export interface DebugState {
  enabled: boolean
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
  { type: 'event'; event: ObservationChange } | { type: 'reset_required'; snapshot_sequence: number }

export type ObservationLiveUpdate =
  | { type: 'live_content'; block: LiveContentBlock }
  | { type: 'live_snapshot'; blocks: LiveContentBlock[] }
  | { type: 'live_gap'; interaction_id: string; run_id: string; reason: string }
  | { type: 'live_finished'; interaction_id: string; run_id: string }

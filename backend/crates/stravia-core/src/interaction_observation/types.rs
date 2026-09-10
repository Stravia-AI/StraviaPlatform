use std::pin::Pin;

use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type BundleStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;
pub type ObservationStream = Pin<Box<dyn Stream<Item = ObservationUpdate> + Send>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfirmedUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct IngressStart {
    pub id: String,
    pub method: String,
    pub path: String,
    pub protocol: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RunStart {
    pub id: String,
    pub principal: String,
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,
    pub generation_root_id: Option<String>,
    pub generation_parent_id: Option<String>,
    pub has_new_user: bool,
    // 仅用于进程内精确重试索引，不允许持久化或对外序列化。
    pub canonical_fingerprint: String,
    pub route_id: String,
    pub model_display_name: Option<String>,
    pub ingress_protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RejectedOutcome {
    pub stage: String,
    pub code: String,
    pub status_code: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunOutcome {
    pub status: String,
    pub terminal_reason: Option<String>,
    pub generation_node_id: Option<String>,
    pub generation_root_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CredentialDiscovery {
    pub rule_ids: Vec<String>,
    pub source_types: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialDiscoveryQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialDiscoverySummary {
    pub interaction_id: String,
    pub api_key_name: Option<String>,
    pub discovered_at: i64,
    pub new_credential_count: i64,
    pub rule_ids: Vec<String>,
    pub source_types: Vec<String>,
    pub status: String,
    pub observation_gap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialDiscoveryPage {
    pub items: Vec<CredentialDiscoverySummary>,
    pub next_cursor: Option<String>,
    pub observation_gap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompactionMode {
    Standalone,
    Inline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompactionPhase {
    Started,
    Registered,
    Published,
    DeliveryUnconfirmed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunEvent {
    CredentialMappingsCreated {
        discoveries: Vec<CredentialDiscovery>,
    },
    CompactionOperation {
        operation_id: String,
        model_turn_id: String,
        attempt_id: Option<String>,
        mode: CompactionMode,
        phase: CompactionPhase,
        source_generation_id: Option<String>,
        source_operation_id: Option<String>,
        registration_id: Option<String>,
        duration_ms: Option<i64>,
        error_code: Option<String>,
    },
    NativeCompactionAssociated {
        source_generation_id: Option<String>,
        source_operation_id: Option<String>,
        registration_id: String,
    },
    RetainedTailAssociated {
        source_run_id: Option<String>,
        source_interaction_id: Option<String>,
        status: String,
        matched_units: usize,
        matched_bytes: usize,
        input_start: Option<usize>,
        candidate_count: usize,
    },
    GenerationAssociated {
        root_id: String,
        parent_id: Option<String>,
        has_new_user: bool,
    },
    ModelTurnStarted {
        model_turn_id: String,
        route_id: String,
        model_display_name: Option<String>,
    },
    ModelTurnFinished {
        model_turn_id: String,
        status: String,
    },
    TargetAttemptStarted {
        model_turn_id: String,
        attempt_id: String,
        target_id: String,
        provider_id: String,
        provider_name: String,
        upstream_model: String,
        protocol: String,
        upstream_url: String,
    },
    TargetAttemptFinished {
        model_turn_id: String,
        attempt_id: String,
        status: String,
        status_code: Option<u16>,
        error_code: Option<String>,
        duration_ms: i64,
        first_token_ms: Option<i64>,
    },
    UsageConfirmed {
        model_turn_id: String,
        attempt_id: String,
        usage: ConfirmedUsage,
    },
    PlatformToolStarted {
        model_turn_id: String,
        tool_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
    },
    PlatformToolFinished {
        model_turn_id: String,
        tool_id: String,
        status: String,
        duration_ms: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
    },
    ClientToolHandoff {
        tool_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
    },
    ClientToolResult {
        tool_id: String,
        content: Value,
        is_error: bool,
    },
    ModelThinkingDelta {
        model_turn_id: String,
        attempt_id: String,
        text: String,
    },
    ModelThinkingFinished {
        model_turn_id: String,
        attempt_id: String,
    },
    ClientVisibleContentDelta {
        text: String,
    },
    ClientOutputCommitted,
    DeliveryFinished {
        status: String,
        reason: Option<String>,
    },
    Checkpoint {
        stage: String,
        model_turn_id: Option<String>,
        attempt_id: Option<String>,
        payload: Value,
    },
    Wire {
        direction: String,
        transport: String,
        protocol: String,
        message_type: String,
        model_turn_id: Option<String>,
        attempt_id: Option<String>,
        status_code: Option<u16>,
        url: Option<String>,
        headers: Value,
        payload: Value,
    },
    ObservationGap {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub sequence: i64,
    pub occurred_at: i64,
    pub interaction_id: Option<String>,
    pub run_id: Option<String>,
    pub rejection_id: Option<String>,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub enum ObservationUpdate {
    Event(ObservationEvent),
    ResetRequired { snapshot_sequence: i64 },
}

#[derive(Debug, thiserror::Error)]
pub enum ObservationQueryError {
    #[error("observation window requires both start_at and end_at")]
    IncompleteWindow,
    #[error("observation window must have end_at after start_at and span at most 24 hours")]
    InvalidWindow,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForestQuery {
    pub start_at: Option<i64>,
    pub end_at: Option<i64>,
    pub anchor_at: Option<i64>,
    pub window_index: Option<u32>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionSummary {
    /// Durable diagnostic/native metadata, never an execution parent override.
    #[serde(default)]
    pub context_events: Vec<ObservationEvent>,
    pub id: String,
    pub root_id: String,
    pub parent_interaction_id: Option<String>,
    pub generation_root_id: Option<String>,
    pub first_route_id: String,
    pub first_model_display_name: Option<String>,
    pub status: String,
    pub started_at: i64,
    pub last_active_at: i64,
    /// Redacted opening text of the initiating user message; absent for uncaptured input.
    pub input_preview: Option<String>,
    pub visible_tail: String,
    pub usage: ConfirmedUsage,
    pub debug_status: String,
    pub observation_gap: bool,
    pub matched: bool,
    pub last_event_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForestRoot {
    pub id: String,
    pub last_active_at: i64,
    pub interactions: Vec<InteractionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForestPage {
    pub anchor_at: i64,
    pub window_index: u32,
    pub window_start: i64,
    pub window_end: i64,
    pub roots: Vec<ForestRoot>,
    pub root_total: i64,
    pub next_cursor: Option<String>,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceManifest {
    pub trace_id: String,
    pub enabled: bool,
    pub status: String,
    pub bytes_written: u64,
    pub event_count: u64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetail {
    pub id: String,
    pub parent_run_id: Option<String>,
    pub generation_node_id: Option<String>,
    pub generation_parent_id: Option<String>,
    pub route_id: String,
    pub model_display_name: Option<String>,
    pub ingress_protocol: String,
    pub status: String,
    pub terminal_reason: Option<String>,
    pub user_interrupted: bool,
    pub debug_enabled: bool,
    pub client_output_committed: bool,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub usage: ConfirmedUsage,
    pub events: Vec<ObservationEvent>,
    pub trace: Option<TraceManifest>,
    pub debug_events: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionDetail {
    pub interaction: InteractionSummary,
    pub root: ForestRoot,
    pub runs: Vec<RunDetail>,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RejectionQuery {
    pub start_at: Option<i64>,
    pub end_at: Option<i64>,
    pub anchor_at: Option<i64>,
    pub window_index: Option<u32>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectionSummary {
    pub id: String,
    pub occurred_at: i64,
    pub method: String,
    pub path: String,
    pub ingress_protocol: String,
    pub stage: String,
    pub code: String,
    pub status_code: u16,
    pub debug_enabled: bool,
    pub debug_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectionPage {
    pub items: Vec<RejectionSummary>,
    pub total: i64,
    pub next_cursor: Option<String>,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectionDetail {
    pub rejection: RejectionSummary,
    pub events: Vec<ObservationEvent>,
    pub trace: Option<TraceManifest>,
    pub debug_events: Vec<Value>,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugState {
    pub enabled: bool,
    pub run_limit_bytes: u64,
    pub total_limit_bytes: u64,
    pub retained_bytes: u64,
    pub partial_trace_count: u64,
    pub retention_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearHistoryResult {
    pub deleted_interactions: u64,
    pub deleted_rejections: u64,
    pub skipped_active: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleResourceKind {
    Interaction,
    RejectedRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRequest {
    pub kind: BundleResourceKind,
    pub resource_id: String,
    pub through_sequence: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTicket {
    pub download_url: String,
    pub expires_at: i64,
    pub through_sequence: i64,
}

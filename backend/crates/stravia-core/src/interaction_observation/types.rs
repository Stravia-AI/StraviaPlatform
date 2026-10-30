use std::pin::Pin;

use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_protocol_codec::accumulator::CanonicalPartIndex;

use crate::pricing::{AttemptAmounts, CostBasis};

pub type BundleStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;
pub type ObservationStream = Pin<Box<dyn Stream<Item = ObservationUpdate> + Send>>;

/// Transport bytes keep their shared backing until the trace record is serialized.
/// Deserialization accepts the persisted JSON representation without reinterpreting it.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum WirePayload {
    Json(Value),
    #[serde(skip)]
    Bytes(Bytes),
    #[serde(skip)]
    Base64(Bytes),
}

impl From<Value> for WirePayload {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

struct EncodedBase64<'a>(&'a [u8]);

impl Serialize for EncodedBase64<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&base64::display::Base64Display::new(
            self.0,
            &base64::engine::general_purpose::STANDARD,
        ))
    }
}

impl Serialize for WirePayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Json(value) => value.serialize(serializer),
            Self::Bytes(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => serializer.serialize_str(text),
                Err(_) => {
                    use serde::ser::SerializeMap;
                    let mut map = serializer.serialize_map(Some(2))?;
                    // Value::Object uses sorted keys; preserve its exact encoded ordering.
                    map.serialize_entry("data", &EncodedBase64(bytes))?;
                    map.serialize_entry("encoding", "base64")?;
                    map.end()
                }
            },
            Self::Base64(bytes) => EncodedBase64(bytes).serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfirmedUsage {
    /// 原始事件为上游总输入；管理读取边界投影为逐 attempt 的净输入，不能再次扣缓存。
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    /// 聚合值只累计已确认用量；单次上游用量事件不携带聚合覆盖信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<UsageCoverage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, sqlx::FromRow)]
pub struct UsageCoverage {
    pub attempt_count: i64,
    /// 管理投影中，总输入或 cache-read 任一未知的 attempt 均计为净输入缺失。
    pub missing_input_tokens: i64,
    pub missing_output_tokens: i64,
    pub missing_cache_read_tokens: i64,
    pub missing_cache_write_tokens: i64,
    pub missing_reasoning_tokens: i64,
}

impl ConfirmedUsage {
    /// 只接收原始 attempt 快照；先逐 attempt 净化，再累计已知部分。
    pub(super) fn aggregate_management<'a>(attempts: impl Iterator<Item = &'a Self>) -> Self {
        let mut sums = [None::<i128>; 5];
        let mut missing = [0; 5];
        let mut attempt_count = 0;
        for attempt in attempts {
            attempt_count += 1;
            for (index, incoming) in [
                management_input_tokens(attempt.input_tokens, attempt.cache_read_tokens),
                attempt.output_tokens,
                attempt.cache_read_tokens,
                attempt.cache_write_tokens,
                attempt.reasoning_tokens,
            ]
            .into_iter()
            .enumerate()
            {
                if let Some(value) = incoming {
                    sums[index] = Some(sums[index].unwrap_or_default() + i128::from(value));
                } else {
                    missing[index] += 1;
                }
            }
        }
        let sums = sums.map(|value| value.and_then(|sum| i64::try_from(sum).ok()));
        Self {
            input_tokens: sums[0],
            output_tokens: sums[1],
            cache_read_tokens: sums[2],
            cache_write_tokens: sums[3],
            reasoning_tokens: sums[4],
            coverage: Some(UsageCoverage {
                attempt_count,
                missing_input_tokens: missing[0],
                missing_output_tokens: missing[1],
                missing_cache_read_tokens: missing[2],
                missing_cache_write_tokens: missing[3],
                missing_reasoning_tokens: missing[4],
            }),
        }
    }
}

pub(super) fn management_input_tokens(input: Option<i64>, cache_read: Option<i64>) -> Option<i64> {
    input
        .zip(cache_read)
        .map(|(input, cache_read)| input.saturating_sub(cache_read).max(0))
}

#[derive(Debug, Clone)]
pub(crate) struct IngressStart {
    pub id: String,
    pub method: String,
    pub path: String,
    pub protocol: String,
}

/// Run identity only. Attribution evidence (client items, chain facts, ingress
/// receipt) travels through `AdmissionFacts`; Run Attribution computes
/// fingerprinting, window capture, and receipt stamping inside its boundary.
#[derive(Debug, Clone)]
pub(crate) struct RunStart {
    pub id: String,
    pub principal: String,
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,
    pub route_id: String,
    pub model_display_name: Option<String>,
    pub ingress_protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RejectedOutcome {
    pub stage: String,
    pub code: String,
    pub status_code: u16,
    pub failure: Option<FailureDiagnostic>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailureDiagnostic {
    pub source: Option<String>,
    pub code: Option<String>,
    pub message: Option<String>,
    pub status_code: Option<u16>,
    /// 上游错误体自带的错误码（Connect `error.code`、OpenAI `code`/`type`、
    /// Anthropic `type`）。`code` 始终保持 Stravia 稳定失败词表。
    pub upstream_code: Option<String>,
}

impl FailureDiagnostic {
    pub(crate) fn platform(
        code: impl Into<String>,
        message: impl Into<String>,
        status: u16,
    ) -> Self {
        Self {
            source: Some("platform".into()),
            code: Some(code.into()),
            message: Some(message.into()),
            status_code: Some(status),
            upstream_code: None,
        }
    }
}

/// 上游错误体形态不一：`{"error":{"code"|"type":..}}`、裸顶层 `code`/`type`。
/// 只取字符串码——数字码已由 `status_code` 表达，不再重复。
pub(crate) fn upstream_body_code(body: &serde_json::Value) -> Option<String> {
    let error = body.get("error").unwrap_or(body);
    ["code", "type"]
        .iter()
        .find_map(|key| error.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientErrorQuery {
    pub start_at: Option<i64>,
    pub end_at: Option<i64>,
    pub anchor_at: Option<i64>,
    pub window_index: Option<u32>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// 客户端错误的类型（`failed` / `rejected`）；缺省为全部，非法值 400。
    /// 与记录种类（拒绝记录 / 运行）正交。
    #[serde(rename = "type")]
    pub request_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientErrorService {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientErrorSummary {
    pub id: String,
    pub kind: String,
    /// 客户端视角的分类：未进入 Target 选择即终止为 `rejected`，其余失败为 `failed`。
    #[serde(rename = "type")]
    pub request_type: String,
    pub request_id: String,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,
    pub client: Option<String>,
    pub model: Option<String>,
    pub model_display_name: Option<String>,
    pub services: Vec<ClientErrorService>,
    pub error: FailureDiagnostic,
    pub interaction_id: Option<String>,
    pub root_id: Option<String>,
    pub run_id: Option<String>,
    pub debug_status: String,
    pub observation_gap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientErrorPage {
    pub items: Vec<ClientErrorSummary>,
    pub total: i64,
    pub next_cursor: Option<String>,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientErrorDetail {
    pub request: ClientErrorSummary,
    pub events: Vec<ObservationEvent>,
    pub trace: Option<TraceManifest>,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpstreamErrorQuery {
    pub start_at: Option<i64>,
    pub end_at: Option<i64>,
    pub anchor_at: Option<i64>,
    pub window_index: Option<u32>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// 查询时派生的结果（`recovered` / `caused_failure` / `in_progress` / `interrupted`）。
    pub outcome: Option<String>,
}

/// 一次失败的上游 Target 尝试（含 capability 尝试）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamErrorSummary {
    /// Target attempt id。
    pub id: String,
    pub run_id: String,
    pub interaction_id: Option<String>,
    pub root_id: Option<String>,
    /// 与 `ClientErrorSummary.request_id` 同义：所属运行的请求标识。
    pub request_id: String,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub provider_id: Option<String>,
    pub provider_name: String,
    /// 实际发往上游的模型。
    pub upstream_model: Option<String>,
    /// 客户端请求的路由模型（run.request_model）。
    pub model: Option<String>,
    pub model_display_name: Option<String>,
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,
    pub client: Option<String>,
    pub error: FailureDiagnostic,
    pub outcome: String,
    pub debug_status: String,
    pub observation_gap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamErrorPage {
    pub items: Vec<UpstreamErrorSummary>,
    pub total: i64,
    pub next_cursor: Option<String>,
    pub snapshot_sequence: i64,
}

/// 同一运行内的一次尝试时序；详情按开始时间升序返回。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UpstreamAttemptTiming {
    pub id: String,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub status: String,
    pub provider_id: Option<String>,
    pub provider_name: String,
    pub upstream_model: Option<String>,
    pub status_code: Option<i64>,
    pub error_code: Option<String>,
    pub error_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamErrorDetail {
    pub error: UpstreamErrorSummary,
    pub attempts: Vec<UpstreamAttemptTiming>,
    pub events: Vec<ObservationEvent>,
    pub trace: Option<TraceManifest>,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeliveryOutcome {
    pub status: String,
    pub reason: Option<String>,
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunOutcome {
    #[serde(default)]
    pub client_output_committed: bool,
    #[serde(default)]
    pub delivery: Option<DeliveryOutcome>,
    /// Full successful client delivery, captured by the transport, not the writer.
    #[serde(default)]
    pub delivery_completed_at: Option<i64>,
    pub status: String,
    pub terminal_reason: Option<String>,
    pub generation_node_id: Option<String>,
    pub generation_root_id: Option<String>,
}

/// 一个映射在一次请求里的命中。`marker` 对所有 Run 记录，但永不进入列表接口。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CredentialDiscovery {
    pub rule_ids: Vec<String>,
    pub source_types: Vec<String>,
    /// 旧事件没有该字段，按空处理。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locators: Vec<CredentialLocator>,
    /// 超出上限而未记录的位置数。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_locators: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
}

/// 块级位置：不含偏移、片段或条目下标。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CredentialLocator {
    pub rule_id: String,
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_input: Option<bool>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl CredentialDiscovery {
    /// 写入队列的容量估算；与其他事件一样只计变长内容。
    pub(super) fn bytes(&self) -> usize {
        let locators: usize = self
            .locators
            .iter()
            .map(|locator| {
                locator.rule_id.len()
                    + locator.source_type.len()
                    + locator.tool_id.as_ref().map_or(0, String::len)
            })
            .sum();
        self.rule_ids
            .iter()
            .chain(&self.source_types)
            .map(String::len)
            .sum::<usize>()
            + locators
            + self.marker.as_ref().map_or(0, String::len)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialDiscoveryQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// 命中记录列表里的一个「规则 ← 来源」配对；顺序由载荷排序确定，`hits/{index}` 依赖它。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct CredentialDiscoveryHit {
    pub rule_id: String,
    pub source_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    /// 仅用户消息有意义：`true` 是本次输入，`false` 是客户端重放的历史。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_input: Option<bool>,
}

/// 命中记录列表的一行：一次新建映射。
#[derive(Debug, Clone, Serialize)]
pub struct CredentialDiscoveryItem {
    /// `{event.sequence}.{discovery index}`。
    pub id: String,
    /// 创建事件的发生时间（毫秒）。
    pub created_at: i64,
    pub interaction_id: String,
    pub run_id: Option<String>,
    pub api_key_name: Option<String>,
    /// 由规则 ID 前缀推导，按字典序去重；`rule_ids` 为空时同样为空。
    pub rule_sets: Vec<String>,
    pub rule_ids: Vec<String>,
    pub hits: Vec<CredentialDiscoveryHit>,
    pub omitted_hits: usize,
    /// 载荷携带非空占位符，才能关联后续复用。
    pub linkable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialDiscoveryPage {
    pub items: Vec<CredentialDiscoveryItem>,
    pub next_cursor: Option<String>,
}

/// 命中字段与来源事件；不含正文。
#[derive(Debug, Clone, Serialize)]
pub struct CredentialDiscoveryField {
    pub source_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// 命中详情；正文含明文，响应类型不得实现 `Debug`。
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CredentialDiscoveryHitDetail {
    Available {
        field: CredentialDiscoveryField,
        text: String,
        truncated: bool,
        /// `secret`：按映射明文命中；`rule`：退化为按所选规则命中。
        hit_basis: String,
        matches: Vec<stravia_credential_protection::CredentialTextMatch>,
    },
    Unavailable {
        field: CredentialDiscoveryField,
        /// `not_retained` 或 `missing`。
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialDiscoveryReuse {
    pub interaction_id: String,
    pub run_id: Option<String>,
    pub api_key_name: Option<String>,
    pub source_type: String,
    pub tool_id: Option<String>,
    pub latest_input: Option<bool>,
    pub last_reused_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialDiscoveryReuses {
    pub linkable: bool,
    pub items: Vec<CredentialDiscoveryReuse>,
    pub truncated: bool,
}

/// 命中记录的行引用、游标与索引错误；服务端据此区分 400/404。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CredentialDiscoveryError {
    #[error("invalid credential discovery cursor")]
    InvalidCursor,
    #[error("invalid credential discovery reference")]
    InvalidReference,
    #[error("credential discovery not found")]
    NotFound,
}

/// 一条新建映射的落库事实；只在内核与服务端适配层之间使用。
#[derive(Debug, Clone)]
pub(crate) struct CredentialDiscoveryRecord {
    pub sequence: i64,
    pub interaction_id: String,
    pub run_id: Option<String>,
    pub principal: String,
    pub discovery: CredentialDiscovery,
    pub hits: Vec<CredentialDiscoveryHit>,
}

/// 命中正文的解析结果；正文只在可用时携带。
pub(crate) enum CredentialDiscoveryBody {
    /// 来源事件不在库中或已过期。
    Missing,
    /// 该来源不保留正文，或来源事件没有正文。
    NotRetained,
    Retained(serde_json::Value),
}

pub(crate) struct CredentialDiscoveryHitContext {
    pub tool_name: Option<String>,
    pub body: CredentialDiscoveryBody,
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
    RequestFailed {
        error: FailureDiagnostic,
    },
    CredentialMappingsCreated {
        discoveries: Vec<CredentialDiscovery>,
        /// 超出单事件上限而未记录的映射数。
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted_discoveries: usize,
    },
    CredentialMappingsReused {
        discoveries: Vec<CredentialDiscovery>,
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted_discoveries: usize,
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
        #[serde(default)]
        retention_detached: bool,
        model_turn_id: String,
        route_id: String,
        model_display_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimated_input_tokens: Option<i64>,
    },
    ModelTurnFinished {
        #[serde(default)]
        retention_detached: bool,
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
        /// Cost basis frozen when the attempt starts; recovery keeps it so an
        /// interrupted terminal records an unknown cost instead of dropping it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_basis: Option<CostBasis>,
    },
    TargetAttemptFinished {
        model_turn_id: String,
        attempt_id: String,
        status: String,
        status_code: Option<u16>,
        error_code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<FailureDiagnostic>,
        duration_ms: i64,
        first_token_ms: Option<i64>,
        #[serde(default)]
        usage: Option<ConfirmedUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amounts: Option<AttemptAmounts>,
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
    ModelThinking {
        model_turn_id: String,
        attempt_id: String,
        text: String,
        parts: Vec<Value>,
        block_id: String,
        item: Value,
        complete: bool,
    },
    ClientVisibleContent {
        text: String,
        parts: Vec<Value>,
        block_id: String,
        item: Value,
        complete: bool,
    },
    ModelThinkingDelta {
        item_ordinal: usize,
        part_index: CanonicalPartIndex,
        model_turn_id: String,
        attempt_id: String,
        text: String,
    },
    ModelThinkingFinished {
        model_turn_id: String,
        attempt_id: String,
    },
    ClientVisibleContentDelta {
        item_ordinal: usize,
        part_index: CanonicalPartIndex,
        text: String,
    },
    ClientOutputCommitted,
    DeliveryFinished {
        status: String,
        reason: Option<String>,
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
        payload: WirePayload,
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
    Change(ObservationChange),
    ResetRequired {
        snapshot_sequence: i64,
    },
    LiveContent(LiveContentBlock),
    LiveSnapshot {
        blocks: Vec<LiveContentBlock>,
    },
    LiveGap {
        interaction_id: String,
        run_id: String,
        reason: String,
    },
    LiveFinished {
        interaction_id: String,
        run_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationChange {
    pub sequence: i64,
    pub occurred_at: i64,
    pub interaction_id: Option<String>,
    pub root_id: Option<String>,
    pub run_id: Option<String>,
    pub rejection_id: Option<String>,
    pub kind: String,
    pub boundary: bool,
    /// 仅 `target_attempt_finished` 且尝试带错误时取事件载荷中的 `error.source`；
    /// 上游错误列表据此判断是否需要静默刷新，读取载荷不额外查询存储。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_source: Option<String>,
}

impl ObservationChange {
    pub(super) fn from_event(event: ObservationEvent, root_id: Option<String>) -> Self {
        let boundary = matches!(
            event.kind.as_str(),
            "run_admitted"
                | "run_finished"
                | "run_state_changed"
                | "client_visible_content"
                | "model_thinking"
                | "request_rejected"
        );
        let error_source = (event.kind == "target_attempt_finished")
            .then(|| {
                event
                    .payload
                    .get("error")
                    .and_then(|error| error.get("source"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .flatten();
        Self {
            sequence: event.sequence,
            occurred_at: event.occurred_at,
            interaction_id: event.interaction_id,
            root_id,
            run_id: event.run_id,
            rejection_id: event.rejection_id,
            kind: event.kind,
            boundary,
            error_source,
        }
    }
}

/// 未提交的观察内容；revision 不得用作持久化 SSE cursor。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveContentBlock {
    pub block_id: String,
    pub interaction_id: String,
    pub run_id: String,
    pub kind: String,
    pub model_turn_id: Option<String>,
    pub attempt_id: Option<String>,
    pub occurred_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receive_order: Option<u64>,
    pub revision: u64,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ObservationQueryError {
    #[error("observation window requires both start_at and end_at")]
    IncompleteWindow,
    #[error("observation window must have end_at after start_at and span at most 24 hours")]
    InvalidWindow,
    #[error("invalid observation event page")]
    InvalidEventPage,
    #[error("invalid observation filter")]
    InvalidFilter,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForestQuery {
    pub start_at: Option<i64>,
    pub end_at: Option<i64>,
    #[serde(default)]
    pub live_window: bool,
    pub anchor_at: Option<i64>,
    pub window_index: Option<u32>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub status: Option<String>,
    /// 隐藏根 DAG 合计 Token（展示口径的输入+输出+缓存读+缓存写，含子孙）低于此值的链路；
    /// 运行中的 Model Turn 尚无上游用量时临时计入输入估算，确认用量到达后不再计入估算；0 或缺省表示不过滤。
    pub min_tokens: Option<i64>,
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
    /// Opening text of the normalized initiating user message; absent for uncaptured input.
    pub input_preview: Option<String>,
    pub visible_tail: String,
    /// The interaction contains at least one terminal-error run that the client-errors view
    /// lists as a run record (failed request, or request rejected before Target selection).
    pub failed_request: bool,
    /// Any run in this interaction committed client-visible output, i.e. the client
    /// received at least one visible byte (text, public tool call, or thinking preview).
    pub client_output_delivered: bool,
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
pub struct RootChangesQuery {
    pub filters: ForestQuery,
    pub roots: Vec<RootChangesBaseline>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootChangesBaseline {
    pub root_id: String,
    pub after_sequence: i64,
    pub known_interactions: Vec<KnownInteraction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownInteraction {
    pub id: String,
    pub last_event_sequence: i64,
    pub matched: bool,
    pub debug_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootChangesPage {
    pub snapshot_sequence: i64,
    pub root_total: i64,
    pub reset_required: bool,
    pub changes: Vec<RootChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootChange {
    pub root_id: String,
    pub last_active_at: i64,
    pub interactions: Vec<InteractionSummary>,
    pub removed_interaction_ids: Vec<String>,
    pub removal_reason: Option<String>,
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
    /// 实际客户端交付结束时间，独立于观测终态写入时间。
    pub delivery_completed_at: Option<i64>,
    pub usage: ConfirmedUsage,
    pub events: Vec<ObservationEvent>,
    pub trace: Option<TraceManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionSnapshot {
    pub interaction: InteractionSummary,
    pub root: ForestRoot,
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionDetail {
    pub interaction: InteractionSummary,
    pub root: ForestRoot,
    pub runs: Vec<RunDetail>,
    pub snapshot_sequence: i64,
    pub older_events_cursor: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InteractionEventsQuery {
    pub after_sequence: Option<i64>,
    pub before_sequence: Option<i64>,
    pub through_sequence: Option<i64>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionEventsPage {
    pub runs: Vec<RunDetail>,
    pub snapshot_sequence: i64,
    pub next_cursor: Option<i64>,
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
    pub snapshot_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugState {
    pub enabled: bool,
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

/// Stable canonical item identity from the real canonical output ordinal.
/// Late provider item IDs never change this identity.
pub(crate) fn canonical_item_block_id(scope: &str, item_ordinal: usize) -> String {
    format!("{scope}:item:{item_ordinal}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_usage_aggregates_each_known_field_independently() {
        let attempts = [
            ConfirmedUsage {
                input_tokens: Some(12528),
                output_tokens: Some(896),
                ..Default::default()
            },
            ConfirmedUsage {
                input_tokens: Some(0),
                cache_read_tokens: Some(0),
                ..Default::default()
            },
            ConfirmedUsage {
                cache_read_tokens: Some(5),
                cache_write_tokens: Some(0),
                ..Default::default()
            },
        ];
        let usage = ConfirmedUsage::aggregate_management(attempts.iter());
        assert_eq!(attempts[0].input_tokens, Some(12528));
        assert_eq!(usage.input_tokens, Some(0));
        assert_eq!(usage.output_tokens, Some(896));
        assert_eq!(usage.cache_read_tokens, Some(5));
        assert_eq!(usage.cache_write_tokens, Some(0));
        assert_eq!(usage.reasoning_tokens, None);
        assert_eq!(
            usage.coverage,
            Some(UsageCoverage {
                attempt_count: 3,
                missing_input_tokens: 2,
                missing_output_tokens: 2,
                missing_cache_read_tokens: 1,
                missing_cache_write_tokens: 2,
                missing_reasoning_tokens: 3,
            })
        );
    }

    #[test]
    fn management_usage_clamps_each_attempt_and_preserves_unknown_and_known_zero() {
        let attempts = [
            ConfirmedUsage {
                input_tokens: Some(12),
                cache_read_tokens: Some(5),
                cache_write_tokens: Some(100),
                ..Default::default()
            },
            ConfirmedUsage {
                input_tokens: Some(3),
                cache_read_tokens: Some(9),
                ..Default::default()
            },
            ConfirmedUsage {
                input_tokens: Some(8),
                ..Default::default()
            },
        ];
        let usage = ConfirmedUsage::aggregate_management(attempts.iter());
        assert_eq!(usage.input_tokens, Some(7));
        assert_eq!(usage.coverage.unwrap().missing_input_tokens, 1);
        assert_eq!(
            ConfirmedUsage::aggregate_management(attempts[1..2].iter()).input_tokens,
            Some(0)
        );
        assert_eq!(
            ConfirmedUsage::aggregate_management(attempts[2..].iter()).input_tokens,
            None
        );
        assert_eq!(management_input_tokens(None, Some(0)), None);
        assert_eq!(management_input_tokens(Some(0), Some(0)), Some(0));
        assert_eq!(
            ConfirmedUsage::aggregate_management(std::iter::empty()).input_tokens,
            None
        );
    }
}

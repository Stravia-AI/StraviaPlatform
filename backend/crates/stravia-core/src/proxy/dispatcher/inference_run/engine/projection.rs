//! Client Projection for one Inference Run.
//!
//! Canonical Text stays credential-free; upload placeholders are replaced only
//! in client delivery copies. OpenAI-compatible clients keep Thinking on the
//! reasoning carrier until the first non-empty Text, then use
//! quoted `content` previews bound to authoritative Thinking History Markers.
//! Other protocols retain native carriers for same-protocol Model Legs;
//! cross-protocol Thinking uses public previews and authoritative Markers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use crate::history_marker::{
    HISTORY_MARKER_PREFIX, HistoryMarker, HistoryMarkerError, HistoryMarkerStore,
    PROJECTION_DELIMITER_PREFIX, ThinkingMarkerInput, render_history_marker,
    render_preview_projection_end, render_preview_projection_span, render_preview_projection_start,
};
use stravia_protocol_codec::transform::ThinkingCarrierFacts;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;

const UPLOAD_PLACEHOLDER: &str = "<stravia-upload-key>";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum UploadCarrier {
    Text(Option<usize>, Option<usize>),
    Tool(usize),
}

struct PendingUploadPrefix {
    text: String,
    slots: Vec<u64>,
}

/// Only delivery copies enter this state. Pending text is bounded by one
/// placeholder prefix per carrier; queued events retain their original order.
#[derive(Default)]
struct UploadProjection {
    gateway: Option<crate::Gateway>,
    enabled: bool,
    grant: Option<crate::agent::upload_grant::UploadGrant>,
    pending: HashMap<UploadCarrier, PendingUploadPrefix>,
    queue: VecDeque<(u64, AiStreamDelta)>,
    next_slot: u64,
}

impl UploadProjection {
    async fn refresh(&mut self) -> Result<(), HistoryMarkerError> {
        if let Some(gateway) = &self.gateway {
            self.enabled = crate::agent::upload_grant::upload_prompt_enabled(gateway)
                .await
                .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
        }
        Ok(())
    }

    fn replace(
        &mut self,
        text: &mut String,
        principal: &Principal,
    ) -> Result<(), HistoryMarkerError> {
        if !self.enabled || !text.contains(UPLOAD_PLACEHOLDER) {
            return Ok(());
        }
        let gateway = self
            .gateway
            .as_ref()
            .expect("enabled upload projection gateway");
        if !self
            .grant
            .as_ref()
            .is_some_and(|grant| gateway.upload_grants.is_valid(grant))
        {
            self.grant = Some(
                gateway
                    .upload_grants
                    .issue(principal)
                    .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?,
            );
        }
        *text = text.replace(
            UPLOAD_PLACEHOLDER,
            &self.grant.as_ref().expect("issued upload grant").key,
        );
        Ok(())
    }

    fn replace_item(
        &mut self,
        item: &mut AiItem,
        principal: &Principal,
    ) -> Result<(), HistoryMarkerError> {
        if item.role != Role::Assistant {
            return Ok(());
        }
        match &mut item.content {
            MessageContent::Text(text) => {
                self.replace(std::sync::Arc::make_mut(text), principal)?
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if let ContentBlock::Text { text, .. } = block {
                        self.replace(std::sync::Arc::make_mut(text), principal)?;
                    }
                }
            }
        }
        if let Some(calls) = &mut item.tool_calls {
            for call in calls {
                self.replace(&mut call.arguments, principal)?;
            }
        }
        Ok(())
    }

    fn item_candidate(item: &AiItem, needle: &str) -> bool {
        item.role == Role::Assistant && (match &item.content {
            MessageContent::Text(text) => text.contains(needle),
            MessageContent::Blocks(blocks) => blocks.iter().any(
                |block| matches!(block, ContentBlock::Text { text, .. } if text.contains(needle)),
            ),
        } || item
            .tool_calls
            .as_ref()
            .is_some_and(|calls| calls.iter().any(|call| call.arguments.contains(needle))))
    }

    fn candidate(delta: &AiStreamDelta) -> bool {
        match delta {
            AiStreamDelta::TextDelta(text)
            | AiStreamDelta::TextDeltaWithMetadata { text, .. }
            | AiStreamDelta::ToolCallDelta {
                arguments: text, ..
            } => text.contains('<'),
            AiStreamDelta::ToolCallComplete { tool_call, .. } => tool_call.arguments.contains('<'),
            AiStreamDelta::ItemDone { item, .. } => Self::item_candidate(item, "<"),
            _ => false,
        }
    }

    fn carrier(delta: &AiStreamDelta) -> Option<UploadCarrier> {
        match delta {
            AiStreamDelta::TextDelta(_) => Some(UploadCarrier::Text(None, None)),
            AiStreamDelta::TextDeltaWithMetadata {
                output_index,
                content_index,
                ..
            } => Some(UploadCarrier::Text(*output_index, *content_index)),
            AiStreamDelta::ToolCallDelta { index, .. } => Some(UploadCarrier::Tool(*index)),
            _ => None,
        }
    }

    fn text(delta: &mut AiStreamDelta) -> &mut String {
        match delta {
            AiStreamDelta::TextDelta(text)
            | AiStreamDelta::TextDeltaWithMetadata { text, .. }
            | AiStreamDelta::ToolCallDelta {
                arguments: text, ..
            } => text,
            _ => unreachable!("upload text carrier"),
        }
    }

    fn drain_ready(
        &mut self,
        principal: &Principal,
    ) -> Result<Vec<AiStreamDelta>, HistoryMarkerError> {
        let first_blocked = self
            .pending
            .values()
            .flat_map(|prefix| prefix.slots.iter())
            .min()
            .copied();
        let mut ready = Vec::new();
        while self
            .queue
            .front()
            .is_some_and(|(slot, _)| first_blocked.is_none_or(|blocked| *slot < blocked))
        {
            let mut delta = self.queue.pop_front().expect("queued upload delta").1;
            match &mut delta {
                AiStreamDelta::TextDelta(text)
                | AiStreamDelta::TextDeltaWithMetadata { text, .. }
                | AiStreamDelta::ToolCallDelta {
                    arguments: text, ..
                } => self.replace(text, principal)?,
                AiStreamDelta::ToolCallComplete { tool_call, .. } => {
                    self.replace(&mut tool_call.arguments, principal)?
                }
                AiStreamDelta::ItemDone { item, .. } => self.replace_item(item, principal)?,
                _ => {}
            }
            ready.push(delta);
        }
        Ok(ready)
    }

    fn flush(&mut self, principal: &Principal) -> Result<Vec<AiStreamDelta>, HistoryMarkerError> {
        self.pending.clear();
        self.drain_ready(principal)
    }

    fn push(
        &mut self,
        mut delta: AiStreamDelta,
        principal: &Principal,
    ) -> Result<Vec<AiStreamDelta>, HistoryMarkerError> {
        if !self.enabled && self.pending.is_empty() && self.queue.is_empty() {
            return Ok(vec![delta]);
        }
        if let Some(carrier) = Self::carrier(&delta) {
            let text = Self::text(&mut delta);
            if text.is_empty() {
                self.queue.push_back((self.next_slot, delta));
                self.next_slot += 1;
                return self.drain_ready(principal);
            }
            if let Some(mut prefix) = self.pending.remove(&carrier) {
                let missing = &UPLOAD_PLACEHOLDER[prefix.text.len()..];
                if text.starts_with(missing) {
                    let replacement = UPLOAD_PLACEHOLDER.to_owned();
                    for (slot, queued) in &mut self.queue {
                        if prefix.slots.contains(slot) {
                            *Self::text(queued) = if *slot == prefix.slots[0] {
                                replacement.clone()
                            } else {
                                String::new()
                            };
                        }
                    }
                    text.drain(..missing.len());
                } else if missing.starts_with(text.as_str()) {
                    prefix.text.push_str(text);
                    prefix.slots.push(self.next_slot);
                    self.pending.insert(carrier, prefix);
                    self.queue.push_back((self.next_slot, delta));
                    self.next_slot += 1;
                    return self.drain_ready(principal);
                }
                // A mismatch releases the original fragments verbatim.
            }
            let keep = if self.enabled {
                (1..UPLOAD_PLACEHOLDER.len())
                    .rev()
                    .find(|&len| text.ends_with(&UPLOAD_PLACEHOLDER[..len]))
                    .unwrap_or(0)
            } else {
                0
            };
            let suffix = text.split_off(text.len() - keep);
            if keep > 0 {
                let mut pending_delta = delta.clone();
                *Self::text(&mut pending_delta) = suffix.clone();
                if let AiStreamDelta::TextDeltaWithMetadata {
                    logprobs,
                    obfuscation,
                    ..
                } = &mut pending_delta
                {
                    logprobs.clear();
                    *obfuscation = None;
                }
                self.queue.push_back((self.next_slot, delta));
                self.next_slot += 1;
                self.pending.insert(
                    carrier,
                    PendingUploadPrefix {
                        text: suffix,
                        slots: vec![self.next_slot],
                    },
                );
                self.queue.push_back((self.next_slot, pending_delta));
            } else {
                self.queue.push_back((self.next_slot, delta));
            }
            self.next_slot += 1;
        } else {
            match &mut delta {
                AiStreamDelta::ToolCallComplete { index, .. } => {
                    self.pending.remove(&UploadCarrier::Tool(*index));
                }
                AiStreamDelta::ItemDone { index, .. } => {
                    self.pending.retain(|carrier, _| {
                        !matches!(carrier, UploadCarrier::Tool(i) if i == index)
                            && !matches!(carrier, UploadCarrier::Text(Some(i), _) if i == index)
                    });
                }
                AiStreamDelta::ThinkingDelta(_)
                | AiStreamDelta::ThinkingDeltaWithMetadata { .. }
                | AiStreamDelta::ReasoningSummaryDelta { .. }
                | AiStreamDelta::ToolCallStart { .. } => {
                    self.pending.remove(&UploadCarrier::Text(None, None));
                }
                AiStreamDelta::Done { .. } | AiStreamDelta::ResponseTerminal { .. } => {
                    self.pending.clear()
                }
                _ => {}
            }
            self.queue.push_back((self.next_slot, delta));
            self.next_slot += 1;
        }
        self.drain_ready(principal)
    }
}

const THINKING_MARKER_PENDING_RETENTION: Duration = Duration::from_secs(60 * 60);
const PUBLISHED_MARKER_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreviewCarrier {
    Unindexed,
    Indexed {
        output_index: Option<usize>,
        content_index: Option<usize>,
        summary: bool,
    },
}

impl PreviewCarrier {
    fn text_delta(self, text: String) -> AiStreamDelta {
        match self {
            Self::Unindexed => AiStreamDelta::TextDelta(text),
            Self::Indexed {
                output_index,
                content_index,
                ..
            } => AiStreamDelta::TextDeltaWithMetadata {
                text,
                logprobs: Vec::new(),
                obfuscation: None,
                output_index,
                content_index,
            },
        }
    }
}

struct QuotedThinkingPreviewEncoder {
    reference: String,
    pending_private_prefix: String,
    pending_cr: bool,
    started: bool,
}

impl QuotedThinkingPreviewEncoder {
    fn new(reference: String) -> Self {
        Self {
            reference,
            pending_private_prefix: String::new(),
            pending_cr: false,
            started: false,
        }
    }

    fn push(&mut self, text: &str) -> String {
        self.pending_private_prefix.push_str(text);
        let keep = private_prefix_lookbehind(&self.pending_private_prefix);
        let split_at = self.pending_private_prefix.len() - keep;
        let safe = self.pending_private_prefix[..split_at].to_owned();
        self.pending_private_prefix.drain(..split_at);

        let escaped = escape_private_syntax(&safe);
        let mut quoted = self.quote_lines(&escaped, false);
        if !self.started {
            self.started = true;
            quoted = format!(
                "{}\n\n> {quoted}",
                render_preview_projection_start(&self.reference, 0)
            );
        }
        quoted
    }

    fn finish(mut self) -> String {
        let pending = std::mem::take(&mut self.pending_private_prefix);
        let escaped = escape_private_syntax(&pending);
        let mut quoted = self.quote_lines(&escaped, true);
        if !self.started {
            quoted = format!(
                "{}\n\n> {quoted}",
                render_preview_projection_start(&self.reference, 0)
            );
        }
        quoted.push_str("\n\n");
        quoted.push_str(&render_preview_projection_end(&self.reference, 0));
        quoted
    }

    fn quote_lines(&mut self, text: &str, finishing: bool) -> String {
        let mut quoted = String::with_capacity(text.len() + 8);
        let mut chars = text.chars().peekable();
        if self.pending_cr {
            self.pending_cr = false;
            if chars.peek() == Some(&'\n') {
                chars.next();
                quoted.push_str("\r\n> ");
            } else {
                quoted.push_str("\r> ");
            }
        }
        while let Some(ch) = chars.next() {
            match ch {
                '\r' if chars.peek() == Some(&'\n') => {
                    chars.next();
                    quoted.push_str("\r\n> ");
                }
                '\r' if chars.peek().is_none() && !finishing => self.pending_cr = true,
                '\r' => quoted.push_str("\r> "),
                '\n' => quoted.push_str("\n> "),
                _ => quoted.push(ch),
            }
        }
        if finishing && self.pending_cr {
            self.pending_cr = false;
            quoted.push_str("\r> ");
        }
        quoted
    }
}

struct LiveThinkingPreview {
    marker: HistoryMarker,
    encoder: QuotedThinkingPreviewEncoder,
    carrier: PreviewCarrier,
    canonical_text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProtectedPreviewCarrier {
    Unindexed,
    Thinking {
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
    Summary {
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
}

impl ProtectedPreviewCarrier {
    fn delta(self, text: String, obfuscation: Option<String>) -> AiStreamDelta {
        match self {
            Self::Unindexed => AiStreamDelta::ThinkingDelta(text),
            Self::Thinking {
                output_index,
                content_index,
            } => AiStreamDelta::ThinkingDeltaWithMetadata {
                text,
                obfuscation,
                output_index,
                content_index,
            },
            Self::Summary {
                output_index,
                content_index,
            } => AiStreamDelta::ReasoningSummaryDelta {
                text,
                obfuscation,
                output_index,
                content_index,
            },
        }
    }
}

struct LiveProtectedPreview {
    marker: HistoryMarker,
    post_text: bool,
    carrier: Option<ProtectedPreviewCarrier>,
    ordinal: usize,
    canonical_text: String,
}

#[derive(Clone)]
struct ProjectedThinkingMarker {
    marker: HistoryMarker,
    post_text: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnindexedItemKind {
    Text,
    Thinking,
    Tool,
}

#[derive(Clone)]
struct ProjectedMarkerReference {
    reference: String,
    platform: bool,
}

/// A projected delivery unit. A batch holding Marker references must reach
/// `ClientProjectionSession::report_delivery` once its bytes are Sent;
/// discarding it skips the publish, so the type is `#[must_use]`.
#[must_use]
pub(super) struct ProjectedDeltaBatch {
    deltas: Vec<AiStreamDelta>,
    references: Vec<ProjectedMarkerReference>,
}

impl ProjectedDeltaBatch {
    fn visible(deltas: Vec<AiStreamDelta>) -> Self {
        Self {
            deltas,
            references: Vec::new(),
        }
    }

    pub(super) fn deltas(&self) -> &[AiStreamDelta] {
        &self.deltas
    }

    pub(super) fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectionDelivery {
    Sent,
    Cancelled,
}

struct ClosedThinking {
    finish_deltas: Vec<AiStreamDelta>,
    preview_deltas: Vec<AiStreamDelta>,
    marker_deltas: Vec<AiStreamDelta>,
    markers: Vec<HistoryMarker>,
    had_live_projection: bool,
}

/// Stateful Client Projection for exactly one Inference Run.
///
/// The session owns run-wide Post-Text state and Thinking Marker persistence.
/// Model Leg boundaries only reset the staged cursor; they never reset the
/// run-wide state.
pub(super) struct ClientProjectionSession {
    state: ProjectionState,
    marker_store: Arc<dyn HistoryMarkerStore>,
    principal: Principal,
    leg_started_post_text: bool,
    thinking_source: Option<crate::history_marker::ThinkingSource>,
    early_thinking: BTreeMap<usize, VecDeque<ProjectedThinkingMarker>>,
    live_platform_carriers: HashMap<String, bool>,
    pending_prefix: Vec<AiStreamDelta>,
    pending_tool_deltas: HashMap<usize, Vec<AiStreamDelta>>,
    pending_tool_names: HashMap<usize, String>,
    exposed_tool_names: HashSet<String>,
    platform_tool_indices: HashSet<usize>,
    projected_thinking_items: HashSet<usize>,
    known_protected_thinking_indices: HashSet<usize>,
    streamed_protected_thinking_indices: HashSet<usize>,
    pending_protected_deltas: HashMap<usize, Vec<AiStreamDelta>>,
    prebuffered_protected_counts: HashMap<usize, usize>,
    pending_unindexed_thinking: Option<(usize, Vec<AiStreamDelta>)>,
    pending_unindexed_signature: Option<String>,
    carrier_facts: ThinkingCarrierFacts,
    next_unindexed_output_index: usize,
    model_leg_ordinal: usize,
    current_unindexed_item_kind: Option<UnindexedItemKind>,
    client_output_started: bool,
    client_output_committed: bool,
    response_started: bool,
    upload: UploadProjection,
    staged_upload_items: HashSet<usize>,
    staged_item_count: usize,
    retained_upload_items: Vec<bool>,
    hidden_upload_items: Vec<bool>,
}

impl ClientProjectionSession {
    pub(super) fn new(
        marker_store: Arc<dyn HistoryMarkerStore>,
        principal: Principal,
        ingress: stravia_runtime_contract::protocol::ids::ProtocolId,
    ) -> Self {
        Self {
            state: ProjectionState::for_ingress(ingress),
            marker_store,
            principal,
            leg_started_post_text: false,
            thinking_source: None,
            early_thinking: BTreeMap::new(),
            live_platform_carriers: HashMap::new(),
            pending_prefix: Vec::new(),
            pending_tool_deltas: HashMap::new(),
            pending_tool_names: HashMap::new(),
            exposed_tool_names: HashSet::new(),
            platform_tool_indices: HashSet::new(),
            projected_thinking_items: HashSet::new(),
            known_protected_thinking_indices: HashSet::new(),
            streamed_protected_thinking_indices: HashSet::new(),
            pending_protected_deltas: HashMap::new(),
            prebuffered_protected_counts: HashMap::new(),
            pending_unindexed_thinking: None,
            pending_unindexed_signature: None,
            carrier_facts: ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: false,
                stream_unprotected_summaries: false,
            },
            next_unindexed_output_index: 0,
            model_leg_ordinal: 0,
            current_unindexed_item_kind: None,
            client_output_started: false,
            client_output_committed: false,
            response_started: false,
            upload: UploadProjection::default(),
            staged_upload_items: HashSet::new(),
            staged_item_count: 0,
            retained_upload_items: Vec::new(),
            hidden_upload_items: Vec::new(),
        }
    }

    pub(super) fn with_upload_gateway(mut self, gateway: crate::Gateway) -> Self {
        self.upload.gateway = Some(gateway);
        self
    }

    /// Copy only at the transport boundary, after canonical persistence staging.
    pub(super) async fn prepare_upload_delivery<'a>(
        &mut self,
        response: &'a AiResponse,
    ) -> Result<std::borrow::Cow<'a, AiResponse>, HistoryMarkerError> {
        let hidden_count = response.items.len().saturating_sub(self.staged_item_count);
        let has_candidate = response.items.iter().enumerate().any(|(index, item)| {
            let eligible = if index < hidden_count {
                self.hidden_upload_items
                    .get(index)
                    .copied()
                    .unwrap_or(false)
            } else {
                self.staged_upload_items.contains(&(index - hidden_count))
            };
            eligible && UploadProjection::item_candidate(item, UPLOAD_PLACEHOLDER)
        });
        if !has_candidate {
            return Ok(std::borrow::Cow::Borrowed(response));
        }
        self.upload.refresh().await?;
        if !self.upload.enabled {
            return Ok(std::borrow::Cow::Borrowed(response));
        }
        let mut delivered = response.clone();
        for (index, item) in delivered.items.iter_mut().enumerate() {
            let eligible = if index < hidden_count {
                self.hidden_upload_items
                    .get(index)
                    .copied()
                    .unwrap_or(false)
            } else {
                self.staged_upload_items.contains(&(index - hidden_count))
            };
            if eligible {
                self.upload.replace_item(item, &self.principal)?;
            }
        }
        Ok(std::borrow::Cow::Owned(delivered))
    }

    pub(super) fn model_leg_ordinal(&self) -> usize {
        self.model_leg_ordinal
    }

    pub(super) fn begin_model_leg(
        &mut self,
        carrier_facts: ThinkingCarrierFacts,
        exposed_tool_names: impl IntoIterator<Item = String>,
        source: Option<crate::history_marker::ThinkingSource>,
    ) {
        self.state.needs_thinking_marker = self.state.openai_compatible
            || source
                .as_ref()
                .and_then(|source| source.protocol.as_ref())
                .and_then(|protocol| protocol.protocol())
                .is_some_and(|protocol| protocol != self.state.ingress.protocol);
        self.model_leg_ordinal += 1;
        self.thinking_source = source;
        self.state.begin_model_leg();
        debug_assert!(
            self.early_thinking.is_empty() && self.live_platform_carriers.is_empty(),
            "the previous Model Leg must consume its live Client Projection"
        );
        self.leg_started_post_text = self.state.post_text_started();
        self.pending_tool_deltas.clear();
        self.pending_tool_names.clear();
        self.exposed_tool_names.clear();
        self.exposed_tool_names.extend(exposed_tool_names);
        self.platform_tool_indices.clear();
        self.projected_thinking_items.clear();
        self.known_protected_thinking_indices.clear();
        self.streamed_protected_thinking_indices.clear();
        self.pending_protected_deltas.clear();
        self.prebuffered_protected_counts.clear();
        self.pending_unindexed_thinking = None;
        self.pending_unindexed_signature = None;
        self.carrier_facts = carrier_facts;
        self.next_unindexed_output_index = 0;
        self.current_unindexed_item_kind = None;
        // Client Output Commit scopes to one Model Leg: a follow-up Leg
        // produces a new response whose output has not reached the client
        // yet, so completion hooks may still rewrite it.
        self.client_output_committed = false;
    }

    fn project_live_delta(
        &mut self,
        output_index: usize,
        delta: AiStreamDelta,
    ) -> Vec<AiStreamDelta> {
        self.state.project_delta(output_index, delta)
    }

    fn begin_protected_thinking(&mut self, output_index: usize) {
        self.state.begin_protected_thinking(output_index);
    }

    fn project_protected_delta(
        &mut self,
        output_index: usize,
        delta: AiStreamDelta,
    ) -> Vec<AiStreamDelta> {
        self.state.project_protected_delta(output_index, delta)
    }

    fn synthetic_thinking_item(&self, output_index: usize) -> Option<AiItem> {
        self.state.synthetic_thinking_item(output_index)
    }

    async fn close_thinking(
        &mut self,
        output_index: usize,
        item: &AiItem,
    ) -> Result<ClosedThinking, HistoryMarkerError> {
        if self.early_thinking.contains_key(&output_index) {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        // 权威项可能在正文之后才封口；Marker 的载体仍属于思考开始时的位置。
        let post_text = self
            .state
            .pre_text_protected_previews
            .get(&output_index)
            .map_or_else(
                || self.state.post_text_started(),
                |preview| preview.post_text,
            );
        let reserved = self.state.reserved_thinking_marker(output_index).cloned();
        let preview_started = self.state.thinking_preview_started(output_index);
        let markers = self
            .persist_thinking_blocks(item, reserved.as_ref())
            .await?;
        let finish_deltas = self.state.close_thinking_preview(output_index);
        let mut preview_deltas = Vec::new();
        let mut marker_deltas = Vec::with_capacity(markers.len());
        if self.state.needs_thinking_marker
            && let MessageContent::Blocks(blocks) = &item.content
        {
            let mut markers_for_blocks = markers.iter();
            for block in blocks {
                if !is_thinking(block) {
                    continue;
                }
                let marker = markers_for_blocks
                    .next()
                    .ok_or(HistoryMarkerError::InvalidPayload)?;
                if self.state.openai_compatible && post_text {
                    if let Some(text) = public_thinking_text(block) {
                        preview_deltas.push(AiStreamDelta::TextDelta(render_quoted_preview(
                            &marker.reference,
                            &text,
                        )));
                    }
                } else {
                    preview_deltas.extend(self.state.preview_deltas(output_index, block, marker));
                }
            }
            if markers_for_blocks.next().is_some() {
                return Err(HistoryMarkerError::InvalidPayload);
            }
        }
        let mut marker_blocks = match &item.content {
            MessageContent::Blocks(blocks) => blocks.as_slice(),
            MessageContent::Text(_) => &[],
        }
        .iter()
        .filter(|block| is_thinking(block));
        for marker in &markers {
            if self.state.ingress
                == stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
            {
                let block = marker_blocks
                    .next()
                    .ok_or(HistoryMarkerError::InvalidPayload)?;
                let content_index = match block {
                    ContentBlock::Reasoning { content, .. } => content.len().saturating_sub(1),
                    _ => 0,
                };
                // Facts describe what a Model Leg can carry, not the identity
                // of the item it actually streamed. Keep the marker on the
                // preview's last carrier, including its content part.
                let carrier = finish_deltas.last().or_else(|| preview_deltas.last());
                let indexed_part = match carrier {
                    Some(AiStreamDelta::ThinkingDeltaWithMetadata {
                        output_index: Some(index),
                        content_index: Some(part),
                        ..
                    }) => Some((*index, *part)),
                    Some(AiStreamDelta::ReasoningSummaryDelta {
                        output_index: Some(index),
                        content_index: Some(_),
                        ..
                    }) => Some((*index, 0)),
                    Some(_) => None,
                    None if matches!(block, ContentBlock::Reasoning { .. }) => {
                        Some((output_index, content_index))
                    }
                    None => None,
                };
                if let Some((index, part)) = indexed_part {
                    marker_deltas.push(AiStreamDelta::ThinkingDeltaWithMetadata {
                        text: render_history_marker(marker),
                        obfuscation: None,
                        output_index: Some(index),
                        content_index: Some(part),
                    });
                } else {
                    marker_deltas.push(AiStreamDelta::ThinkingDelta(render_history_marker(marker)));
                }
                marker_deltas.push(AiStreamDelta::ItemDone {
                    index: output_index,
                    item: if indexed_part.is_some() {
                        AiItem::reasoning(Vec::new(), Vec::new(), None)
                    } else {
                        AiItem::thinking("", None)
                    },
                });
            } else {
                marker_deltas.push(marker_delta_for(
                    self.state.openai_compatible,
                    post_text,
                    render_history_marker(marker),
                ));
            }
        }
        if !markers.is_empty() {
            self.early_thinking.insert(
                output_index,
                markers
                    .iter()
                    .cloned()
                    .map(|marker| ProjectedThinkingMarker { marker, post_text })
                    .collect(),
            );
        }
        Ok(ClosedThinking {
            finish_deltas,
            preview_deltas,
            marker_deltas,
            markers,
            had_live_projection: preview_started,
        })
    }

    async fn persist_thinking_blocks(
        &self,
        item: &AiItem,
        reserved: Option<&HistoryMarker>,
    ) -> Result<Vec<HistoryMarker>, HistoryMarkerError> {
        if !self.state.needs_thinking_marker {
            return reserved
                .is_none()
                .then(Vec::new)
                .ok_or(HistoryMarkerError::InvalidPayload);
        }
        let MessageContent::Blocks(blocks) = &item.content else {
            return reserved
                .is_none()
                .then(Vec::new)
                .ok_or(HistoryMarkerError::InvalidPayload);
        };
        let mut markers = Vec::new();
        let mut reserved = reserved;
        for block in blocks.iter().filter(|block| is_thinking(block)) {
            let marker = self
                .persist_thinking_block(
                    block.clone(),
                    reserved.take(),
                    crate::history_marker::ThinkingSource::from_item(item)
                        .or_else(|| self.thinking_source.clone()),
                )
                .await?;
            markers.push(marker);
        }
        if reserved.is_some() {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        Ok(markers)
    }

    fn project_platform_marker_delta(
        &mut self,
        reference: &str,
        rendered: String,
    ) -> AiStreamDelta {
        let post_text = self.state.post_text_started();
        self.live_platform_carriers
            .insert(reference.to_owned(), post_text);
        self.state.marker_delta(rendered)
    }

    async fn publish(&self, references: &[String]) -> Result<(), HistoryMarkerError> {
        self.marker_store
            .publish(&self.principal, references, PUBLISHED_MARKER_RETENTION)
            .await
    }

    /// Whether the current Model Leg's client output has been committed.
    ///
    /// `report_delivery` is the only place this flips: a `Sent` batch that
    /// carried visible deltas commits the current Model Leg's client output.
    /// `Cancelled` deliveries, publish failures, and empty batches never
    /// commit; `begin_model_leg` resets the flag for the next response.
    pub(super) fn client_output_committed(&self) -> bool {
        self.client_output_committed
    }

    pub(super) async fn report_delivery(
        &mut self,
        batch: ProjectedDeltaBatch,
        outcome: ProjectionDelivery,
    ) -> Result<Vec<String>, HistoryMarkerError> {
        if outcome == ProjectionDelivery::Cancelled {
            self.abandon_live_projection();
            return Ok(Vec::new());
        }
        let has_visible = !batch.is_empty();
        if batch.references.is_empty() {
            self.client_output_committed |= has_visible;
            return Ok(Vec::new());
        }
        let references = batch
            .references
            .iter()
            .map(|reference| reference.reference.clone())
            .collect::<Vec<_>>();
        if let Err(error) = self.publish(&references).await {
            self.abandon_live_projection();
            return Err(error);
        }
        self.client_output_committed |= has_visible;
        Ok(batch
            .references
            .into_iter()
            .filter(|reference| reference.platform)
            .map(|reference| reference.reference)
            .collect())
    }

    fn abandon_live_projection(&mut self) {
        self.state.abandon_live_projection();
        self.early_thinking.clear();
        self.live_platform_carriers.clear();
        self.pending_protected_deltas.clear();
        self.pending_unindexed_thinking = None;
        self.pending_unindexed_signature = None;
    }

    pub(super) fn project_platform_marker(
        &mut self,
        marker: &HistoryMarker,
    ) -> ProjectedDeltaBatch {
        let delta =
            self.project_platform_marker_delta(&marker.reference, render_history_marker(marker));
        ProjectedDeltaBatch {
            deltas: self.commit_visible(vec![delta]),
            references: vec![ProjectedMarkerReference {
                reference: marker.reference.clone(),
                platform: true,
            }],
        }
    }

    pub(super) async fn project_live_deltas(
        &mut self,
        mut deltas: Vec<AiStreamDelta>,
        model_leg_completed: bool,
    ) -> Result<Vec<ProjectedDeltaBatch>, HistoryMarkerError> {
        deltas = self.filter_platform_deltas(deltas);
        if !self.upload.pending.is_empty() || deltas.iter().any(UploadProjection::candidate) {
            self.upload.refresh().await?;
        }
        let mut ready = Vec::new();
        for delta in deltas {
            ready.extend(self.upload.push(delta, &self.principal)?);
        }
        if model_leg_completed {
            ready.extend(self.upload.flush(&self.principal)?);
        }
        deltas = ready;
        let has_completed_thinking = deltas.iter().any(|delta| {
            matches!(
                delta,
                AiStreamDelta::ItemDone { item, .. } if is_thinking_item(item)
            )
        });
        let mut batches = Vec::new();
        for delta in deltas {
            self.capture_protected_candidates(std::slice::from_ref(&delta));
            let Some(delta) = self.capture_unindexed_signature(delta) else {
                continue;
            };
            if !has_completed_thinking
                && Self::ends_unindexed_thinking(&delta)
                && let Some((index, item)) = self
                    .synthetic_buffered_thinking_item()
                    .or_else(|| self.synthetic_post_text_thinking_item())
            {
                let (deltas, markers) = self.close_live_thinking(index, &item).await?;
                if !deltas.is_empty() {
                    batches.push(ProjectedDeltaBatch {
                        deltas: self.commit_visible(deltas),
                        references: markers
                            .into_iter()
                            .map(|marker| ProjectedMarkerReference {
                                reference: marker.reference,
                                platform: false,
                            })
                            .collect(),
                    });
                }
                self.current_unindexed_item_kind = None;
            }
            if let AiStreamDelta::ItemDone { index, item } = &delta
                && is_thinking_item(item)
            {
                let (deltas, markers) = self.close_live_thinking(*index, item).await?;
                if !deltas.is_empty() {
                    batches.push(ProjectedDeltaBatch {
                        deltas: self.commit_visible(deltas),
                        references: markers
                            .into_iter()
                            .map(|marker| ProjectedMarkerReference {
                                reference: marker.reference,
                                platform: false,
                            })
                            .collect(),
                    });
                }
            }
            let visible = self.filter_live_deltas(std::iter::once(delta));
            if !visible.is_empty() {
                batches.push(ProjectedDeltaBatch::visible(visible));
            }
        }
        if model_leg_completed
            && !has_completed_thinking
            && let Some((index, item)) = self
                .synthetic_buffered_thinking_item()
                .or_else(|| self.synthetic_post_text_thinking_item())
        {
            let (deltas, markers) = self.close_live_thinking(index, &item).await?;
            if !deltas.is_empty() {
                batches.push(ProjectedDeltaBatch {
                    deltas: self.commit_visible(deltas),
                    references: markers
                        .into_iter()
                        .map(|marker| ProjectedMarkerReference {
                            reference: marker.reference,
                            platform: false,
                        })
                        .collect(),
                });
            }
            self.current_unindexed_item_kind = None;
        }
        Ok(batches)
    }

    pub(super) fn complete_live_model_leg(&mut self) -> ProjectedDeltaBatch {
        debug_assert!(
            self.upload.queue.is_empty(),
            "completed Model Leg flushes upload delivery prefixes"
        );
        let pending_thinking = self.flush_unindexed_thinking();
        ProjectedDeltaBatch::visible(self.route_visible_deltas(pending_thinking))
    }

    fn observe_unindexed_item(&mut self, kind: UnindexedItemKind) -> usize {
        if self.current_unindexed_item_kind != Some(kind) {
            self.current_unindexed_item_kind = Some(kind);
            let index = self.next_unindexed_output_index;
            self.next_unindexed_output_index = self.next_unindexed_output_index.saturating_add(1);
            index
        } else {
            self.next_unindexed_output_index.saturating_sub(1)
        }
    }

    fn capture_unindexed_signature(&mut self, delta: AiStreamDelta) -> Option<AiStreamDelta> {
        if self.pending_unindexed_thinking.is_none() {
            return Some(delta);
        }
        match delta {
            AiStreamDelta::ThinkingSignature(signature) => {
                if !signature.is_empty() {
                    self.pending_unindexed_signature
                        .get_or_insert_with(String::new)
                        .push_str(&signature);
                }
                self.pending_unindexed_thinking
                    .as_mut()
                    .expect("pending unindexed Thinking remains present")
                    .1
                    .push(AiStreamDelta::ThinkingSignature(signature));
                None
            }
            other => Some(other),
        }
    }

    fn synthetic_buffered_thinking_item(&self) -> Option<(usize, AiItem)> {
        let signature = self
            .pending_unindexed_signature
            .as_ref()
            .filter(|value| !value.is_empty());
        if signature.is_none() && !self.state.needs_thinking_marker {
            return None;
        }
        let (index, deltas) = self.pending_unindexed_thinking.as_ref()?;
        let thinking = deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        Some((*index, AiItem::thinking(thinking, signature.cloned())))
    }

    fn synthetic_post_text_thinking_item(&self) -> Option<(usize, AiItem)> {
        if self.current_unindexed_item_kind != Some(UnindexedItemKind::Thinking) {
            return None;
        }
        let index = self.next_unindexed_output_index.saturating_sub(1);
        self.synthetic_thinking_item(index)
            .map(|item| (index, item))
    }

    fn protected_candidate_index(&mut self, delta: &AiStreamDelta) -> Option<usize> {
        match delta {
            AiStreamDelta::ThinkingDeltaWithMetadata {
                output_index: Some(index),
                ..
            }
            | AiStreamDelta::ReasoningSummaryDelta {
                output_index: Some(index),
                ..
            } if self.carrier_facts.may_be_protected => Some(*index),
            AiStreamDelta::ThinkingDelta(_)
            | AiStreamDelta::ThinkingDeltaWithMetadata {
                output_index: None, ..
            }
            | AiStreamDelta::ReasoningSummaryDelta {
                output_index: None, ..
            } if self.carrier_facts.may_be_protected => {
                Some(self.observe_unindexed_item(UnindexedItemKind::Thinking))
            }
            _ => None,
        }
    }

    fn streams_unprotected_reasoning_summary(&self, index: usize, delta: &AiStreamDelta) -> bool {
        self.carrier_facts.stream_unprotected_summaries
            && (!self.state.needs_thinking_marker || self.state.openai_compatible)
            && !self.known_protected_thinking_indices.contains(&index)
            && matches!(delta, AiStreamDelta::ReasoningSummaryDelta { .. })
    }

    fn capture_protected_candidates(&mut self, deltas: &[AiStreamDelta]) {
        for delta in deltas {
            if let AiStreamDelta::ProtectedThinkingStart { index } = delta {
                self.known_protected_thinking_indices.insert(*index);
                continue;
            }
            let Some(index) = self.protected_candidate_index(delta) else {
                continue;
            };
            if self.streams_unprotected_reasoning_summary(index, delta) {
                continue;
            }
            if matches!(
                delta,
                AiStreamDelta::ThinkingDeltaWithMetadata {
                    output_index: Some(_),
                    ..
                } | AiStreamDelta::ReasoningSummaryDelta {
                    output_index: Some(_),
                    ..
                }
            ) {
                self.pending_protected_deltas
                    .entry(index)
                    .or_default()
                    .push(delta.clone());
            } else {
                match self.pending_unindexed_thinking.as_mut() {
                    Some((pending_index, pending)) if *pending_index == index => {
                        pending.push(delta.clone());
                    }
                    _ => self.pending_unindexed_thinking = Some((index, vec![delta.clone()])),
                }
            }
            *self.prebuffered_protected_counts.entry(index).or_default() += 1;
        }
    }

    async fn close_live_thinking(
        &mut self,
        index: usize,
        item: &AiItem,
    ) -> Result<(Vec<AiStreamDelta>, Vec<HistoryMarker>), HistoryMarkerError> {
        self.known_protected_thinking_indices.remove(&index);
        let streamed_protected = self.streamed_protected_thinking_indices.remove(&index);
        let pending = if let Some(pending) = self.pending_protected_deltas.remove(&index) {
            Some(pending)
        } else if self
            .pending_unindexed_thinking
            .as_ref()
            .is_some_and(|(pending_index, _)| *pending_index == index)
        {
            self.pending_unindexed_signature = None;
            self.current_unindexed_item_kind = None;
            self.pending_unindexed_thinking
                .take()
                .map(|(_, deltas)| deltas)
        } else {
            None
        };
        if self.current_unindexed_item_kind == Some(UnindexedItemKind::Thinking)
            && self.next_unindexed_output_index.checked_sub(1) == Some(index)
        {
            self.current_unindexed_item_kind = None;
        }
        let closed = self.close_thinking(index, item).await?;
        if closed.had_live_projection || !closed.markers.is_empty() {
            self.projected_thinking_items.insert(index);
        }
        let mut deltas = closed.finish_deltas;
        if closed.markers.is_empty() {
            if !streamed_protected && let Some(pending) = pending {
                deltas.extend(pending);
            }
            return Ok((deltas, closed.markers));
        }
        if !closed.had_live_projection && !streamed_protected {
            deltas.extend(closed.preview_deltas);
        }
        deltas.extend(closed.marker_deltas);
        Ok((deltas, closed.markers))
    }

    fn commit_visible(&mut self, mut deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
        if !self.client_output_started {
            self.client_output_started = true;
            self.pending_prefix.append(&mut deltas);
            let committed = std::mem::take(&mut self.pending_prefix);
            self.response_started |= committed
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::MessageStart { .. }));
            committed
        } else {
            deltas
        }
    }

    fn unindexed_item_kind(delta: &AiStreamDelta) -> Option<UnindexedItemKind> {
        match delta {
            AiStreamDelta::TextDelta(_)
            | AiStreamDelta::TextDeltaWithMetadata {
                output_index: None, ..
            }
            | AiStreamDelta::RefusalDelta(_)
            | AiStreamDelta::RefusalDeltaWithIndex { .. } => Some(UnindexedItemKind::Text),
            AiStreamDelta::ThinkingDelta(_)
            | AiStreamDelta::ThinkingDeltaWithMetadata {
                output_index: None, ..
            }
            | AiStreamDelta::ReasoningSummaryDelta {
                output_index: None, ..
            } => Some(UnindexedItemKind::Thinking),
            AiStreamDelta::ToolCallStart { .. }
            | AiStreamDelta::ToolCallDelta { .. }
            | AiStreamDelta::ToolCallComplete { .. } => Some(UnindexedItemKind::Tool),
            _ => None,
        }
    }

    fn ends_unindexed_thinking(delta: &AiStreamDelta) -> bool {
        // Transport/response bookkeeping can arrive between public Thinking
        // and its late signature. Only a semantic item/turn boundary closes it.
        // Unknown events are prefix carriers here; typed ItemDone carries any
        // opaque content item's actual boundary.
        !matches!(
            delta,
            AiStreamDelta::MessageStart { .. }
                | AiStreamDelta::ResponseMetadata { .. }
                | AiStreamDelta::ProtectedThinkingStart { .. }
                | AiStreamDelta::Usage(_)
                | AiStreamDelta::ResponseTerminal { .. }
                | AiStreamDelta::Unknown { .. }
                | AiStreamDelta::ThinkingDelta(_)
                | AiStreamDelta::ThinkingDeltaWithMetadata {
                    output_index: None,
                    ..
                }
                | AiStreamDelta::ReasoningSummaryDelta {
                    output_index: None,
                    ..
                }
                | AiStreamDelta::ThinkingSignature(_)
        )
    }

    fn flush_unindexed_thinking(&mut self) -> Vec<AiStreamDelta> {
        self.pending_unindexed_signature = None;
        self.pending_unindexed_thinking
            .take()
            .map(|(_, deltas)| deltas)
            .unwrap_or_default()
    }

    fn route_visible_deltas(&mut self, deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for delta in deltas {
            let prefix_only = matches!(
                delta,
                AiStreamDelta::MessageStart { .. }
                    | AiStreamDelta::ResponseMetadata { .. }
                    | AiStreamDelta::Usage(_)
                    | AiStreamDelta::ResponseTerminal { .. }
                    | AiStreamDelta::Unknown { .. }
            );
            if !self.client_output_started && prefix_only {
                self.pending_prefix.push(delta);
                continue;
            }
            if !self.client_output_started {
                self.client_output_started = true;
                visible.append(&mut self.pending_prefix);
            }
            let output_index = match &delta {
                AiStreamDelta::ThinkingDeltaWithMetadata {
                    output_index: Some(index),
                    ..
                }
                | AiStreamDelta::ReasoningSummaryDelta {
                    output_index: Some(index),
                    ..
                }
                | AiStreamDelta::ItemDone { index, .. } => *index,
                AiStreamDelta::ThinkingDelta(_)
                | AiStreamDelta::ThinkingDeltaWithMetadata {
                    output_index: None, ..
                }
                | AiStreamDelta::ReasoningSummaryDelta {
                    output_index: None, ..
                } => self.observe_unindexed_item(UnindexedItemKind::Thinking),
                _ => self.next_unindexed_output_index,
            };
            visible.extend(self.project_live_delta(output_index, delta));
        }
        self.response_started |= visible
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::MessageStart { .. }));
        visible
    }

    fn filter_platform_deltas(&mut self, deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for delta in deltas {
            match &delta {
                AiStreamDelta::ToolCallStart { index, name, .. } => {
                    if self.pending_tool_deltas.contains_key(index) {
                        let index = *index;
                        let accumulated = self.pending_tool_names.entry(index).or_default();
                        accumulated.push_str(name);
                        let is_platform = self.exposed_tool_names.contains(accumulated);
                        let remains_ambiguous = self
                            .exposed_tool_names
                            .iter()
                            .any(|registered| registered.starts_with(accumulated.as_str()));
                        self.pending_tool_deltas
                            .entry(index)
                            .or_default()
                            .push(delta);
                        if is_platform {
                            self.pending_tool_deltas.remove(&index);
                            self.pending_tool_names.remove(&index);
                            self.platform_tool_indices.insert(index);
                        } else if !remains_ambiguous {
                            if let Some(pending) = self.pending_tool_deltas.remove(&index) {
                                visible.extend(pending);
                            }
                            self.pending_tool_names.remove(&index);
                        }
                        continue;
                    }
                    if self.exposed_tool_names.contains(name) {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if self
                        .exposed_tool_names
                        .iter()
                        .any(|registered| registered.starts_with(name))
                    {
                        self.pending_tool_names.insert(*index, name.clone());
                        self.pending_tool_deltas
                            .entry(*index)
                            .or_default()
                            .push(delta);
                        continue;
                    }
                }
                AiStreamDelta::ToolCallDelta { index, .. }
                    if self.pending_tool_deltas.contains_key(index) =>
                {
                    self.pending_tool_deltas
                        .entry(*index)
                        .or_default()
                        .push(delta);
                    continue;
                }
                AiStreamDelta::ToolCallComplete { index, tool_call } => {
                    if self.exposed_tool_names.contains(&tool_call.name) {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if let Some(pending) = self.pending_tool_deltas.remove(index) {
                        visible.extend(pending);
                    }
                    self.pending_tool_names.remove(index);
                }
                AiStreamDelta::ItemDone { index, item } => {
                    let platform = item
                        .function_call_ref()
                        .is_some_and(|call| self.exposed_tool_names.contains(&call.name));
                    if platform {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if let Some(pending) = self.pending_tool_deltas.remove(index) {
                        visible.extend(pending);
                    }
                    self.pending_tool_names.remove(index);
                }
                _ => {}
            }
            let hidden_platform_delta = match &delta {
                AiStreamDelta::ToolCallStart { index, name, .. }
                    if self.exposed_tool_names.contains(name) =>
                {
                    self.platform_tool_indices.insert(*index);
                    true
                }
                AiStreamDelta::ToolCallDelta { index, .. } => {
                    self.platform_tool_indices.contains(index)
                }
                AiStreamDelta::ToolCallComplete { index, tool_call } => {
                    let hidden = self.platform_tool_indices.contains(index)
                        || self.exposed_tool_names.contains(&tool_call.name);
                    if hidden {
                        self.platform_tool_indices.insert(*index);
                    }
                    hidden
                }
                AiStreamDelta::ItemDone { index, item } => {
                    let hidden = self.platform_tool_indices.contains(index)
                        || item
                            .function_call_ref()
                            .is_some_and(|call| self.exposed_tool_names.contains(&call.name));
                    if hidden {
                        self.platform_tool_indices.insert(*index);
                    }
                    hidden
                }
                _ => false,
            };
            if hidden_platform_delta {
                continue;
            }
            visible.push(delta);
        }
        visible
    }

    fn filter_live_deltas(
        &mut self,
        deltas: impl IntoIterator<Item = AiStreamDelta>,
    ) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for mut delta in deltas {
            if matches!(
                delta,
                AiStreamDelta::ProtectedThinkingStart { .. } | AiStreamDelta::Usage(_)
            ) {
                continue;
            }
            if let Some(index) = self.protected_candidate_index(&delta) {
                if self.streams_unprotected_reasoning_summary(index, &delta) {
                    visible.extend(self.route_visible_deltas(vec![delta]));
                    continue;
                }
                let prebuffered =
                    if let Some(count) = self.prebuffered_protected_counts.get(&index).copied() {
                        if count <= 1 {
                            self.prebuffered_protected_counts.remove(&index);
                        } else {
                            self.prebuffered_protected_counts.insert(index, count - 1);
                        }
                        true
                    } else {
                        false
                    };
                if !prebuffered {
                    if matches!(
                        delta,
                        AiStreamDelta::ThinkingDeltaWithMetadata {
                            output_index: Some(_),
                            ..
                        } | AiStreamDelta::ReasoningSummaryDelta {
                            output_index: Some(_),
                            ..
                        }
                    ) {
                        self.pending_protected_deltas
                            .entry(index)
                            .or_default()
                            .push(delta.clone());
                    } else {
                        match self.pending_unindexed_thinking.as_mut() {
                            Some((pending_index, pending)) if *pending_index == index => {
                                pending.push(delta.clone())
                            }
                            _ => {
                                self.pending_unindexed_thinking = Some((index, vec![delta.clone()]))
                            }
                        }
                    }
                }
                if self.state.post_text_started() {
                    visible.extend(self.project_live_delta(index, delta));
                } else if (self.state.needs_thinking_marker && !self.state.openai_compatible)
                    || self.known_protected_thinking_indices.contains(&index)
                {
                    self.streamed_protected_thinking_indices.insert(index);
                    self.begin_protected_thinking(index);
                    let projected = self.project_protected_delta(index, delta);
                    visible.extend(self.commit_visible(projected));
                }
                continue;
            }
            let kind = Self::unindexed_item_kind(&delta);
            if Self::ends_unindexed_thinking(&delta)
                && self.pending_unindexed_signature.is_none()
                && self.pending_unindexed_thinking.is_some()
            {
                let pending = self.flush_unindexed_thinking();
                visible.extend(self.route_visible_deltas(pending));
            }
            if let Some(kind) = kind
                && self.current_unindexed_item_kind != Some(kind)
            {
                self.observe_unindexed_item(kind);
            }
            if self.response_started
                && matches!(
                    delta,
                    AiStreamDelta::MessageStart { .. } | AiStreamDelta::ResponseMetadata { .. }
                )
            {
                // 后续 Model Leg 的 profile 仍不能覆盖已公开的消息头，但明确
                // empty/null reasoning 是消息字段更新，不能随重复头一起丢弃。
                let AiStreamDelta::ResponseMetadata { metadata } = &mut delta else {
                    continue;
                };
                let key =
                    stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;
                let Some(fields) = metadata.as_object_mut() else {
                    continue;
                };
                if fields
                    .get(key)
                    .filter(|value| value.is_null() || value.as_str().is_some_and(str::is_empty))
                    .is_none()
                {
                    continue;
                }
                fields.retain(|field, _| field == key);
            }
            if matches!(&delta, AiStreamDelta::ItemDone { index, .. } if self.projected_thinking_items.remove(index))
            {
                continue;
            }
            visible.extend(self.route_visible_deltas(vec![delta]));
        }
        visible
    }

    /// Project a completed response into the staged client shape.
    ///
    /// Returns the Marker batch the caller must hand to `report_delivery` once
    /// the projected bytes are confirmed Sent. The batch is a value, not
    /// session state, so a settlement path cannot silently skip the publish.
    pub(super) async fn project_staged(
        &mut self,
        response: &mut AiResponse,
        platform: &[(&str, &HistoryMarker)],
    ) -> Result<ProjectedDeltaBatch, HistoryMarkerError> {
        let by_call_id = platform
            .iter()
            .copied()
            .collect::<HashMap<&str, &HistoryMarker>>();
        let mut post_text = self.leg_started_post_text;
        let mut projected = Vec::with_capacity(response.items.len() + platform.len());
        let mut staged_deltas = Vec::new();
        let mut staged_references = Vec::new();
        self.hidden_upload_items
            .append(&mut self.retained_upload_items);
        self.staged_upload_items.clear();

        for (output_index, mut item) in std::mem::take(&mut response.items).into_iter().enumerate()
        {
            if item
                .function_call_output_ref()
                .is_some_and(|(call_id, _)| by_call_id.contains_key(call_id))
            {
                continue;
            }
            if item.role != Role::Assistant {
                projected.push(item);
                continue;
            }

            let projected_start = projected.len();
            let mut prepared = self
                .early_thinking
                .remove(&output_index)
                .unwrap_or_default();
            let source = crate::history_marker::ThinkingSource::from_item(&item)
                .or_else(|| self.thinking_source.clone());
            let mut meta = item.meta.take();
            match std::mem::replace(
                &mut item.content,
                MessageContent::Text(std::sync::Arc::new(String::new())),
            ) {
                MessageContent::Text(text) => {
                    if !text.is_empty() {
                        self.staged_upload_items.insert(projected.len());
                        projected.push(AiItem {
                            role: Role::Assistant,
                            content: MessageContent::Text(text),
                            tool_calls: None,
                            tool_call_id: None,
                            meta: meta.take(),
                        });
                        post_text = true;
                    }
                }
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        if !is_thinking(&block) {
                            if matches!(&block, ContentBlock::Text { text, .. } if !text.is_empty())
                            {
                                post_text = true;
                                self.staged_upload_items.insert(projected.len());
                            }
                            push_projection_block(&mut projected, block, &mut meta);
                            continue;
                        }

                        let recorded = prepared.front().cloned();
                        let block_post_text =
                            recorded.as_ref().map_or(post_text, |entry| entry.post_text);
                        let needs_marker = self.state.needs_thinking_marker;
                        if !needs_marker {
                            push_projection_block(&mut projected, block, &mut meta);
                            continue;
                        }
                        if recorded
                            .as_ref()
                            .is_some_and(|entry| entry.post_text != post_text)
                        {
                            return Err(HistoryMarkerError::InvalidPayload);
                        }
                        let (marker, newly_persisted) = if let Some(entry) = prepared.pop_front() {
                            (entry.marker, false)
                        } else {
                            let marker = self
                                .persist_thinking_block(block.clone(), None, source.clone())
                                .await?;
                            (marker, true)
                        };
                        if self.state.ingress
                            == stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
                        {
                            let mut carrier =
                                self.state.responses_thinking_carrier(&block, &marker);
                            carrier.meta = meta.take();
                            projected.push(carrier);
                        } else if self.state.openai_compatible && block_post_text {
                            if let Some(mut preview) = self.state.post_text_preview(&block, &marker)
                            {
                                preview.meta = meta.take();
                                projected.push(preview);
                            }
                        } else if let Some(visible) =
                            self.state.visible_protected_block(&block, &marker)
                        {
                            push_projection_block(&mut projected, visible, &mut meta);
                        }
                        if self.state.ingress
                            != stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
                        {
                            let mut marker_item = marker_item_for(
                                self.state.openai_compatible,
                                block_post_text,
                                &marker,
                            );
                            marker_item.meta = meta.take();
                            projected.push(marker_item);
                        }
                        if newly_persisted {
                            staged_deltas.push(marker_delta_for(
                                self.state.openai_compatible,
                                block_post_text,
                                render_history_marker(&marker),
                            ));
                            staged_references.push(ProjectedMarkerReference {
                                reference: marker.reference,
                                platform: false,
                            });
                        }
                    }
                }
            }

            if let Some(calls) = item.tool_calls.take() {
                for call in calls {
                    let mut call_item = if let Some(marker) = by_call_id.get(call.id.as_str()) {
                        let live_marker_post_text =
                            self.live_platform_carriers.remove(&marker.reference);
                        let marker_post_text = live_marker_post_text.unwrap_or(post_text);
                        if marker_post_text != post_text {
                            return Err(HistoryMarkerError::InvalidPayload);
                        }
                        if live_marker_post_text.is_none() {
                            staged_deltas.push(marker_delta_for(
                                self.state.openai_compatible,
                                marker_post_text,
                                render_history_marker(marker),
                            ));
                            staged_references.push(ProjectedMarkerReference {
                                reference: marker.reference.clone(),
                                platform: true,
                            });
                        }
                        marker_item_for(self.state.openai_compatible, marker_post_text, marker)
                    } else {
                        self.staged_upload_items.insert(projected.len());
                        AiItem::function_call(call)
                    };
                    call_item.meta = meta.take();
                    projected.push(call_item);
                }
            }
            if !prepared.is_empty() {
                return Err(HistoryMarkerError::InvalidPayload);
            }
            if projected.len() == projected_start && meta.is_some() {
                item.meta = meta;
                projected.push(item);
            }
        }

        if !self.early_thinking.is_empty() || !self.live_platform_carriers.is_empty() {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        if post_text {
            self.state.post_text_started = true;
        }
        self.staged_item_count = projected.len();
        self.retained_upload_items = projected
            .iter()
            .enumerate()
            .filter(|(_, item)| super::ledger::retain_hidden_round_item(item))
            .map(|(index, _)| self.staged_upload_items.contains(&index))
            .collect();
        response.items = projected;
        Ok(ProjectedDeltaBatch {
            deltas: staged_deltas,
            references: staged_references,
        })
    }

    async fn persist_thinking_block(
        &self,
        block: ContentBlock,
        reserved: Option<&HistoryMarker>,
        source: Option<crate::history_marker::ThinkingSource>,
    ) -> Result<HistoryMarker, HistoryMarkerError> {
        let input = ThinkingMarkerInput {
            block,
            source,
            activity: "Preserving protected reasoning".into(),
            pending_retention: THINKING_MARKER_PENDING_RETENTION,
        };
        if let Some(reserved) = reserved {
            self.marker_store
                .create_reserved_thinking(&self.principal, reserved, input)
                .await
        } else {
            self.marker_store
                .create_thinking(&self.principal, input)
                .await
        }
    }
}

fn marker_item_for(openai_compatible: bool, post_text: bool, marker: &HistoryMarker) -> AiItem {
    let rendered = render_history_marker(marker);
    if openai_compatible && post_text {
        AiItem::output_text(rendered)
    } else {
        AiItem::thinking(rendered, None)
    }
}

fn marker_delta_for(openai_compatible: bool, post_text: bool, rendered: String) -> AiStreamDelta {
    if openai_compatible && post_text {
        AiStreamDelta::TextDelta(rendered)
    } else {
        AiStreamDelta::ThinkingDelta(rendered)
    }
}

fn push_projection_block(
    projected: &mut Vec<AiItem>,
    block: ContentBlock,
    meta: &mut Option<Box<stravia_runtime_contract::protocol::ir::AiItemMetadata>>,
) {
    projected.push(AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![block]),
        tool_calls: None,
        tool_call_id: None,
        meta: meta.take(),
    });
}

/// Run-wide Client Projection state. `begin_model_leg` deliberately does not
/// reset `post_text_started`.
struct ProjectionState {
    ingress: stravia_runtime_contract::protocol::ids::ProtocolId,
    needs_thinking_marker: bool,
    openai_compatible: bool,
    post_text_started: bool,
    live_previews: HashMap<usize, LiveThinkingPreview>,
    pre_text_protected_previews: HashMap<usize, LiveProtectedPreview>,
}

impl Default for ProjectionState {
    fn default() -> Self {
        Self {
            ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            needs_thinking_marker: true,
            openai_compatible: true,
            post_text_started: false,
            live_previews: HashMap::new(),
            pre_text_protected_previews: HashMap::new(),
        }
    }
}

impl ProjectionState {
    fn for_ingress(ingress: stravia_runtime_contract::protocol::ids::ProtocolId) -> Self {
        Self {
            ingress,
            needs_thinking_marker: ingress == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            openai_compatible: ingress == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ..Self::default()
        }
    }

    pub(super) fn begin_model_leg(&mut self) {
        debug_assert!(
            self.live_previews.is_empty() && self.pre_text_protected_previews.is_empty(),
            "a completed Model Leg must finalize every Thinking Marker"
        );
    }

    fn abandon_live_projection(&mut self) {
        self.live_previews.clear();
        self.pre_text_protected_previews.clear();
    }

    pub(super) fn post_text_started(&self) -> bool {
        self.post_text_started
    }

    pub(super) fn observe_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.post_text_started = true;
        }
    }

    pub(super) fn project_delta(
        &mut self,
        output_index: usize,
        delta: AiStreamDelta,
    ) -> Vec<AiStreamDelta> {
        match delta {
            AiStreamDelta::TextDelta(text) => {
                self.observe_text(&text);
                vec![AiStreamDelta::TextDelta(text)]
            }
            AiStreamDelta::TextDeltaWithMetadata {
                text,
                logprobs,
                obfuscation,
                output_index,
                content_index,
            } => {
                self.observe_text(&text);
                vec![AiStreamDelta::TextDeltaWithMetadata {
                    text,
                    logprobs,
                    obfuscation,
                    output_index,
                    content_index,
                }]
            }
            delta @ (AiStreamDelta::ThinkingDelta(_)
            | AiStreamDelta::ThinkingDeltaWithMetadata { .. }
            | AiStreamDelta::ReasoningSummaryDelta { .. })
                if self.needs_thinking_marker =>
            {
                if self.pre_text_protected_previews.contains_key(&output_index)
                    || !self.openai_compatible
                    || !self.post_text_started
                {
                    self.begin_protected_thinking(output_index);
                    return self.project_protected_delta(output_index, delta);
                }
                let (carrier, text) = match delta {
                    AiStreamDelta::ThinkingDelta(text) => (PreviewCarrier::Unindexed, text),
                    AiStreamDelta::ThinkingDeltaWithMetadata {
                        text,
                        output_index,
                        content_index,
                        ..
                    } => (
                        PreviewCarrier::Indexed {
                            output_index,
                            content_index,
                            summary: false,
                        },
                        text,
                    ),
                    AiStreamDelta::ReasoningSummaryDelta {
                        text,
                        output_index,
                        content_index,
                        ..
                    } => (
                        PreviewCarrier::Indexed {
                            output_index,
                            content_index,
                            summary: true,
                        },
                        text,
                    ),
                    _ => unreachable!(),
                };
                self.project_thinking_delta(output_index, carrier, text)
            }
            AiStreamDelta::ThinkingSignature(_) if self.needs_thinking_marker => Vec::new(),
            other => vec![other],
        }
    }

    pub(super) fn begin_protected_thinking(&mut self, output_index: usize) {
        if self.needs_thinking_marker && (!self.openai_compatible || !self.post_text_started) {
            self.pre_text_protected_previews
                .entry(output_index)
                .or_insert_with(|| LiveProtectedPreview {
                    marker: crate::history_marker::reserve_thinking_marker(),
                    post_text: self.post_text_started,
                    carrier: None,
                    ordinal: 0,
                    canonical_text: String::new(),
                });
        }
    }

    pub(super) fn project_protected_delta(
        &mut self,
        output_index: usize,
        delta: AiStreamDelta,
    ) -> Vec<AiStreamDelta> {
        if !self.needs_thinking_marker {
            return vec![delta];
        }
        if self.openai_compatible
            && self.post_text_started
            && !self.pre_text_protected_previews.contains_key(&output_index)
        {
            return self.project_delta(output_index, delta);
        }
        let preview = self
            .pre_text_protected_previews
            .get_mut(&output_index)
            .expect("protected Thinking start precedes its public deltas");
        let (carrier, text, obfuscation) = match delta {
            AiStreamDelta::ThinkingDelta(text) => (ProtectedPreviewCarrier::Unindexed, text, None),
            AiStreamDelta::ThinkingDeltaWithMetadata {
                text,
                obfuscation,
                output_index,
                content_index,
            } => (
                ProtectedPreviewCarrier::Thinking {
                    output_index,
                    content_index,
                },
                text,
                obfuscation,
            ),
            AiStreamDelta::ReasoningSummaryDelta {
                text,
                obfuscation,
                output_index,
                content_index,
            } => (
                ProtectedPreviewCarrier::Summary {
                    output_index,
                    content_index,
                },
                text,
                obfuscation,
            ),
            other => return vec![other],
        };
        if text.is_empty() {
            return Vec::new();
        }
        preview.canonical_text.push_str(&text);
        let mut projected = Vec::with_capacity(2);
        if let Some(previous) = preview.carrier
            && previous != carrier
        {
            projected.push(previous.delta(
                format!(
                    "\n\n{}",
                    render_preview_projection_end(&preview.marker.reference, preview.ordinal)
                ),
                None,
            ));
            preview.ordinal += 1;
        }
        let text = if preview.carrier == Some(carrier) {
            text
        } else {
            format!(
                "{}\n\n{text}",
                render_preview_projection_start(&preview.marker.reference, preview.ordinal)
            )
        };
        preview.carrier = Some(carrier);
        projected.push(carrier.delta(text, obfuscation));
        projected
    }

    fn project_thinking_delta(
        &mut self,
        output_index: usize,
        carrier: PreviewCarrier,
        text: String,
    ) -> Vec<AiStreamDelta> {
        if text.is_empty() {
            return Vec::new();
        }
        let preview = self.live_previews.entry(output_index).or_insert_with(|| {
            let marker = crate::history_marker::reserve_thinking_marker();
            LiveThinkingPreview {
                encoder: QuotedThinkingPreviewEncoder::new(marker.reference.clone()),
                marker,
                carrier,
                canonical_text: String::new(),
            }
        });
        let new_part = preview.carrier != carrier;
        preview.carrier = carrier;
        preview.canonical_text.push_str(&text);
        let projected = if new_part {
            let mut projected = preview.encoder.push("\n\n");
            projected.push_str(&preview.encoder.push(&text));
            projected
        } else {
            preview.encoder.push(&text)
        };
        (!projected.is_empty())
            .then(|| carrier.text_delta(projected))
            .into_iter()
            .collect()
    }

    pub(super) fn reserved_thinking_marker(&self, output_index: usize) -> Option<&HistoryMarker> {
        self.live_previews
            .get(&output_index)
            .map(|preview| &preview.marker)
            .or_else(|| {
                self.pre_text_protected_previews
                    .get(&output_index)
                    .map(|preview| &preview.marker)
            })
    }

    pub(super) fn thinking_preview_started(&self, output_index: usize) -> bool {
        self.live_previews.contains_key(&output_index)
            || self
                .pre_text_protected_previews
                .get(&output_index)
                .is_some_and(|preview| preview.carrier.is_some())
    }

    pub(super) fn synthetic_thinking_item(&self, output_index: usize) -> Option<AiItem> {
        self.live_previews
            .get(&output_index)
            .map(|preview| AiItem::thinking(preview.canonical_text.clone(), None))
            .or_else(|| {
                self.pre_text_protected_previews
                    .get(&output_index)
                    .map(|preview| AiItem::thinking(preview.canonical_text.clone(), None))
            })
    }

    pub(super) fn close_thinking_preview(&mut self, output_index: usize) -> Vec<AiStreamDelta> {
        if let Some(preview) = self.live_previews.remove(&output_index) {
            return vec![preview.carrier.text_delta(preview.encoder.finish())];
        }
        self.pre_text_protected_previews
            .remove(&output_index)
            .and_then(|preview| {
                preview.carrier.map(|carrier| {
                    carrier.delta(
                        format!(
                            "\n\n{}",
                            render_preview_projection_end(
                                &preview.marker.reference,
                                preview.ordinal
                            )
                        ),
                        None,
                    )
                })
            })
            .into_iter()
            .collect()
    }

    pub(super) fn preview_deltas(
        &self,
        output_index: usize,
        block: &ContentBlock,
        marker: &HistoryMarker,
    ) -> Vec<AiStreamDelta> {
        let Some(visible) = self.visible_protected_block(block, marker) else {
            return Vec::new();
        };
        match visible {
            ContentBlock::Thinking { thinking, .. } => {
                vec![AiStreamDelta::ThinkingDelta(thinking)]
            }
            ContentBlock::Reasoning {
                summary, content, ..
            } => summary
                .into_iter()
                .enumerate()
                .map(
                    |(content_index, text)| AiStreamDelta::ReasoningSummaryDelta {
                        text,
                        obfuscation: None,
                        output_index: Some(output_index),
                        content_index: Some(content_index),
                    },
                )
                .chain(
                    content
                        .into_iter()
                        .enumerate()
                        .map(
                            |(content_index, text)| AiStreamDelta::ThinkingDeltaWithMetadata {
                                text,
                                obfuscation: None,
                                output_index: Some(output_index),
                                content_index: Some(content_index),
                            },
                        ),
                )
                .collect(),
            other => unreachable!("protected preview remains Thinking: {other:?}"),
        }
    }

    pub(super) fn marker_delta(&self, rendered: String) -> AiStreamDelta {
        if self.openai_compatible && self.post_text_started {
            AiStreamDelta::TextDelta(rendered)
        } else {
            AiStreamDelta::ThinkingDelta(rendered)
        }
    }

    pub(super) fn visible_protected_block(
        &self,
        block: &ContentBlock,
        marker: &HistoryMarker,
    ) -> Option<ContentBlock> {
        let mut visible = match block {
            ContentBlock::Thinking {
                thinking,
                signature: Some(_),
            } => ContentBlock::Thinking {
                thinking: thinking.clone(),
                signature: None,
            },
            ContentBlock::Reasoning {
                summary,
                content,
                encrypted_content: Some(_),
            } => ContentBlock::Reasoning {
                summary: summary.clone(),
                content: content.clone(),
                encrypted_content: None,
            },
            ContentBlock::RedactedThinking { .. } => return None,
            other => other.clone(),
        };
        render_preview_spans(&mut visible, marker);
        Some(visible)
    }

    fn responses_thinking_carrier(&self, block: &ContentBlock, marker: &HistoryMarker) -> AiItem {
        let (mut summary, mut content) = match self.visible_protected_block(block, marker) {
            Some(ContentBlock::Reasoning {
                summary, content, ..
            }) => (summary, content),
            Some(ContentBlock::Thinking { thinking, .. }) => (
                Vec::new(),
                if thinking.is_empty() {
                    Vec::new()
                } else {
                    vec![thinking]
                },
            ),
            None => (Vec::new(), Vec::new()),
            _ => unreachable!("Thinking projection uses a native reasoning carrier"),
        };
        // The marker belongs to this synthetic block, not a second independent item.
        // It follows the last content part, matching the streamed carrier.
        let rendered = render_history_marker(marker);
        if let Some(last) = content.last_mut() {
            last.push_str(&rendered);
        } else {
            content.push(rendered);
        }
        // Empty preview parts emit no live bytes. Their authoritative boundaries
        // remain in the marker payload rather than creating unary-only carriers.
        summary.retain(|part| !part.is_empty());
        content.retain(|part| !part.is_empty());
        AiItem::reasoning(summary, content, None)
    }

    pub(super) fn post_text_preview(
        &self,
        block: &ContentBlock,
        marker: &HistoryMarker,
    ) -> Option<AiItem> {
        public_thinking_text(block)
            .map(|text| AiItem::output_text(render_quoted_preview(&marker.reference, &text)))
    }
}

fn is_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::RedactedThinking { .. }
    )
}

fn is_thinking_item(item: &AiItem) -> bool {
    match &item.content {
        MessageContent::Blocks(blocks) => blocks.iter().any(is_thinking),
        MessageContent::Text(_) => false,
    }
}

fn public_thinking_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Thinking { thinking, .. } => (!thinking.is_empty()).then(|| thinking.clone()),
        ContentBlock::Reasoning {
            summary, content, ..
        } => {
            let text = summary
                .iter()
                .chain(content)
                .filter(|text| !text.is_empty())
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n\n");
            (!text.is_empty()).then_some(text)
        }
        ContentBlock::RedactedThinking { .. } => None,
        _ => None,
    }
}

fn render_preview_spans(block: &mut ContentBlock, marker: &HistoryMarker) {
    match block {
        ContentBlock::Thinking { thinking, .. } => {
            if !thinking.is_empty() {
                *thinking = render_preview_projection_span(
                    &marker.reference,
                    0,
                    &format!("\n\n{thinking}\n\n"),
                );
            }
        }
        ContentBlock::Reasoning {
            summary, content, ..
        } => {
            for (ordinal, text) in summary
                .iter_mut()
                .chain(content)
                .filter(|text| !text.is_empty())
                .enumerate()
            {
                *text = render_preview_projection_span(
                    &marker.reference,
                    ordinal,
                    &format!("\n\n{text}\n\n"),
                );
            }
        }
        _ => unreachable!("protected Thinking preview remains a reasoning block"),
    }
}

fn render_quoted_preview(reference: &str, text: &str) -> String {
    let mut encoder = QuotedThinkingPreviewEncoder::new(reference.to_owned());
    let mut rendered = encoder.push(text);
    rendered.push_str(&encoder.finish());
    rendered
}

fn private_prefix_lookbehind(text: &str) -> usize {
    [HISTORY_MARKER_PREFIX, PROJECTION_DELIMITER_PREFIX]
        .into_iter()
        .flat_map(|prefix| {
            (1..prefix.len()).filter(move |&length| text.ends_with(&prefix[..length]))
        })
        .max()
        .unwrap_or(0)
}

fn escape_private_syntax(text: &str) -> String {
    text.replace(HISTORY_MARKER_PREFIX, "&lt;!--sh:")
        .replace(PROJECTION_DELIMITER_PREFIX, "&lt;!--sp:")
}

#[cfg(test)]
async fn projection_session_fixture(
    principal_id: &str,
) -> (
    ClientProjectionSession,
    Arc<dyn HistoryMarkerStore>,
    Principal,
) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .expect("SQLite migrations");
    let store: Arc<dyn HistoryMarkerStore> =
        Arc::new(crate::history_marker::SqlHistoryMarkerStore::sqlite(pool));
    let principal = Principal::new(principal_id);
    (
        ClientProjectionSession::new(
            Arc::clone(&store),
            principal.clone(),
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        ),
        store,
        principal,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_marker::HistoryMarkerKind;
    use crate::history_marker::{ReasoningRejections, ThinkingProvenance, ThinkingSource};
    use stravia_runtime_contract::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        OPEN_RESPONSES_2026_04_24, ProtocolId,
    };

    fn replay_source(protocol: ProtocolId, model: &str) -> ThinkingSource {
        ThinkingSource {
            namespace: format!("issuer-{protocol}-{model}"),
            protocol: Some(protocol.into()),
            actual_model: model.into(),
            target_id: "actual-provider".into(),
            authority: Some(format!("authority-{protocol}-{model}")),
        }
    }

    fn protected_leg(session: &mut ClientProjectionSession, source: ThinkingSource) {
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            Some(source),
        );
    }

    #[tokio::test]
    async fn responses_plugin_bare_thinking_keeps_one_carrier_through_tool_handoff() {
        use serde_json::json;
        use stravia_protocol_codec::codec::open_responses::{
            decoder::ResponsesDecoder, stream::ResponsesStreamFormatter,
        };
        let (_, store, principal) = projection_session_fixture("plugin-bare-owner").await;
        let mut session = ClientProjectionSession::new(
            Arc::clone(&store),
            principal.clone(),
            OPEN_RESPONSES_2026_04_24,
        );
        // The plugin/default facts permit indexed and protected items; they do
        // not describe the identity of this particular bare stream carrier.
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: true,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            Some(replay_source(ANTHROPIC_MESSAGES_2023_06_01, "plugin")),
        );
        let call = stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call-read".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        let mut formatter = ResponsesStreamFormatter::new();
        let mut events = formatter.format_deltas(&[AiStreamDelta::MessageStart {
            id: "plugin".into(),
            model: "model".into(),
        }]);
        for (step, deltas) in [
            vec![AiStreamDelta::ThinkingDelta("Inspect the file.".into())],
            vec![
                AiStreamDelta::ToolCallStart {
                    index: 1,
                    id: call.id.to_string(),
                    name: call.name.clone(),
                },
                AiStreamDelta::ToolCallComplete {
                    index: 1,
                    tool_call: call,
                },
            ],
        ]
        .into_iter()
        .enumerate()
        {
            for batch in session.project_live_deltas(deltas, false).await.unwrap() {
                events.extend(formatter.format_deltas(batch.deltas()));
                session
                    .report_delivery(batch, ProjectionDelivery::Sent)
                    .await
                    .unwrap();
            }
            if step == 0 {
                assert!(
                    events.iter().any(|event| {
                        event.event.as_deref() == Some("response.reasoning_text.delta")
                            && event.data.contains("Inspect the file.")
                    }),
                    "the preview streams before the tool handoff",
                );
            }
        }
        events.extend(formatter.format_deltas(&[AiStreamDelta::Done {
            stop_reason: "tool_calls".into(),
        }]));
        let reasoning_events = |kind: &str| {
            events
                .iter()
                .filter(|event| event.event.as_deref() == Some(kind))
                .map(|event| serde_json::from_str::<serde_json::Value>(&event.data).unwrap())
                .filter(|event| event["item"]["type"] == "reasoning")
                .collect::<Vec<_>>()
        };
        let added = reasoning_events("response.output_item.added");
        let done = reasoning_events("response.output_item.done");
        assert_eq!(added.len(), 1);
        assert_eq!(done.len(), 1);
        assert_eq!(added[0]["item"]["id"], done[0]["item"]["id"]);
        let terminal = events
            .iter()
            .find(|event| event.event.as_deref() == Some("response.completed"))
            .map(|event| serde_json::from_str::<serde_json::Value>(&event.data).unwrap())
            .unwrap();
        let output = terminal["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0], done[0]["item"]);
        let preview = output[0]["content"][0]["text"].as_str().unwrap();
        assert!(preview.contains("Inspect the file."));
        assert!(preview.contains(":e-->"));
        let mut replay = ResponsesDecoder
            .decode_request(json!({"model": "model", "input": output}))
            .unwrap();
        assert_eq!(
            crate::history_marker::history_marker_references(&replay.items).len(),
            1,
        );
        crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut replay)
            .await
            .unwrap();
        assert_eq!(
            replay.items[0].thinking_ref().unwrap().0,
            "Inspect the file."
        );
        assert_eq!(replay.items[1].function_call_ref().unwrap().name, "read");
    }

    #[tokio::test]
    async fn responses_synthetic_carriers_preserve_independent_items_and_parts() {
        use serde_json::json;
        use stravia_protocol_codec::codec::open_responses::{
            decoder::ResponsesDecoder, formatter::ResponsesResponseFormatter,
            stream::ResponsesStreamFormatter,
        };
        let (_, store, principal) = projection_session_fixture("independent-responses-owner").await;
        let mut session = ClientProjectionSession::new(
            Arc::clone(&store),
            principal.clone(),
            OPEN_RESPONSES_2026_04_24,
        );
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: true,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            Some(replay_source(ANTHROPIC_MESSAGES_2023_06_01, "source-model")),
        );
        let originals = vec![
            AiItem::reasoning(
                vec!["summary A".into(), String::new(), "summary B".into()],
                vec!["content A".into(), String::new(), "content B".into()],
                Some("cipher A".into()),
            ),
            AiItem::reasoning(
                vec!["independent summary".into()],
                Vec::new(),
                Some("cipher B".into()),
            ),
        ];
        let mut formatter = ResponsesStreamFormatter::new();
        let mut events = formatter.format_deltas(&[AiStreamDelta::MessageStart {
            id: "independent".into(),
            model: "model".into(),
        }]);
        for (index, original) in originals.iter().enumerate() {
            let (summary, content, _) = original.reasoning_ref().unwrap();
            let mut deltas = vec![AiStreamDelta::ProtectedThinkingStart { index }];
            deltas.extend(summary.iter().enumerate().map(|(content_index, text)| {
                AiStreamDelta::ReasoningSummaryDelta {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(index),
                    content_index: Some(content_index),
                }
            }));
            deltas.extend(content.iter().enumerate().map(|(content_index, text)| {
                AiStreamDelta::ThinkingDeltaWithMetadata {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(index),
                    content_index: Some(content_index),
                }
            }));
            deltas.push(AiStreamDelta::ItemDone {
                index,
                item: original.clone(),
            });
            for batch in session.project_live_deltas(deltas, false).await.unwrap() {
                events.extend(formatter.format_deltas(batch.deltas()));
                session
                    .report_delivery(batch, ProjectionDelivery::Sent)
                    .await
                    .unwrap();
            }
        }
        events.extend(formatter.format_deltas(&[AiStreamDelta::Done {
            stop_reason: "stop".into(),
        }]));
        let delivered = events
            .iter()
            .find(|event| event.event.as_deref() == Some("response.completed"))
            .map(|event| {
                serde_json::from_str::<serde_json::Value>(&event.data).unwrap()["response"].clone()
            })
            .unwrap();
        let output = delivered["output"].as_array().unwrap();
        assert_eq!(output.len(), 2, "independent source blocks are not merged");
        assert_eq!(output[0]["summary"].as_array().unwrap().len(), 2);
        assert_eq!(output[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(output[1]["summary"].as_array().unwrap().len(), 1);
        let added = events
            .iter()
            .filter(|event| event.event.as_deref() == Some("response.output_item.added"))
            .map(|event| serde_json::from_str::<serde_json::Value>(&event.data).unwrap())
            .collect::<Vec<_>>();
        let done = events
            .iter()
            .filter(|event| event.event.as_deref() == Some("response.output_item.done"))
            .map(|event| serde_json::from_str::<serde_json::Value>(&event.data).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(added.len(), 2);
        assert_eq!(done.len(), 2);
        assert_ne!(added[0]["item"]["id"], added[1]["item"]["id"]);
        for (index, ((added, done), output)) in added.iter().zip(&done).zip(output).enumerate() {
            assert_eq!(added["output_index"], index);
            assert_eq!(done["output_index"], index);
            assert_eq!(added["item"]["id"], done["item"]["id"]);
            assert_eq!(&done["item"], output);
            assert!(output.get("encrypted_content").is_none());
        }
        let mut canonical = AiResponse::new("independent", "model");
        canonical.items = originals.clone();
        let batch = session.project_staged(&mut canonical, &[]).await.unwrap();
        session
            .report_delivery(batch, ProjectionDelivery::Sent)
            .await
            .unwrap();
        let unary = ResponsesResponseFormatter.format_response(&canonical);
        let mut replay = ResponsesDecoder
            .decode_request(json!({"model": "model", "input": output}))
            .unwrap();
        let unary_replay = ResponsesDecoder
            .decode_request(json!({"model": "model", "input": unary["output"]}))
            .unwrap();
        assert!(
            stravia_runtime_contract::protocol::ir::canonical::history_items_equal(
                &replay.items,
                &unary_replay.items
            )
        );
        crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut replay)
            .await
            .unwrap();
        assert_eq!(replay.items.len(), 2);
        for (restored, original) in replay.items.iter().zip(&originals) {
            assert_eq!(
                serde_json::to_value(&restored.content).unwrap(),
                serde_json::to_value(&original.content).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn cross_protocol_protected_delivery_restores_issuer_for_all_replay_targets() {
        for (issuer, ingress, model) in [
            (
                ANTHROPIC_MESSAGES_2023_06_01,
                OPEN_RESPONSES_2026_04_24,
                "claude",
            ),
            (
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                ANTHROPIC_MESSAGES_2023_06_01,
                "gemini",
            ),
        ] {
            let (_, store, principal) = projection_session_fixture("cross-protocol-owner").await;
            let source = replay_source(issuer, model);
            let mut session =
                ClientProjectionSession::new(Arc::clone(&store), principal.clone(), ingress);
            protected_leg(&mut session, source.clone());
            let original = AiItem::thinking("provider thought", Some("provider-opaque".into()));
            let mut response = AiResponse::new("response", model);
            response.items = vec![original.clone()];
            let batch = session
                .project_staged(&mut response, &[])
                .await
                .expect("project foreign Thinking");
            assert!(
                !serde_json::to_string(&response.items)
                    .unwrap()
                    .contains("provider-opaque"),
                "client delivery must not expose a foreign native signature"
            );
            assert_eq!(
                crate::history_marker::history_marker_references(&response.items).len(),
                1
            );
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("publish marker");
            let mut restored =
                stravia_runtime_contract::protocol::ir::AiRequest::new(model, response.items);
            crate::history_marker::resolve_request_markers(
                store.as_ref(),
                &principal,
                &mut restored,
            )
            .await
            .expect("client resubmits delivery unchanged");
            let restored_thinking = restored
                .items
                .iter()
                .filter(|item| is_thinking_item(item))
                .collect::<Vec<_>>();
            assert_eq!(
                restored_thinking.len(),
                1,
                "one block must restore exactly once"
            );
            assert_eq!(
                serde_json::to_value(&restored_thinking[0].content).unwrap(),
                serde_json::to_value(&original.content).unwrap()
            );
            assert_eq!(
                ThinkingSource::from_item(restored_thinking[0]),
                Some(source.clone())
            );
            for target in [
                OPEN_RESPONSES_2026_04_24,
                ANTHROPIC_MESSAGES_2023_06_01,
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ] {
                let target_source = replay_source(target, model);
                let mut replay = restored.clone();
                stravia_protocol_codec::transform::prepare_thinking_replay(&mut replay, |item| {
                    target_source.provenance(item, &ReasoningRejections::default())
                        != ThinkingProvenance::Foreign
                });
                let thoughts = replay
                    .items
                    .iter()
                    .filter_map(AiItem::thinking_ref)
                    .collect::<Vec<_>>();
                assert_eq!(
                    thoughts,
                    vec![(
                        "provider thought",
                        (target == issuer).then_some("provider-opaque")
                    )]
                );
            }
            if issuer == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
                let other_model = replay_source(issuer, "different-gemini");
                let mut replay = restored;
                stravia_protocol_codec::transform::prepare_thinking_replay(&mut replay, |item| {
                    other_model.provenance(item, &ReasoningRejections::default())
                        != ThinkingProvenance::Foreign
                });
                assert_eq!(
                    replay
                        .items
                        .iter()
                        .filter_map(AiItem::thinking_ref)
                        .collect::<Vec<_>>(),
                    vec![("provider thought", None)]
                );
            }
        }
    }

    #[tokio::test]
    async fn gemini_native_sse_late_signatures_project_and_restore_through_responses() {
        use serde_json::json;
        use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
        use stravia_protocol_codec::transform::ProtocolTransform;

        let (_, store, principal) = projection_session_fixture("native-gemini-sse-owner").await;
        let source = replay_source(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, "gemini-native-model");
        let mut session = ClientProjectionSession::new(
            Arc::clone(&store),
            principal.clone(),
            OPEN_RESPONSES_2026_04_24,
        );
        let pair = ProtocolTransform::global()
            .bind(
                OPEN_RESPONSES_2026_04_24,
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            )
            .expect("Gemini source Responses client pair");
        session.begin_model_leg(
            pair.thinking_carrier_facts(),
            Vec::new(),
            Some(source.clone()),
        );
        let (mut decoder, mut encoder) = pair.stream().expect("native stream stages").into_parts();
        let frames = [
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "thought A", "thought": true}]}}], "modelVersion": "gemini-native-model"}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": " / thought B", "thought": true}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "", "thought": true, "thoughtSignature": "native-thought-signature"}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "ordinary answer"}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {"id": "native-call", "name": "lookup", "args": {"value": 1}}, "thoughtSignature": "native-call-signature"}]}, "finishReason": "STOP"}]}),
        ];
        let mut accumulator = StreamResponseAccumulator::default();
        let mut events = Vec::new();
        for (frame_index, frame) in frames.into_iter().enumerate() {
            let bytes = format!("data: {frame}\n\n");
            let deltas = decoder
                .decode_chunk(bytes.as_bytes())
                .expect("decode real native Gemini SSE");
            accumulator.apply_all(&deltas);
            let batches = session
                .project_live_deltas(deltas, false)
                .await
                .unwrap_or_else(|error| {
                    panic!("native frame {frame_index} projection typed failure: {error:?}")
                });
            for batch in batches {
                events.extend(
                    encoder
                        .encode_deltas(batch.deltas())
                        .expect("encode projected Responses live delivery"),
                );
                session
                    .report_delivery(batch, ProjectionDelivery::Sent)
                    .await
                    .expect("publish delivered Thinking markers");
            }
        }
        let final_deltas = decoder.finish().expect("finish native Gemini parser");
        accumulator.apply_all(&final_deltas);
        for batch in session
            .project_live_deltas(final_deltas, true)
            .await
            .expect("complete native projection")
        {
            events.extend(
                encoder
                    .encode_deltas(batch.deltas())
                    .expect("encode Responses completion"),
            );
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("publish final markers");
        }
        let completed = events
            .iter()
            .filter(|event| event.event.as_deref() == Some("response.completed"))
            .map(|event| {
                serde_json::from_str::<serde_json::Value>(&event.data).unwrap()["response"].clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            completed.len(),
            1,
            "native source must produce exactly one successful client terminal"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event.as_deref(), Some("error" | "response.failed")))
        );
        let output = completed[0]["output"]
            .as_array()
            .expect("Responses terminal output");
        assert!(output.iter().any(|item| item["type"] == "function_call"
            && item["call_id"] == "native-call"
            && item["name"] == "lookup"));
        let client_pair = ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
            .expect("Responses replay pair");
        let mut replay = client_pair
            .decode_request(json!({"model": "gemini-native-model", "input": output}))
            .expect("decode genuine client terminal history");
        let live_references = crate::history_marker::history_marker_references(&replay.items);
        assert_eq!(
            live_references.len(),
            2,
            "thought and native call-signature carrier each have one marker"
        );
        crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut replay)
            .await
            .expect("restore original native Thinking from genuine client history");
        let thinking = replay
            .items
            .iter()
            .filter_map(AiItem::thinking_ref)
            .collect::<Vec<_>>();
        assert_eq!(
            thinking,
            vec![
                ("thought A / thought B", Some("native-thought-signature")),
                ("", Some("native-call-signature"))
            ]
        );
        for item in replay
            .items
            .iter()
            .filter(|item| item.thinking_ref().is_some())
        {
            assert_eq!(ThinkingSource::from_item(item), Some(source.clone()));
        }
        assert!(replay.items.iter().any(|item| {
            item.function_call_ref()
                .is_some_and(|call| call.id.as_str() == "native-call" && call.name == "lookup")
        }));
        let mut canonical = accumulator.into_ai_response();
        let staged = session
            .project_staged(&mut canonical, &[])
            .await
            .expect("settle parser canonical completion");
        assert!(
            staged.references.is_empty(),
            "staged history reuses live published markers"
        );
        assert_eq!(
            crate::history_marker::history_marker_references(&canonical.items),
            live_references
        );
        let unary =
            stravia_protocol_codec::codec::open_responses::formatter::ResponsesResponseFormatter
                .format_response(&canonical);
        let live_history = client_pair
            .decode_request(json!({
                "model": "gemini-native-model", "input": output,
            }))
            .expect("live replay");
        let unary_history = client_pair
            .decode_request(json!({
                "model": "gemini-native-model", "input": unary["output"],
            }))
            .expect("unary replay");
        assert!(
            stravia_runtime_contract::protocol::ir::canonical::history_items_equal(
                &live_history.items,
                &unary_history.items,
            ),
            "actual delivered live and unary histories preserve identical synthetic block boundaries"
        );
    }

    #[tokio::test]
    async fn indexed_thinking_closes_after_text_and_tool_without_duplicate_marker() {
        for ingress in [
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        ] {
            let (_, store, principal) =
                projection_session_fixture("interleaved-thinking-owner").await;
            let mut session =
                ClientProjectionSession::new(Arc::clone(&store), principal.clone(), ingress);
            let source = replay_source(
                stravia_runtime_contract::protocol::ids::DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
                "deepseek-model",
            );
            session.begin_model_leg(
                ThinkingCarrierFacts {
                    indexed: true,
                    may_be_protected: true,
                    stream_unprotected_summaries: false,
                },
                Vec::new(),
                Some(source.clone()),
            );
            let original =
                AiItem::reasoning(Vec::new(), vec!["Use the addition tool.".into()], None);
            let call = stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call-add".into(),
                name: "add_integers".into(),
                arguments: "{\"a\":17,\"b\":25}".into(),
            };
            let mut live = Vec::new();
            for deltas in [
                vec![
                    AiStreamDelta::MessageStart {
                        id: "response-bound".into(),
                        model: "logical-model".into(),
                    },
                    AiStreamDelta::ProtectedThinkingStart { index: 0 },
                    AiStreamDelta::ThinkingDeltaWithMetadata {
                        text: "Use the ".into(),
                        obfuscation: None,
                        output_index: Some(0),
                        content_index: Some(0),
                    },
                ],
                vec![
                    AiStreamDelta::TextDeltaWithMetadata {
                        text: " \n".into(),
                        logprobs: Vec::new(),
                        obfuscation: None,
                        output_index: Some(1),
                        content_index: Some(0),
                    },
                    AiStreamDelta::ThinkingDeltaWithMetadata {
                        text: "addition tool.".into(),
                        obfuscation: None,
                        output_index: Some(0),
                        content_index: Some(0),
                    },
                ],
                vec![
                    AiStreamDelta::ToolCallStart {
                        index: 2,
                        id: call.id.to_string(),
                        name: call.name.clone(),
                    },
                    AiStreamDelta::ToolCallDelta {
                        index: 2,
                        arguments: call.arguments.clone(),
                    },
                ],
                vec![
                    AiStreamDelta::ItemDone {
                        index: 1,
                        item: AiItem::output_text(" \n"),
                    },
                    AiStreamDelta::ItemDone {
                        index: 0,
                        item: original.clone(),
                    },
                    AiStreamDelta::ToolCallComplete {
                        index: 2,
                        tool_call: call.clone(),
                    },
                    AiStreamDelta::ItemDone {
                        index: 2,
                        item: AiItem::function_call(call.clone()),
                    },
                ],
            ] {
                for batch in session.project_live_deltas(deltas, false).await.unwrap() {
                    live.extend_from_slice(batch.deltas());
                    session
                        .report_delivery(batch, ProjectionDelivery::Sent)
                        .await
                        .unwrap();
                }
            }
            let rendered = text_of(
                &live
                    .into_iter()
                    .filter(|delta| {
                        matches!(
                            delta,
                            AiStreamDelta::ThinkingDelta(_)
                                | AiStreamDelta::ThinkingDeltaWithMetadata { .. }
                                | AiStreamDelta::ReasoningSummaryDelta { .. }
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            assert_eq!(rendered.matches(HISTORY_MARKER_PREFIX).count(), 1);
            let mut response = AiResponse::new("response-bound", "logical-model");
            response.items = vec![
                original.clone(),
                AiItem::output_text(" \n"),
                AiItem::function_call(call),
            ];
            let staged = session
                .project_staged(&mut response, &[])
                .await
                .expect("settle delayed thinking");
            session
                .report_delivery(staged, ProjectionDelivery::Sent)
                .await
                .expect("deliver settled thinking");
            let mut replay = stravia_runtime_contract::protocol::ir::AiRequest::new(
                "deepseek-model",
                vec![AiItem::thinking(rendered, None)],
            );
            crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut replay)
                .await
                .unwrap();
            let thoughts = replay
                .items
                .iter()
                .filter(|item| is_thinking_item(item))
                .collect::<Vec<_>>();
            assert_eq!(thoughts.len(), 1);
            assert_eq!(
                serde_json::to_value(&thoughts[0].content).unwrap(),
                serde_json::to_value(&original.content).unwrap()
            );
            assert_eq!(ThinkingSource::from_item(thoughts[0]), Some(source));
        }
    }

    #[tokio::test]
    async fn cross_protocol_indexed_summary_restores_only_original_reasoning() {
        for ingress in [
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        ] {
            let (_, store, principal) =
                projection_session_fixture("indexed-late-cipher-owner").await;
            let source = replay_source(OPEN_RESPONSES_2026_04_24, "responses-model");
            let mut session =
                ClientProjectionSession::new(Arc::clone(&store), principal.clone(), ingress);
            session.begin_model_leg(
                ThinkingCarrierFacts {
                    indexed: true,
                    may_be_protected: true,
                    stream_unprotected_summaries: true,
                },
                Vec::new(),
                Some(source.clone()),
            );
            let original = AiItem::reasoning(
                vec!["public summary".into()],
                Vec::new(),
                Some("late-cipher".into()),
            );
            let mut live = Vec::new();
            for batch in session
                .project_live_deltas(
                    vec![AiStreamDelta::ReasoningSummaryDelta {
                        text: "public summary".into(),
                        obfuscation: None,
                        output_index: Some(0),
                        content_index: Some(0),
                    }],
                    false,
                )
                .await
                .expect("stream summary before cipher")
            {
                live.extend_from_slice(batch.deltas());
                session
                    .report_delivery(batch, ProjectionDelivery::Sent)
                    .await
                    .expect("deliver preview");
            }
            assert!(
                text_of(&live).contains("public summary"),
                "public summary must stream immediately"
            );
            for batch in session
                .project_live_deltas(
                    vec![AiStreamDelta::ItemDone {
                        index: 0,
                        item: original.clone(),
                    }],
                    true,
                )
                .await
                .expect("close with late cipher")
            {
                live.extend_from_slice(batch.deltas());
                session
                    .report_delivery(batch, ProjectionDelivery::Sent)
                    .await
                    .expect("publish marker");
            }
            let rendered = text_of(&live);
            assert!(!rendered.contains("late-cipher"));
            assert_eq!(rendered.matches(HISTORY_MARKER_PREFIX).count(), 1);
            let mut replay = stravia_runtime_contract::protocol::ir::AiRequest::new(
                "responses-model",
                vec![AiItem::thinking(rendered, None)],
            );
            crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut replay)
                .await
                .expect("restore live delivery");
            let thoughts = replay
                .items
                .iter()
                .filter(|item| is_thinking_item(item))
                .collect::<Vec<_>>();
            assert_eq!(
                thoughts.len(),
                1,
                "preview must not duplicate restored reasoning"
            );
            assert_eq!(
                serde_json::to_value(&thoughts[0].content).unwrap(),
                serde_json::to_value(&original.content).unwrap()
            );
            assert_eq!(ThinkingSource::from_item(thoughts[0]), Some(source));
            let mut response = AiResponse::new("response", "responses-model");
            response.items = vec![original];
            let batch = session
                .project_staged(&mut response, &[])
                .await
                .expect("settle indexed reasoning");
            assert!(batch.references.is_empty());
            assert_eq!(
                crate::history_marker::history_marker_references(&response.items),
                crate::history_marker::history_marker_references(&[AiItem::thinking(
                    text_of(&live),
                    None
                )])
            );
        }
    }

    #[tokio::test]
    async fn same_protocol_protected_delivery_keeps_native_carrier_without_marker() {
        for protocol in [
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        ] {
            let (_, store, principal) = projection_session_fixture("native-protected-owner").await;
            let mut session = ClientProjectionSession::new(store, principal, protocol);
            protected_leg(&mut session, replay_source(protocol, "native-model"));
            let mut response = AiResponse::new("response", "native-model");
            response.items = vec![
                AiItem::output_text("answer"),
                AiItem::thinking("native thought", Some("native-signature".into())),
            ];
            let original = response.items.clone();
            let batch = session
                .project_staged(&mut response, &[])
                .await
                .expect("native projection");
            assert_eq!(
                serde_json::to_value(&response.items).unwrap(),
                serde_json::to_value(&original).unwrap()
            );
            assert!(crate::history_marker::history_marker_references(&response.items).is_empty());
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("native delivery");
        }
    }

    #[tokio::test]
    async fn responses_cross_protocol_live_and_staged_share_one_marker_after_text() {
        let (_, store, principal) = projection_session_fixture("responses-live-owner").await;
        let source = replay_source(ANTHROPIC_MESSAGES_2023_06_01, "claude");
        let mut session = ClientProjectionSession::new(
            Arc::clone(&store),
            principal.clone(),
            OPEN_RESPONSES_2026_04_24,
        );
        protected_leg(&mut session, source.clone());
        let original = AiItem::thinking("later thought", Some("claude-signature".into()));
        let mut live = Vec::new();
        for batch in session
            .project_live_deltas(
                vec![
                    AiStreamDelta::TextDelta("answer".into()),
                    AiStreamDelta::ThinkingDelta("later thought".into()),
                    AiStreamDelta::ThinkingSignature("claude-signature".into()),
                    AiStreamDelta::ItemDone {
                        index: 1,
                        item: original.clone(),
                    },
                ],
                true,
            )
            .await
            .expect("live foreign protected Thinking")
        {
            live.extend_from_slice(batch.deltas());
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("publish live marker");
        }
        assert!(
            !live
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::ThinkingSignature(_))),
            "Responses client must not receive a Claude signature as native opaque data"
        );
        let rendered = text_of(&live);
        assert!(
            rendered.starts_with("answer"),
            "ordinary Text must retain first position"
        );
        assert!(rendered.contains("later thought"));
        assert_eq!(rendered.matches(HISTORY_MARKER_PREFIX).count(), 1);
        let mut response = AiResponse::new("response", "claude");
        response.items = vec![AiItem::output_text("answer"), original.clone()];
        let batch = session
            .project_staged(&mut response, &[])
            .await
            .expect("settle same leg");
        let live_references =
            crate::history_marker::history_marker_references(&[AiItem::output_text(rendered)]);
        assert_eq!(
            crate::history_marker::history_marker_references(&response.items),
            live_references
        );
        assert!(
            batch.references.is_empty(),
            "settlement must reuse the published live marker"
        );
        let mut request =
            stravia_runtime_contract::protocol::ir::AiRequest::new("claude", response.items);
        crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut request)
            .await
            .expect("restore settled client history");
        let thought = request
            .items
            .iter()
            .filter(|item| is_thinking_item(item))
            .collect::<Vec<_>>();
        assert_eq!(thought.len(), 1);
        assert_eq!(
            serde_json::to_value(&thought[0].content).unwrap(),
            serde_json::to_value(&original.content).unwrap()
        );
        assert_eq!(ThinkingSource::from_item(thought[0]), Some(source));
    }

    #[tokio::test]

    async fn same_batch_text_and_thinking_close_in_wire_order_and_restore_original() {
        let (mut session, store, principal) = projection_session_fixture("same-batch-owner").await;
        begin_openai_leg(&mut session);
        let original = "**first**\nsecond";
        let batches = session
            .project_live_deltas(
                vec![
                    AiStreamDelta::TextDelta("answer".into()),
                    AiStreamDelta::ThinkingDelta(original.into()),
                    AiStreamDelta::ItemDone {
                        index: 1,
                        item: AiItem::thinking(original, None),
                    },
                ],
                true,
            )
            .await
            .expect("project same-batch completion");
        let rendered = batches
            .iter()
            .map(|batch| text_of(batch.deltas()))
            .collect::<String>();
        assert!(rendered.starts_with("answer"), "{rendered}");
        assert!(rendered.contains("> **first**\n> second"), "{rendered}");
        assert_eq!(
            rendered.matches(HISTORY_MARKER_PREFIX).count(),
            1,
            "{rendered}"
        );
        for batch in batches {
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("publish delivery");
        }
        let mut request = stravia_runtime_contract::protocol::ir::AiRequest::new(
            "model",
            vec![AiItem::output_text(rendered)],
        );
        crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut request)
            .await
            .expect("restore authoritative Thinking");
        let reasoning = request
            .items
            .iter()
            .filter_map(|item| match &item.content {
                MessageContent::Blocks(blocks) => Some(blocks),
                _ => None,
            })
            .flatten()
            .filter(|block| is_thinking(block))
            .collect::<Vec<_>>();
        assert!(matches!(
            reasoning.as_slice(),
            [ContentBlock::Thinking { thinking, signature: None }] if thinking == original
        ));
    }

    #[tokio::test]
    async fn empty_deltas_and_late_signature_restore_buffered_thinking_without_layout() {
        let (mut session, store, principal) =
            projection_session_fixture("signed-batch-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            None,
        );
        let batches = session
            .project_live_deltas(
                vec![
                    AiStreamDelta::ThinkingDelta(String::new()),
                    AiStreamDelta::ThinkingDelta("**first**".into()),
                    AiStreamDelta::ThinkingDelta(String::new()),
                    AiStreamDelta::ThinkingDelta("second".into()),
                    AiStreamDelta::ThinkingSignature("opaque-signature".into()),
                    AiStreamDelta::TextDelta("answer".into()),
                ],
                true,
            )
            .await
            .expect("close signed buffered Thinking");
        let rendered = batches
            .iter()
            .map(|batch| text_of(batch.deltas()))
            .collect::<String>();
        assert!(rendered.contains("**first**second"), "{rendered}");
        assert!(rendered.ends_with("answer"), "{rendered}");
        assert!(!rendered.contains("opaque-signature"), "{rendered}");
        assert_eq!(rendered.matches(":s-->").count(), 1, "{rendered}");
        for batch in batches {
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("publish delivery");
        }
        let mut request = stravia_runtime_contract::protocol::ir::AiRequest::new(
            "model",
            vec![AiItem::thinking(rendered, None)],
        );
        crate::history_marker::resolve_request_markers(store.as_ref(), &principal, &mut request)
            .await
            .expect("restore signed original");
        let reasoning = request
            .items
            .iter()
            .filter_map(|item| match &item.content {
                MessageContent::Blocks(blocks) => Some(blocks),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                ContentBlock::Thinking {
                    thinking,
                    signature: Some(signature),
                } => Some((thinking.as_str(), signature.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning, vec![("**first**second", "opaque-signature")]);
    }

    #[tokio::test]
    async fn upload_delivery_renews_expired_grants_without_extending_old_authorization() {
        use crate::agent::upload_grant::{UPLOAD_PLACEHOLDER, UploadGrantIssuer};
        use std::sync::atomic::{AtomicI64, Ordering};

        async fn deliver(session: &mut ClientProjectionSession) -> String {
            let batches = session
                .project_live_deltas(
                    vec![AiStreamDelta::TextDelta(UPLOAD_PLACEHOLDER.into())],
                    false,
                )
                .await
                .unwrap();
            let mut text = String::new();
            for batch in batches {
                text.push_str(&text_of(batch.deltas()));
                session
                    .report_delivery(batch, ProjectionDelivery::Sent)
                    .await
                    .unwrap();
            }
            text
        }

        let directory = tempfile::tempdir().unwrap();
        let mut gateway = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
        let now = Arc::new(AtomicI64::new(1_800_000_000_000));
        let clock = Arc::clone(&now);
        gateway.upload_grants = Arc::new(UploadGrantIssuer::with_clock(
            &[17; 32],
            Arc::new(move || clock.load(Ordering::SeqCst)),
        ));
        let mut settings = stravia_runtime_contract::artifact::ArtifactSettings {
            client_base_url: "https://client.example/prefix".into(),
            upload_prompt_injection: true,
            ..Default::default()
        };
        gateway
            .admin()
            .set_setting(
                "artifact_settings",
                &serde_json::to_string(&settings).unwrap(),
            )
            .await
            .unwrap();
        let (session, _, principal) = projection_session_fixture("long-upload-owner").await;
        let mut session = session.with_upload_gateway(gateway.clone());
        begin_openai_leg(&mut session);
        let first = deliver(&mut session).await;
        assert_eq!(
            gateway.upload_grants.authenticate(&first).unwrap(),
            principal
        );
        now.fetch_add(14 * 60 * 1000, Ordering::SeqCst);
        assert_eq!(deliver(&mut session).await, first);
        now.fetch_add(60 * 1000, Ordering::SeqCst);
        let renewed = deliver(&mut session).await;
        assert_ne!(renewed, first);
        assert!(gateway.upload_grants.authenticate(&first).is_err());
        assert_eq!(
            gateway.upload_grants.authenticate(&renewed).unwrap(),
            principal
        );

        let (next, _, _) = projection_session_fixture("long-upload-owner").await;
        let mut next = next.with_upload_gateway(gateway.clone());
        begin_openai_leg(&mut next);
        assert_ne!(deliver(&mut next).await, renewed);
        settings.upload_prompt_injection = false;
        gateway
            .admin()
            .set_setting(
                "artifact_settings",
                &serde_json::to_string(&settings).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deliver(&mut session).await, UPLOAD_PLACEHOLDER);
        assert!(gateway.upload_grants.authenticate(&renewed).is_ok());
        gateway.shutdown().await;
    }

    fn marker(reference: &str, kind: HistoryMarkerKind) -> HistoryMarker {
        HistoryMarker {
            reference: reference.into(),
            kind,
            activity: "Preserving protected reasoning".into(),
        }
    }

    fn begin_openai_leg(session: &mut ClientProjectionSession) {
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: false,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            None,
        );
    }

    fn text_of(deltas: &[AiStreamDelta]) -> String {
        deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::TextDelta(text)
                | AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::TextDeltaWithMetadata { text, .. }
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
                | AiStreamDelta::ReasoningSummaryDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn marker_reference(batch: &ProjectedDeltaBatch) -> String {
        let text = text_of(batch.deltas());
        crate::history_marker::history_marker_references(&[AiItem::output_text(text)])
            .into_iter()
            .next()
            .expect("projected History Marker reference")
    }

    fn preview_reference(deltas: &[AiStreamDelta]) -> String {
        text_of(deltas)
            .split_once(PROJECTION_DELIMITER_PREFIX)
            .expect("projected Thinking Preview delimiter")
            .1
            .split(':')
            .next()
            .expect("projected Thinking Preview reference")
            .to_owned()
    }

    #[tokio::test]
    async fn sent_publishes_persisted_thinking_marker_through_the_session() {
        let (mut session, store, principal) = projection_session_fixture("sent-owner").await;
        begin_openai_leg(&mut session);

        let answer = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("answer".into())], false)
            .await
            .expect("project answer");
        for batch in answer {
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("deliver answer");
        }
        let preview = session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("reason".into())], false)
            .await
            .expect("project Preview");
        for batch in preview {
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("deliver Preview");
        }
        let mut closed = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("later".into())], true)
            .await
            .expect("close Thinking");
        assert_eq!(closed.len(), 2, "Marker barrier precedes later Text");
        let marker = closed.remove(0);
        let reference = marker_reference(&marker);
        assert!(
            store
                .resolve(&principal, &reference)
                .await
                .expect("resolve persisted Marker")
                .is_some_and(|resolved| !resolved.published)
        );

        session
            .report_delivery(marker, ProjectionDelivery::Sent)
            .await
            .expect("publish delivered Marker");
        assert!(
            store
                .resolve(&principal, &reference)
                .await
                .expect("resolve published Marker")
                .is_some_and(|resolved| resolved.published)
        );
        session
            .report_delivery(closed.remove(0), ProjectionDelivery::Sent)
            .await
            .expect("deliver later Text");
    }

    #[tokio::test]
    async fn cancelled_delivery_abandons_the_session_reservation_without_publish() {
        let (mut session, store, principal) = projection_session_fixture("cancelled-owner").await;
        begin_openai_leg(&mut session);
        session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("answer".into())], false)
            .await
            .expect("project answer");
        session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("reason".into())], false)
            .await
            .expect("project Preview");
        let mut closed = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("later".into())], true)
            .await
            .expect("close Thinking");
        let marker = closed.remove(0);
        let reference = marker_reference(&marker);

        session
            .report_delivery(marker, ProjectionDelivery::Cancelled)
            .await
            .expect("cancel projected Marker");
        assert!(
            store
                .resolve(&principal, &reference)
                .await
                .expect("resolve cancelled Marker")
                .is_some_and(|resolved| !resolved.published)
        );
    }

    #[tokio::test]
    async fn injected_carrier_facts_control_summary_streaming_without_protocol_ids() {
        let (mut session, _, _) = projection_session_fixture("carrier-facts-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: true,
                may_be_protected: true,
                stream_unprotected_summaries: true,
            },
            Vec::new(),
            None,
        );
        let streamed = session
            .project_live_deltas(
                vec![AiStreamDelta::ReasoningSummaryDelta {
                    text: "public summary".into(),
                    obfuscation: None,
                    output_index: Some(0),
                    content_index: Some(0),
                }],
                false,
            )
            .await
            .expect("project public summary");
        assert_eq!(streamed.len(), 1);
        assert!(text_of(streamed[0].deltas()).contains("public summary"));

        let (mut session, _, _) = projection_session_fixture("carrier-facts-indexed-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: true,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            None,
        );
        let indexed_buffered = session
            .project_live_deltas(
                vec![AiStreamDelta::ReasoningSummaryDelta {
                    text: "possibly protected summary".into(),
                    obfuscation: None,
                    output_index: Some(0),
                    content_index: Some(0),
                }],
                false,
            )
            .await
            .expect("buffer indexed protected candidate");
        assert!(indexed_buffered.is_empty());

        let (mut session, _, _) = projection_session_fixture("carrier-facts-unindexed-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            None,
        );
        let unindexed_buffered = session
            .project_live_deltas(
                vec![AiStreamDelta::ThinkingDelta(
                    "possibly protected unindexed Thinking".into(),
                )],
                false,
            )
            .await
            .expect("buffer unindexed protected candidate");
        assert!(unindexed_buffered.is_empty());
    }

    #[tokio::test]
    async fn split_platform_tool_name_is_classified_and_hidden_inside_the_session() {
        let (mut session, _, _) = projection_session_fixture("tool-name-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: false,
                stream_unprotected_summaries: false,
            },
            vec!["stravia__ordered_tool".to_owned()],
            None,
        );
        for delta in [
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call-split".into(),
                name: "stravia__ord".into(),
            },
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: String::new(),
                name: "ered_tool".into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{}".into(),
            },
            AiStreamDelta::ToolCallComplete {
                index: 0,
                tool_call: stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "call-split".into(),
                    name: "stravia__ordered_tool".into(),
                    arguments: "{}".into(),
                },
            },
        ] {
            assert!(
                session
                    .project_live_deltas(vec![delta], true)
                    .await
                    .expect("classify Platform Tool delta")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn unambiguous_client_tool_arguments_stream_without_waiting_for_complete() {
        let (mut session, _, _) = projection_session_fixture("client-tool-stream-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: false,
                stream_unprotected_summaries: false,
            },
            vec!["StraviaRead".to_owned()],
            None,
        );

        let started = session
            .project_live_deltas(
                vec![AiStreamDelta::ToolCallStart {
                    index: 0,
                    id: "call-eval".into(),
                    name: "eval".into(),
                }],
                true,
            )
            .await
            .expect("forward client tool start");
        assert!(started.iter().any(|batch| {
            batch.deltas().iter().any(|delta| {
                matches!(
                    delta,
                    AiStreamDelta::ToolCallStart { name, .. } if name == "eval"
                )
            })
        }));

        let streamed = session
            .project_live_deltas(
                vec![AiStreamDelta::ToolCallDelta {
                    index: 0,
                    arguments: r#"{"language":"py"}"#.into(),
                }],
                true,
            )
            .await
            .expect("forward client tool arguments");
        assert!(streamed.iter().any(|batch| {
            batch.deltas().iter().any(|delta| {
                matches!(
                    delta,
                    AiStreamDelta::ToolCallDelta { arguments, .. }
                        if arguments == r#"{"language":"py"}"#
                )
            })
        }));
    }

    #[tokio::test]
    async fn parallel_client_tools_stream_arguments_independently() {
        let (mut session, _, _) = projection_session_fixture("parallel-client-tools-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: false,
                stream_unprotected_summaries: false,
            },
            vec!["StraviaRead".to_owned()],
            None,
        );

        let first = session
            .project_live_deltas(
                vec![
                    AiStreamDelta::ToolCallStart {
                        index: 0,
                        id: "call-read".into(),
                        name: "read".into(),
                    },
                    AiStreamDelta::ToolCallDelta {
                        index: 0,
                        arguments: r#"{"path":"a"}"#.into(),
                    },
                ],
                true,
            )
            .await
            .expect("forward first client tool");
        assert!(first.iter().any(|batch| {
            batch
                .deltas()
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::ToolCallDelta { index: 0, .. }))
        }));

        let second = session
            .project_live_deltas(
                vec![
                    AiStreamDelta::ToolCallStart {
                        index: 1,
                        id: "call-grep".into(),
                        name: "grep".into(),
                    },
                    AiStreamDelta::ToolCallDelta {
                        index: 1,
                        arguments: r#"{"pattern":"x"}"#.into(),
                    },
                ],
                true,
            )
            .await
            .expect("forward second client tool");
        assert!(second.iter().any(|batch| {
            batch
                .deltas()
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::ToolCallDelta { index: 1, .. }))
        }));
        assert!(
            session
                .project_live_deltas(
                    vec![AiStreamDelta::ToolCallStart {
                        index: 2,
                        id: "call-platform".into(),
                        name: "StraviaRead".into(),
                    }],
                    true,
                )
                .await
                .expect("hide platform tool")
                .iter()
                .all(|batch| batch.is_empty())
        );
    }

    #[tokio::test]
    async fn protected_thinking_without_public_bytes_projects_only_its_marker() {
        let (mut session, _, _) = projection_session_fixture("marker-only-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: true,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            None,
        );
        assert!(
            session
                .project_live_deltas(
                    vec![AiStreamDelta::ProtectedThinkingStart { index: 0 }],
                    false,
                )
                .await
                .expect("reserve protected Marker")
                .is_empty()
        );
        let projected = session
            .project_live_deltas(
                vec![AiStreamDelta::ItemDone {
                    index: 0,
                    item: AiItem {
                        role: Role::Assistant,
                        content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
                            data: "opaque".into(),
                        }]),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: None,
                    },
                }],
                true,
            )
            .await
            .expect("project protected Marker");

        assert_eq!(projected.len(), 1);
        let visible = text_of(projected[0].deltas());
        assert!(visible.contains(HISTORY_MARKER_PREFIX), "{visible}");
        assert!(!visible.contains("opaque"), "{visible}");
    }

    #[tokio::test]
    async fn platform_marker_publishes_only_after_session_reports_sent() {
        let (mut session, store, principal) =
            projection_session_fixture("platform-sent-owner").await;
        begin_openai_leg(&mut session);
        let marker = store
            .create_platform(
                &principal,
                crate::history_marker::PlatformMarkerInput {
                    tool_id: "web_search".into(),
                    call: stravia_runtime_contract::protocol::ir::ToolCall {
                        id: "call-platform".into(),
                        name: "web_search".into(),
                        arguments: "{}".into(),
                    },
                    activity: "Searching".into(),
                    execution_limit: Duration::from_secs(30),
                    pending_retention: Duration::from_secs(60),
                },
            )
            .await
            .expect("persist Platform Marker");
        let batch = session.project_platform_marker(&marker);
        assert!(
            store
                .resolve(&principal, &marker.reference)
                .await
                .expect("resolve pending Platform Marker")
                .is_some_and(|resolved| !resolved.published)
        );

        assert_eq!(
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("publish Platform Marker"),
            vec![marker.reference.clone()]
        );
        assert!(
            store
                .resolve(&principal, &marker.reference)
                .await
                .expect("resolve published Platform Marker")
                .is_some_and(|resolved| resolved.published)
        );
    }

    #[tokio::test]
    async fn one_session_projects_live_and_staged_with_the_same_marker() {
        let (mut session, store, principal) = projection_session_fixture("projection-owner").await;
        begin_openai_leg(&mut session);
        let platform = store
            .create_platform(
                &principal,
                crate::history_marker::PlatformMarkerInput {
                    tool_id: "web_search".into(),
                    call: stravia_runtime_contract::protocol::ir::ToolCall {
                        id: "call-platform".into(),
                        name: "web_search".into(),
                        arguments: "{}".into(),
                    },
                    activity: "Searching".into(),
                    execution_limit: Duration::from_secs(30),
                    pending_retention: Duration::from_secs(60),
                },
            )
            .await
            .expect("persist Platform Marker");
        let mut live = Vec::new();
        for batch in session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("answer".into())], false)
            .await
            .expect("project Text")
        {
            live.extend_from_slice(batch.deltas());
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("deliver Text");
        }
        for batch in session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("reason".into())], false)
            .await
            .expect("project Thinking Preview")
        {
            live.extend_from_slice(batch.deltas());
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("deliver Thinking Preview");
        }
        let reserved = preview_reference(&live);
        assert!(
            store
                .resolve(&principal, &reserved)
                .await
                .expect("resolve reserved marker")
                .is_none()
        );

        let closed = session
            .project_live_deltas(
                vec![AiStreamDelta::ItemDone {
                    index: 1,
                    item: AiItem::thinking("reason", None),
                }],
                false,
            )
            .await
            .expect("close Thinking");
        assert_eq!(closed.len(), 1);
        for batch in closed {
            live.extend_from_slice(batch.deltas());
            session
                .report_delivery(batch, ProjectionDelivery::Sent)
                .await
                .expect("deliver Thinking Marker");
        }
        let platform_batch = session.project_platform_marker(&platform);
        live.extend_from_slice(platform_batch.deltas());
        session
            .report_delivery(platform_batch, ProjectionDelivery::Sent)
            .await
            .expect("deliver Platform Marker");
        let persisted = store
            .resolve(&principal, &reserved)
            .await
            .expect("resolve persisted marker")
            .expect("persisted marker");
        assert!(persisted.published);

        let mut staged = AiResponse::new("response", "model");
        staged.items = vec![
            AiItem::output_text("answer"),
            AiItem::thinking("reason", None),
            AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call-platform".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
        ];
        let staged_batch = session
            .project_staged(&mut staged, &[("call-platform", &platform)])
            .await
            .expect("project staged response");
        assert!(
            staged_batch.references.is_empty(),
            "staged projection must consume the live Markers"
        );
        let staged_text = staged
            .items
            .iter()
            .filter_map(AiItem::output_text_ref)
            .collect::<String>();

        assert_eq!(text_of(&live), staged_text);
        assert_eq!(
            crate::history_marker::history_marker_references(&staged.items),
            vec![reserved.clone(), platform.reference.clone()]
        );
        assert!(
            store
                .resolve(&principal, &reserved)
                .await
                .expect("resolve published marker")
                .is_some_and(|marker| marker.published)
        );
    }

    #[tokio::test]
    async fn persist_failure_abandons_preview_without_retyping_it_as_canonical_text() {
        let (mut session, _, _) = projection_session_fixture("persist-failure-owner").await;
        begin_openai_leg(&mut session);
        session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("answer".into())], false)
            .await
            .expect("project Text");
        let preview = session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("reason".into())], false)
            .await
            .expect("project Thinking Preview");

        assert!(matches!(
            session
                .project_live_deltas(
                    vec![AiStreamDelta::ItemDone {
                        index: 1,
                        item: AiItem::thinking(String::new(), None),
                    }],
                    false,
                )
                .await,
            Err(HistoryMarkerError::InvalidPayload)
        ));
        let preview = preview
            .iter()
            .map(|batch| text_of(batch.deltas()))
            .collect::<String>();
        assert!(preview.contains(PROJECTION_DELIMITER_PREFIX), "{preview}");
        assert_ne!(preview, "reason");
    }

    #[tokio::test]
    async fn publish_failure_leaves_the_persisted_marker_unpublished() {
        let (mut session, store, principal) =
            projection_session_fixture("publish-failure-owner").await;
        begin_openai_leg(&mut session);
        session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("answer".into())], false)
            .await
            .expect("project Text");
        session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("reason".into())], false)
            .await
            .expect("project Preview");
        let mut closed = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("later".into())], true)
            .await
            .expect("persist Thinking Marker");
        let mut marker_delivery = closed.remove(0);
        let reference = marker_delivery.references[0].reference.clone();
        marker_delivery.references.push(ProjectedMarkerReference {
            reference: "abcdefghijklmnopqrstuvwxyzaz".to_owned(),
            platform: false,
        });

        let error = session
            .report_delivery(marker_delivery, ProjectionDelivery::Sent)
            .await
            .expect_err("missing sibling reference must roll publication back");
        assert!(matches!(error, HistoryMarkerError::Storage(_)));
        assert!(!text_of(closed[0].deltas()).contains("reason"));
        assert!(
            store
                .resolve(&principal, &reference)
                .await
                .expect("resolve unpublished marker")
                .is_some_and(|marker| !marker.published)
        );
    }

    #[tokio::test]
    async fn post_text_survives_a_hidden_model_leg_boundary() {
        let (mut session, _, _) = projection_session_fixture("hidden-leg-owner").await;
        begin_openai_leg(&mut session);
        let mut first_leg = AiResponse::new("first", "model");
        first_leg.items = vec![AiItem::output_text("visible answer")];
        let _ = session
            .project_staged(&mut first_leg, &[])
            .await
            .expect("project first leg");

        begin_openai_leg(&mut session);
        let mut hidden_leg = AiResponse::new("hidden", "model");
        hidden_leg.items = vec![AiItem::thinking("later reasoning", None)];
        let delivery = session
            .project_staged(&mut hidden_leg, &[])
            .await
            .expect("project hidden leg");

        assert_eq!(delivery.references.len(), 1);
        assert!(
            hidden_leg
                .items
                .iter()
                .all(|item| item.thinking_ref().is_none())
        );
        let visible = hidden_leg
            .items
            .iter()
            .filter_map(AiItem::output_text_ref)
            .collect::<String>();
        assert!(visible.contains("> later reasoning"), "{visible}");
        assert!(visible.contains(HISTORY_MARKER_PREFIX), "{visible}");
    }

    #[tokio::test]
    async fn explicit_reasoning_metadata_survives_committed_model_leg_boundaries() {
        use stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;

        let (mut session, _, _) = projection_session_fixture("reasoning-field-owner").await;
        begin_openai_leg(&mut session);
        let mut initial = AiResponse::new("first", "model");
        initial.push_output_text("answer");
        session
            .project_live_deltas(
                crate::model_turn::support::ai_response_to_deltas(&initial),
                true,
            )
            .await
            .expect("commit first response");
        assert!(session.response_started);

        for value in [serde_json::json!(""), serde_json::Value::Null] {
            begin_openai_leg(&mut session);
            let mut response = AiResponse::new("next", "model");
            response
                .vendor
                .ingress
                .insert(CHAT_REASONING_FIELD_META.into(), value.clone());
            response.push_output_text("next answer");
            let mut deltas = crate::model_turn::support::ai_response_to_deltas(&response);
            let AiStreamDelta::ResponseMetadata { metadata } = &mut deltas[1] else {
                panic!("explicit field must follow MessageStart");
            };
            metadata["model"] = serde_json::json!("other-model");
            let batches = session.project_live_deltas(deltas, true).await.unwrap();
            let visible = batches.iter().flat_map(|batch| batch.deltas());
            let mut fields = 0;
            for delta in visible {
                match delta {
                    AiStreamDelta::ResponseMetadata { metadata } => {
                        assert_eq!(metadata.get(CHAT_REASONING_FIELD_META), Some(&value));
                        assert!(metadata.get("model").is_none());
                        fields += 1;
                    }
                    AiStreamDelta::MessageStart { .. } => panic!("duplicate MessageStart"),
                    AiStreamDelta::ThinkingDelta(_) => panic!("empty field became Thinking"),
                    _ => {}
                }
            }
            assert_eq!(fields, 1);
            assert!(batches.iter().all(|batch| batch.references.is_empty()));
        }
        let batches = session
            .project_live_deltas(
                vec![AiStreamDelta::ResponseMetadata {
                    metadata: serde_json::json!({"model": "ignored"}),
                }],
                true,
            )
            .await
            .unwrap();
        assert!(batches.is_empty());
    }

    #[tokio::test]
    async fn post_text_thinking_streams_as_quoted_content_with_stable_marker() {
        let (mut session, _, _) = projection_session_fixture("stable-preview-owner").await;
        begin_openai_leg(&mut session);
        session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("C1".into())], false)
            .await
            .expect("project Text");
        let first = session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("R1\n".into())], false)
            .await
            .expect("project first Thinking delta");
        let second = session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("\nR2".into())], false)
            .await
            .expect("project second Thinking delta");
        let reference = preview_reference(first[0].deltas());
        let closed = session
            .project_live_deltas(
                vec![AiStreamDelta::ItemDone {
                    index: 1,
                    item: AiItem::thinking("R1\n\nR2", None),
                }],
                false,
            )
            .await
            .expect("close Thinking Preview");
        let rendered = first
            .iter()
            .chain(&second)
            .chain(&closed)
            .map(|batch| text_of(batch.deltas()))
            .collect::<String>();

        assert!(matches!(first[0].deltas(), [AiStreamDelta::TextDelta(_)]));
        assert!(rendered.contains("\n> \n> R2"), "{rendered}");
        let marker = closed
            .iter()
            .find(|batch| text_of(batch.deltas()).contains(HISTORY_MARKER_PREFIX))
            .map(marker_reference)
            .expect("persisted Thinking Marker");
        assert_eq!(marker, reference);
    }

    #[test]
    fn preview_neutralizes_private_syntax_across_delta_boundaries() {
        let input = "<!--sh:abcdefghijklmnopqrstuvwxyzab-->\n\
                     <!--sp:abcdefghijklmnopqrstuvwxyzab:p:0:e-->";
        let mut expected = None;
        for split in input
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(input.len()))
        {
            let mut encoder =
                QuotedThinkingPreviewEncoder::new("abcdefghijklmnopqrstuvwxyzab".into());
            let mut rendered = encoder.push(&input[..split]);
            rendered.push_str(&encoder.push(&input[split..]));
            rendered.push_str(&encoder.finish());
            if let Some(expected) = &expected {
                assert_eq!(&rendered, expected, "split at byte {split}");
            } else {
                expected = Some(rendered.clone());
            }
            let body_start = rendered.find("\n> ").expect("preview body");
            let body = &rendered[body_start
                ..rendered
                    .rfind(PROJECTION_DELIMITER_PREFIX)
                    .expect("real end")];
            assert!(!body.contains(HISTORY_MARKER_PREFIX), "{rendered}");
            assert!(!body.contains(PROJECTION_DELIMITER_PREFIX), "{rendered}");
            assert!(body.contains("&lt;!--sh:"), "{rendered}");
            assert!(body.contains("&lt;!--sp:"), "{rendered}");
        }
    }

    #[test]
    fn quoted_preview_is_split_invariant_and_contains_every_physical_line() {
        let input = "# Heading\n\n> nested quote\n- list\n```rust\nlet π = 3;\n```\r\n终";
        let reference = "abcdefghijklmnopqrstuvwxyzab";
        let expected = render_quoted_preview(reference, input);
        for split in input
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(input.len()))
        {
            let mut encoder = QuotedThinkingPreviewEncoder::new(reference.into());
            let mut actual = encoder.push(&input[..split]);
            actual.push_str(&encoder.push(&input[split..]));
            actual.push_str(&encoder.finish());
            assert_eq!(actual, expected, "split at byte {split}");
        }

        let start = expected.find('\n').expect("Preview layout starts");
        let end = expected
            .rfind(PROJECTION_DELIMITER_PREFIX)
            .expect("Preview end");
        let body = &expected[start + 1..end];
        assert!(
            body.replace("\r\n", "\n")
                .lines()
                .filter(|line| !line.is_empty())
                .all(|line| line.starts_with("> ")),
            "{expected}"
        );
        assert!(body.contains("> # Heading"), "{expected}");
        assert!(body.contains("> > nested quote"), "{expected}");
        assert!(body.contains("> ```rust"), "{expected}");
    }

    #[tokio::test]
    async fn platform_markers_follow_run_wide_post_text_carrier() {
        let (mut session, _, _) = projection_session_fixture("platform-owner").await;
        begin_openai_leg(&mut session);
        let platform = marker("abcdefghijklmnopqrstuvwxyzab", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&platform).deltas(),
            [AiStreamDelta::ThinkingDelta(_)]
        ));
        session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("C1".into())], false)
            .await
            .expect("project Text");
        let second = marker("abcdefghijklmnopqrstuvwxyzac", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&second).deltas(),
            [AiStreamDelta::TextDelta(_)]
        ));
        let mut staged = AiResponse::new("response", "model");
        staged.items = vec![
            AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call-before".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
            AiItem::output_text("C1"),
            AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call-after".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
        ];
        let _ = session
            .project_staged(
                &mut staged,
                &[("call-before", &platform), ("call-after", &second)],
            )
            .await
            .expect("consume live Platform Marker carriers");
        begin_openai_leg(&mut session);
        let third = marker("abcdefghijklmnopqrstuvwxyzad", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&third).deltas(),
            [AiStreamDelta::TextDelta(_)]
        ));
    }

    #[tokio::test]
    async fn only_non_empty_text_starts_post_text_state() {
        let (mut session, _, _) = projection_session_fixture("text-state-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: false,
                stream_unprotected_summaries: false,
            },
            vec!["stravia__ordered_tool".to_owned()],
            None,
        );
        let empty = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta(String::new())], false)
            .await
            .expect("project empty Text");
        let before_text = marker("abcdefghijklmnopqrstuvwxyzab", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&before_text).deltas(),
            [AiStreamDelta::ThinkingDelta(_)]
        ));

        let whitespace = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta(" ".into())], false)
            .await
            .expect("project whitespace Text");
        let after_text = marker("abcdefghijklmnopqrstuvwxyzac", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&after_text).deltas(),
            [AiStreamDelta::TextDelta(_)]
        ));
        assert_eq!(empty.len(), 1);
        assert_eq!(whitespace.len(), 1);
        assert_eq!(text_of(whitespace[0].deltas()), " ");
    }

    #[tokio::test]
    async fn protected_post_text_blocks_expose_only_public_preview_bytes() {
        let (mut session, _, _) = projection_session_fixture("protected-owner").await;
        begin_openai_leg(&mut session);
        let mut response = AiResponse::new("response", "model");
        response.items = vec![
            AiItem::output_text("answer"),
            AiItem::reasoning(
                vec!["public summary".into()],
                Vec::new(),
                Some("opaque-encrypted-payload".into()),
            ),
            AiItem::thinking(String::new(), Some("opaque-signature".into())),
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
                    data: "opaque-redacted-payload".into(),
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ];

        let delivery = session
            .project_staged(&mut response, &[])
            .await
            .expect("project protected Thinking");
        let visible = response
            .items
            .iter()
            .filter_map(AiItem::output_text_ref)
            .collect::<String>();

        assert!(visible.contains("> public summary"), "{visible}");
        assert!(!visible.contains("opaque-encrypted-payload"), "{visible}");
        assert!(!visible.contains("opaque-signature"), "{visible}");
        assert!(!visible.contains("opaque-redacted-payload"), "{visible}");
        assert_eq!(delivery.references.len(), 3);
        assert_eq!(
            crate::history_marker::history_marker_references(&response.items).len(),
            3
        );
    }

    #[tokio::test]
    async fn each_post_text_thinking_block_reserves_a_distinct_marker() {
        let (mut session, _, _) = projection_session_fixture("block-owner").await;
        begin_openai_leg(&mut session);
        session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("C1".into())], false)
            .await
            .expect("project Text");
        session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("R1".into())], false)
            .await
            .expect("project first Thinking Preview");
        let first_batches = session
            .project_live_deltas(
                vec![AiStreamDelta::ItemDone {
                    index: 1,
                    item: AiItem::thinking("R1", None),
                }],
                false,
            )
            .await
            .expect("close first Thinking block");
        let first = first_batches
            .iter()
            .find(|batch| text_of(batch.deltas()).contains(HISTORY_MARKER_PREFIX))
            .map(marker_reference)
            .expect("first marker");
        let mut first_response = AiResponse::new("first", "model");
        first_response.items = vec![AiItem::output_text("C1"), AiItem::thinking("R1", None)];
        let _ = session
            .project_staged(&mut first_response, &[])
            .await
            .expect("consume first live projection");

        session
            .project_live_deltas(vec![AiStreamDelta::ThinkingDelta("R2".into())], false)
            .await
            .expect("project second Thinking Preview");
        let second_batches = session
            .project_live_deltas(
                vec![AiStreamDelta::ItemDone {
                    index: 2,
                    item: AiItem::thinking("R2", None),
                }],
                false,
            )
            .await
            .expect("close second Thinking block");
        let second = second_batches
            .iter()
            .find(|batch| text_of(batch.deltas()).contains(HISTORY_MARKER_PREFIX))
            .map(marker_reference)
            .expect("second marker");

        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn client_output_committed_flips_only_on_sent_delivery() {
        let (mut session, _, _) = projection_session_fixture("commit-owner").await;
        begin_openai_leg(&mut session);
        assert!(!session.client_output_committed());

        let mut cancelled = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("lost".into())], false)
            .await
            .expect("project cancelled batch");
        session
            .report_delivery(cancelled.remove(0), ProjectionDelivery::Cancelled)
            .await
            .expect("report cancelled delivery");
        assert!(!session.client_output_committed());

        let mut sent = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("answer".into())], false)
            .await
            .expect("project sent batch");
        session
            .report_delivery(sent.remove(0), ProjectionDelivery::Sent)
            .await
            .expect("report sent delivery");
        assert!(session.client_output_committed());
    }

    #[tokio::test]
    async fn platform_projection_never_retypes_text() {
        let platform = marker("abcdefghijklmnopqrstuvwxyzab", HistoryMarkerKind::Platform);
        let (mut session, _, _) = projection_session_fixture("platform-staged-owner").await;
        begin_openai_leg(&mut session);
        let mut response = AiResponse::new("response", "model");
        response.items = vec![
            AiItem::output_text("C1"),
            AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call-1".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
        ];
        let _ = session
            .project_staged(&mut response, &[("call-1", &platform)])
            .await
            .expect("project Platform Marker");

        assert_eq!(response.items[0].output_text_ref(), Some("C1"));
        assert!(response.items[0].thinking_ref().is_none());
        assert!(
            response.items[1]
                .output_text_ref()
                .is_some_and(|text| text.contains(HISTORY_MARKER_PREFIX))
        );
    }
}

//! Client Projection for one Inference Run.
//!
//! Canonical Text is delivered unchanged. OpenAI-compatible clients keep
//! Thinking on the reasoning carrier until the first non-empty Text, then use
//! quoted `content` previews bound to authoritative Thinking History Markers.
//! Other protocols retain their native carriers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use crate::history_marker::{
    HISTORY_MARKER_PREFIX, HistoryMarker, HistoryMarkerError, HistoryMarkerStore,
    PROJECTION_DELIMITER_PREFIX, ThinkingMarkerInput, render_history_marker,
    render_preview_projection_end, render_preview_projection_span, render_preview_projection_start,
};
use crate::hook::Principal;
use crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use crate::protocol::ir::{AiItem, AiResponse, AiStreamDelta, ContentBlock, MessageContent, Role};
use crate::protocol::transform::ThinkingCarrierFacts;

const THINKING_MARKER_PENDING_RETENTION: Duration = Duration::from_secs(60 * 60);
const PUBLISHED_MARKER_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Copy)]
enum PreviewCarrier {
    Unindexed,
    Indexed {
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
}

impl PreviewCarrier {
    fn text_delta(self, text: String) -> AiStreamDelta {
        match self {
            Self::Unindexed => AiStreamDelta::TextDelta(text),
            Self::Indexed {
                output_index,
                content_index,
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
                "{}\n> {quoted}",
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
                "{}\n> {quoted}",
                render_preview_projection_start(&self.reference, 0)
            );
        }
        quoted.push('\n');
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
    fn ordinal(self) -> usize {
        match self {
            Self::Unindexed => 0,
            Self::Thinking { content_index, .. } | Self::Summary { content_index, .. } => {
                content_index.unwrap_or(0)
            }
        }
    }

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
    carrier: Option<ProtectedPreviewCarrier>,
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
    early_thinking: BTreeMap<usize, VecDeque<ProjectedThinkingMarker>>,
    live_platform_carriers: HashMap<String, bool>,
    staged_delivery: Option<ProjectedDeltaBatch>,
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
    current_unindexed_item_kind: Option<UnindexedItemKind>,
    client_output_started: bool,
    response_started: bool,
}

impl ClientProjectionSession {
    pub(super) fn new(
        marker_store: Arc<dyn HistoryMarkerStore>,
        principal: Principal,
        ingress: crate::protocol::ids::ProtocolId,
    ) -> Self {
        Self {
            state: ProjectionState::for_ingress(ingress),
            marker_store,
            principal,
            leg_started_post_text: false,
            early_thinking: BTreeMap::new(),
            live_platform_carriers: HashMap::new(),
            staged_delivery: None,
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
            current_unindexed_item_kind: None,
            client_output_started: false,
            response_started: false,
        }
    }

    pub(super) fn begin_model_leg(
        &mut self,
        carrier_facts: ThinkingCarrierFacts,
        exposed_tool_names: impl IntoIterator<Item = String>,
    ) {
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
        let post_text = self.state.post_text_started();
        let reserved = self.state.reserved_thinking_marker(output_index).cloned();
        let had_live_projection = reserved.is_some();
        let preview_started = self.state.thinking_preview_started(output_index);
        let markers = self
            .persist_thinking_blocks(item, reserved.as_ref(), post_text)
            .await?;
        let finish_deltas = self.state.close_thinking_preview(output_index);
        let mut preview_deltas = Vec::new();
        let mut marker_deltas = Vec::with_capacity(markers.len());
        if self.state.openai_compatible
            && let MessageContent::Blocks(blocks) = &item.content
        {
            let mut markers_for_blocks = markers.iter();
            for block in blocks {
                if !is_thinking(block) || (!post_text && !is_protected_thinking(block)) {
                    continue;
                }
                let marker = markers_for_blocks
                    .next()
                    .ok_or(HistoryMarkerError::InvalidPayload)?;
                if is_protected_thinking(block) {
                    preview_deltas.extend(self.state.preview_deltas(output_index, block, marker));
                }
            }
            if markers_for_blocks.next().is_some() {
                return Err(HistoryMarkerError::InvalidPayload);
            }
        }
        for marker in &markers {
            marker_deltas.push(self.state.marker_delta(render_history_marker(marker)));
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
            had_live_projection: had_live_projection || preview_started,
        })
    }

    async fn persist_thinking_blocks(
        &self,
        item: &AiItem,
        reserved: Option<&HistoryMarker>,
        post_text: bool,
    ) -> Result<Vec<HistoryMarker>, HistoryMarkerError> {
        if !self.state.openai_compatible {
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
        for block in blocks
            .iter()
            .filter(|block| is_thinking(block) && (post_text || is_protected_thinking(block)))
        {
            let marker = self
                .persist_thinking_block(block.clone(), reserved.take())
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

    pub(super) async fn report_delivery(
        &mut self,
        batch: ProjectedDeltaBatch,
        outcome: ProjectionDelivery,
    ) -> Result<Vec<String>, HistoryMarkerError> {
        if outcome == ProjectionDelivery::Cancelled {
            self.abandon_live_projection();
            return Ok(Vec::new());
        }
        if batch.references.is_empty() {
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

    pub(super) fn take_staged_delivery(&mut self) -> ProjectedDeltaBatch {
        self.staged_delivery
            .take()
            .unwrap_or_else(|| ProjectedDeltaBatch::visible(Vec::new()))
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
        self.capture_protected_candidates(&deltas);
        self.capture_unindexed_signatures(&mut deltas);
        let has_completed_thinking = deltas.iter().any(|delta| {
            matches!(
                delta,
                AiStreamDelta::ItemDone { item, .. } if is_thinking_item(item)
            )
        });
        let thinking_completed = !has_completed_thinking
            && (model_leg_completed
                || deltas
                    .iter()
                    .any(ClientProjectionSession::ends_unindexed_thinking));
        if thinking_completed
            && let Some((index, item)) = self
                .synthetic_signed_thinking_item()
                .or_else(|| self.synthetic_post_text_thinking_item())
        {
            deltas.insert(0, AiStreamDelta::ItemDone { index, item });
        }

        let mut completed_indices = HashSet::new();
        let completed = deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::ItemDone { index, item }
                    if completed_indices.insert(*index) && is_thinking_item(item) =>
                {
                    Some((*index, item.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut batches = Vec::new();
        for (output_index, item) in completed {
            let (marker_deltas, markers) = self.close_live_thinking(output_index, &item).await?;
            let marker_deltas = self.route_visible_deltas(marker_deltas);
            if !marker_deltas.is_empty() {
                batches.push(ProjectedDeltaBatch {
                    deltas: marker_deltas,
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
        let visible = self.filter_live_deltas(deltas);
        if !visible.is_empty() {
            batches.push(ProjectedDeltaBatch::visible(visible));
        }
        Ok(batches)
    }

    pub(super) fn complete_live_model_leg(&mut self) -> ProjectedDeltaBatch {
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

    fn capture_unindexed_signatures(&mut self, deltas: &mut Vec<AiStreamDelta>) {
        if self.pending_unindexed_thinking.is_none() {
            return;
        }
        let mut remaining = Vec::with_capacity(deltas.len());
        for delta in std::mem::take(deltas) {
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
                }
                other => remaining.push(other),
            }
        }
        *deltas = remaining;
    }

    fn synthetic_signed_thinking_item(&self) -> Option<(usize, AiItem)> {
        let signature = self.pending_unindexed_signature.as_deref()?;
        if signature.is_empty() {
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
        Some((
            *index,
            AiItem::thinking(thinking, Some(signature.to_owned())),
        ))
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
            } if self.carrier_facts.indexed && self.carrier_facts.may_be_protected => Some(*index),
            AiStreamDelta::ThinkingDelta(_)
            | AiStreamDelta::ThinkingDeltaWithMetadata {
                output_index: None, ..
            }
            | AiStreamDelta::ReasoningSummaryDelta {
                output_index: None, ..
            } if !self.carrier_facts.indexed && self.carrier_facts.may_be_protected => {
                Some(self.observe_unindexed_item(UnindexedItemKind::Thinking))
            }
            _ => None,
        }
    }

    fn streams_unprotected_reasoning_summary(&self, index: usize, delta: &AiStreamDelta) -> bool {
        self.carrier_facts.stream_unprotected_summaries
            && !self.known_protected_thinking_indices.contains(&index)
            && matches!(delta, AiStreamDelta::ReasoningSummaryDelta { .. })
    }

    fn capture_protected_candidates(&mut self, deltas: &[AiStreamDelta]) {
        for delta in deltas {
            if let AiStreamDelta::ProtectedThinkingStart { index } = delta {
                self.known_protected_thinking_indices.insert(*index);
                self.begin_protected_thinking(*index);
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
        !matches!(
            delta,
            AiStreamDelta::ThinkingDelta(_)
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

    fn filter_live_deltas(&mut self, deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for delta in deltas {
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
                } else if self.known_protected_thinking_indices.contains(&index) {
                    self.streamed_protected_thinking_indices.insert(index);
                    let projected = self.project_protected_delta(index, delta);
                    visible.extend(self.commit_visible(projected));
                }
                continue;
            }
            let kind = Self::unindexed_item_kind(&delta);
            if kind != Some(UnindexedItemKind::Thinking)
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
                continue;
            }
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
                                visible.extend(self.route_visible_deltas(pending));
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
                        visible.extend(self.route_visible_deltas(pending));
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
                        visible.extend(self.route_visible_deltas(pending));
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
            if matches!(&delta, AiStreamDelta::ItemDone { index, .. } if self.projected_thinking_items.remove(index))
            {
                continue;
            }
            visible.extend(self.route_visible_deltas(vec![delta]));
        }
        visible
    }

    pub(super) async fn project_staged(
        &mut self,
        response: &mut AiResponse,
        platform: &[(&str, &HistoryMarker)],
    ) -> Result<(), HistoryMarkerError> {
        let by_call_id = platform
            .iter()
            .copied()
            .collect::<HashMap<&str, &HistoryMarker>>();
        let mut post_text = self.leg_started_post_text;
        let mut projected = Vec::with_capacity(response.items.len() + platform.len());
        let mut staged_deltas = Vec::new();
        let mut staged_references = Vec::new();

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
            let mut meta = item.meta.take();
            match std::mem::replace(&mut item.content, MessageContent::Text(String::new())) {
                MessageContent::Text(text) => {
                    if !text.is_empty() {
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
                            }
                            push_projection_block(&mut projected, block, &mut meta);
                            continue;
                        }

                        let recorded = prepared.front().cloned();
                        let block_post_text =
                            recorded.as_ref().map_or(post_text, |entry| entry.post_text);
                        let needs_marker = self.state.openai_compatible
                            && (block_post_text || is_protected_thinking(&block));
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
                            let marker = self.persist_thinking_block(block.clone(), None).await?;
                            (marker, true)
                        };
                        if block_post_text {
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
                        let mut marker_item =
                            marker_item_for(self.state.openai_compatible, block_post_text, &marker);
                        marker_item.meta = meta.take();
                        projected.push(marker_item);
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
        response.items = projected;
        self.staged_delivery = Some(ProjectedDeltaBatch {
            deltas: staged_deltas,
            references: staged_references,
        });
        Ok(())
    }

    async fn persist_thinking_block(
        &self,
        block: ContentBlock,
        reserved: Option<&HistoryMarker>,
    ) -> Result<HistoryMarker, HistoryMarkerError> {
        let input = ThinkingMarkerInput {
            block,
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
    meta: &mut Option<serde_json::Value>,
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
    openai_compatible: bool,
    post_text_started: bool,
    live_previews: HashMap<usize, LiveThinkingPreview>,
    pre_text_protected_previews: HashMap<usize, LiveProtectedPreview>,
}

impl Default for ProjectionState {
    fn default() -> Self {
        Self {
            openai_compatible: true,
            post_text_started: false,
            live_previews: HashMap::new(),
            pre_text_protected_previews: HashMap::new(),
        }
    }
}

impl ProjectionState {
    fn for_ingress(ingress: crate::protocol::ids::ProtocolId) -> Self {
        Self {
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
            AiStreamDelta::ThinkingDelta(text)
                if self.openai_compatible && self.post_text_started =>
            {
                self.project_thinking_delta(output_index, PreviewCarrier::Unindexed, text)
            }
            AiStreamDelta::ThinkingDeltaWithMetadata {
                text,
                output_index: delta_output_index,
                content_index,
                ..
            }
            | AiStreamDelta::ReasoningSummaryDelta {
                text,
                output_index: delta_output_index,
                content_index,
                ..
            } if self.openai_compatible && self.post_text_started => self.project_thinking_delta(
                output_index,
                PreviewCarrier::Indexed {
                    output_index: delta_output_index,
                    content_index,
                },
                text,
            ),
            other => vec![other],
        }
    }

    pub(super) fn begin_protected_thinking(&mut self, output_index: usize) {
        if self.openai_compatible && !self.post_text_started {
            self.pre_text_protected_previews
                .entry(output_index)
                .or_insert_with(|| LiveProtectedPreview {
                    marker: crate::history_marker::reserve_thinking_marker(),
                    carrier: None,
                });
        }
    }

    pub(super) fn project_protected_delta(
        &mut self,
        output_index: usize,
        delta: AiStreamDelta,
    ) -> Vec<AiStreamDelta> {
        if !self.openai_compatible {
            return vec![delta];
        }
        if self.post_text_started {
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
        let mut projected = Vec::with_capacity(2);
        if let Some(previous) = preview.carrier
            && previous != carrier
        {
            projected.push(previous.delta(
                render_preview_projection_end(&preview.marker.reference, previous.ordinal()),
                None,
            ));
        }
        let text = if preview.carrier == Some(carrier) {
            text
        } else {
            format!(
                "{}{text}",
                render_preview_projection_start(&preview.marker.reference, carrier.ordinal())
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
        let preview = self.live_previews.entry(output_index).or_insert_with(|| {
            let marker = crate::history_marker::reserve_thinking_marker();
            LiveThinkingPreview {
                encoder: QuotedThinkingPreviewEncoder::new(marker.reference.clone()),
                marker,
                carrier,
                canonical_text: String::new(),
            }
        });
        preview.carrier = carrier;
        preview.canonical_text.push_str(&text);
        let projected = preview.encoder.push(&text);
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
                        render_preview_projection_end(&preview.marker.reference, carrier.ordinal()),
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

/// Protected reasoning the client must not receive in its authoritative form.
fn is_protected_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking {
            signature: Some(_),
            ..
        } | ContentBlock::Reasoning {
            encrypted_content: Some(_),
            ..
        } | ContentBlock::RedactedThinking { .. }
    )
}

fn public_thinking_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Thinking { thinking, .. } => (!thinking.is_empty()).then(|| thinking.clone()),
        ContentBlock::Reasoning {
            summary, content, ..
        } => {
            let text = summary.iter().chain(content).cloned().collect::<String>();
            (!text.is_empty()).then_some(text)
        }
        ContentBlock::RedactedThinking { .. } => None,
        _ => None,
    }
}

fn render_preview_spans(block: &mut ContentBlock, marker: &HistoryMarker) {
    match block {
        ContentBlock::Thinking { thinking, .. } => {
            *thinking = render_preview_projection_span(&marker.reference, 0, thinking);
        }
        ContentBlock::Reasoning {
            summary, content, ..
        } => {
            for (ordinal, text) in summary.iter_mut().chain(content).enumerate() {
                *text = render_preview_projection_span(&marker.reference, ordinal, text);
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
    text.replace(HISTORY_MARKER_PREFIX, "&lt;!-- stravia-history-marker:")
        .replace(PROJECTION_DELIMITER_PREFIX, "&lt;!-- stravia-projection:")
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
    crate::migrations::migrate_sqlite(&pool)
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
        );
    }

    fn text_of(deltas: &[AiStreamDelta]) -> String {
        deltas
            .iter()
            .map(|delta| match delta {
                AiStreamDelta::TextDelta(text)
                | AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::TextDeltaWithMetadata { text, .. }
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
                | AiStreamDelta::ReasoningSummaryDelta { text, .. } => text.as_str(),
                other => panic!("unexpected projected delta: {other:?}"),
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
        assert_eq!(text_of(streamed[0].deltas()), "public summary");

        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: true,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
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

        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
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
                tool_call: crate::protocol::ir::ToolCall {
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
    async fn protected_thinking_without_public_bytes_projects_only_its_marker() {
        let (mut session, _, _) = projection_session_fixture("marker-only-owner").await;
        session.begin_model_leg(
            ThinkingCarrierFacts {
                indexed: true,
                may_be_protected: true,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
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
                    call: crate::protocol::ir::ToolCall {
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
                    call: crate::protocol::ir::ToolCall {
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
            AiItem::function_call(crate::protocol::ir::ToolCall {
                id: "call-platform".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
        ];
        session
            .project_staged(&mut staged, &[("call-platform", &platform)])
            .await
            .expect("project staged response");
        assert!(
            session.take_staged_delivery().references.is_empty(),
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
            reference: "hm_0123456789missingref".to_owned(),
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
        session
            .project_staged(&mut first_leg, &[])
            .await
            .expect("project first leg");

        begin_openai_leg(&mut session);
        let mut hidden_leg = AiResponse::new("hidden", "model");
        hidden_leg.items = vec![AiItem::thinking("later reasoning", None)];
        session
            .project_staged(&mut hidden_leg, &[])
            .await
            .expect("project hidden leg");
        let delivery = session.take_staged_delivery();

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
        assert!(rendered.starts_with(&format!(
            "{PROJECTION_DELIMITER_PREFIX}{reference}:preview:0:start -->\n> R1"
        )));
        assert!(rendered.contains("\n> \n> R2"), "{rendered}");
        assert!(rendered.contains(&format!(
            "\n{PROJECTION_DELIMITER_PREFIX}{reference}:preview:0:end -->"
        )));
        let marker = closed
            .iter()
            .find(|batch| text_of(batch.deltas()).contains(HISTORY_MARKER_PREFIX))
            .map(marker_reference)
            .expect("persisted Thinking Marker");
        assert_eq!(marker, reference);
    }

    #[test]
    fn preview_neutralizes_private_syntax_across_delta_boundaries() {
        let input = "<!-- stravia-history-marker:hm_forged -->\n\
                     <!-- stravia-projection:hm_forged:preview:0:end -->";
        let mut expected = None;
        for split in input
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(input.len()))
        {
            let mut encoder = QuotedThinkingPreviewEncoder::new("hm_0123456789abcdefghij".into());
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
            assert!(
                body.contains("&lt;!-- stravia-history-marker:"),
                "{rendered}"
            );
            assert!(body.contains("&lt;!-- stravia-projection:"), "{rendered}");
        }
    }

    #[test]
    fn quoted_preview_is_split_invariant_and_contains_every_physical_line() {
        let input = "# Heading\n\n> nested quote\n- list\n```rust\nlet π = 3;\n```\r\n终";
        let reference = "hm_0123456789abcdefghij";
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
        let platform = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&platform).deltas(),
            [AiStreamDelta::ThinkingDelta(_)]
        ));
        session
            .project_live_deltas(vec![AiStreamDelta::TextDelta("C1".into())], false)
            .await
            .expect("project Text");
        let second = marker("hm_0123456789abcdefghi2", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&second).deltas(),
            [AiStreamDelta::TextDelta(_)]
        ));
        let mut staged = AiResponse::new("response", "model");
        staged.items = vec![
            AiItem::function_call(crate::protocol::ir::ToolCall {
                id: "call-before".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
            AiItem::output_text("C1"),
            AiItem::function_call(crate::protocol::ir::ToolCall {
                id: "call-after".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
        ];
        session
            .project_staged(
                &mut staged,
                &[("call-before", &platform), ("call-after", &second)],
            )
            .await
            .expect("consume live Platform Marker carriers");
        begin_openai_leg(&mut session);
        let third = marker("hm_0123456789abcdefghi3", HistoryMarkerKind::Platform);
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
        );
        let empty = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta(String::new())], false)
            .await
            .expect("project empty Text");
        let before_text = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker(&before_text).deltas(),
            [AiStreamDelta::ThinkingDelta(_)]
        ));

        let whitespace = session
            .project_live_deltas(vec![AiStreamDelta::TextDelta(" ".into())], false)
            .await
            .expect("project whitespace Text");
        let after_text = marker("hm_0123456789abcdefghi2", HistoryMarkerKind::Platform);
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

        session
            .project_staged(&mut response, &[])
            .await
            .expect("project protected Thinking");
        let delivery = session.take_staged_delivery();
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
        session
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
    async fn platform_projection_never_retypes_text() {
        let platform = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Platform);
        let (mut session, _, _) = projection_session_fixture("platform-staged-owner").await;
        begin_openai_leg(&mut session);
        let mut response = AiResponse::new("response", "model");
        response.items = vec![
            AiItem::output_text("C1"),
            AiItem::function_call(crate::protocol::ir::ToolCall {
                id: "call-1".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
            }),
        ];
        session
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

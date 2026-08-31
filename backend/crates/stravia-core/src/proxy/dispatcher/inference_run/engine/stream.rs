//! Streaming response handlers. Every path decodes provider events to canonical
//! deltas, applies HookRuntime stream transformations, and encodes the resulting
//! semantic stream for the ingress protocol.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderMap;
use futures::StreamExt;

use crate::agent::{CanonicalEvent, ModelTurn, ModelTurnExecutor};
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, AiStreamDelta, ContentBlock, MessageContent,
};
use crate::proxy::context::RequestContext;

use super::delivery::LiveStreamRequest;
use super::{
    ClientOutputCommit, CompletionContext, CompletionFailure, CompletionInput, CompletionOutcome,
    DeliveryAdapter, DeliveryProgress, EarlyPlatformExecution, EarlyThinkingMarkers,
    FollowupModelTurn, LogBuilder, PhaseTracker, PublishedPlatformExecutions, RequestExtras,
    RoundOutcome, StreamResponseAccumulator, acquire_followup_model_turn, ai_response_to_deltas,
    buffered_response, complete_canonical_response, error_response, hook_failure_response,
    live_response, prepare_platform_markers, prepare_thinking_markers, publish_markers,
    render_completion_failure,
};

pub(super) struct HookLegGuard<'a> {
    run: &'a mut crate::hook::InferenceRun,
    closed: bool,
}

impl<'a> HookLegGuard<'a> {
    pub(super) fn new(run: &'a mut crate::hook::InferenceRun) -> Self {
        Self { run, closed: false }
    }

    pub(super) fn run_mut(&mut self) -> &mut crate::hook::InferenceRun {
        self.run
    }

    pub(super) async fn close(&mut self) -> Result<Vec<AiStreamDelta>, crate::hook::HookError> {
        if self.closed {
            return Ok(Vec::new());
        }
        let result = self.run.flush_stream();
        self.closed = true;
        result
    }
}

impl Drop for HookLegGuard<'_> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.run.flush_stream();
            self.closed = true;
        }
    }
}

pub(super) struct ModelTurnStreamInput {
    pub(super) turn: ModelTurn,
    pub(super) executor: Arc<dyn ModelTurnExecutor>,
    pub(super) gateway: crate::Gateway,
    pub(super) headers: HeaderMap,
    pub(super) ingress: crate::protocol::ids::ProtocolId,
    pub(super) request_context: RequestContext,
    pub(super) request: AiRequest,
    pub(super) generation: super::GenerationChainRun,
    pub(super) inference_run: crate::hook::InferenceRun,
    pub(super) phase: PhaseTracker,
    pub(super) start: Instant,
    pub(super) turn_started: Instant,
    pub(super) request_extras: RequestExtras,
    pub(super) log: LogBuilder,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnindexedItemKind {
    Text,
    Thinking,
    Tool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectionSpanCarrier {
    Unindexed,
    Indexed {
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
}

impl ProjectionSpanCarrier {
    fn delta(self, text: String, obfuscation: Option<String>) -> AiStreamDelta {
        match self {
            Self::Unindexed => AiStreamDelta::ThinkingDelta(text),
            Self::Indexed {
                output_index,
                content_index,
            } => AiStreamDelta::ThinkingDeltaWithMetadata {
                text,
                obfuscation,
                output_index,
                content_index,
            },
        }
    }
}

#[derive(Default)]
struct LiveDeltaGate {
    pending_prefix: Vec<AiStreamDelta>,
    pending_suffix: Vec<AiStreamDelta>,
    pending_tool_deltas: HashMap<usize, Vec<AiStreamDelta>>,
    pending_tool_names: HashMap<usize, String>,
    platform_tool_indices: HashSet<usize>,
    pending_protected_deltas: HashMap<usize, Vec<AiStreamDelta>>,
    prebuffered_protected_counts: HashMap<usize, usize>,
    pending_unindexed_thinking: Option<(usize, Vec<AiStreamDelta>)>,
    pending_unindexed_signature: Option<String>,
    buffer_indexed_protected: bool,
    buffer_unindexed_protected: bool,
    next_unindexed_output_index: usize,
    current_unindexed_item_kind: Option<UnindexedItemKind>,
    ambiguous_suffix: bool,
    contains_platform: bool,
    projection_reference: Option<String>,
    projection_span_ordinal: usize,
    projection_span_carrier: Option<ProjectionSpanCarrier>,
    projected_text_output_indices: HashSet<usize>,
    client_output_started: bool,
    response_started: bool,
}

impl LiveDeltaGate {
    fn begin_model_leg(&mut self, egress: crate::protocol::ids::Protocol) {
        debug_assert!(
            self.pending_suffix.is_empty(),
            "a completed Model Leg must resolve its ambiguous suffix"
        );
        debug_assert!(
            self.projection_span_carrier.is_none(),
            "a completed Model Leg must close its projected Text span"
        );
        self.pending_tool_deltas.clear();
        self.pending_tool_names.clear();
        self.platform_tool_indices.clear();
        self.pending_protected_deltas.clear();
        self.prebuffered_protected_counts.clear();
        self.pending_unindexed_thinking = None;
        self.pending_unindexed_signature = None;
        self.buffer_indexed_protected =
            matches!(egress, crate::protocol::ids::Protocol::OpenResponses);
        self.buffer_unindexed_protected = matches!(
            egress,
            crate::protocol::ids::Protocol::AnthropicMessages
                | crate::protocol::ids::Protocol::GoogleGemini
        );
        self.next_unindexed_output_index = 0;
        self.current_unindexed_item_kind = None;
        self.ambiguous_suffix = false;
        self.contains_platform = false;
        self.projection_reference = None;
        self.projection_span_ordinal = 0;
        self.projection_span_carrier = None;
        self.projected_text_output_indices.clear();
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

    fn note_unindexed_signature(&mut self, signature: &str) {
        if self.pending_unindexed_thinking.is_some() && !signature.is_empty() {
            self.pending_unindexed_signature
                .get_or_insert_with(String::new)
                .push_str(signature);
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
                    self.note_unindexed_signature(&signature);
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
        let mut thinking = String::new();
        for delta in deltas {
            match delta {
                AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. } => {
                    thinking.push_str(text);
                }
                _ => {}
            }
        }
        Some((
            *index,
            AiItem::thinking(thinking, Some(signature.to_owned())),
        ))
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
            } if self.buffer_indexed_protected => Some(*index),
            AiStreamDelta::ThinkingDelta(_)
            | AiStreamDelta::ThinkingDeltaWithMetadata {
                output_index: None, ..
            }
            | AiStreamDelta::ReasoningSummaryDelta {
                output_index: None, ..
            } if self.buffer_unindexed_protected => {
                Some(self.observe_unindexed_item(UnindexedItemKind::Thinking))
            }
            _ => None,
        }
    }

    fn capture_protected_candidates(&mut self, deltas: &[AiStreamDelta]) {
        for delta in deltas {
            let Some(index) = self.protected_candidate_index(delta) else {
                continue;
            };
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
                    _ => {
                        self.pending_unindexed_thinking = Some((index, vec![delta.clone()]));
                    }
                }
            }
            *self.prebuffered_protected_counts.entry(index).or_default() += 1;
        }
    }

    fn resolve_completed_item(
        &mut self,
        index: usize,
        item: &AiItem,
        markers: &[crate::history_marker::HistoryMarker],
    ) -> Vec<AiStreamDelta> {
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
        let Some(pending) = pending else {
            return Vec::new();
        };
        if markers.is_empty() {
            return pending;
        }
        let mut preview = Vec::new();
        let mut marker_index = 0;
        if let MessageContent::Blocks(blocks) = &item.content {
            for block in blocks {
                let is_protected = matches!(
                    block,
                    ContentBlock::Thinking {
                        signature: Some(_),
                        ..
                    } | ContentBlock::Reasoning {
                        encrypted_content: Some(_),
                        ..
                    } | ContentBlock::RedactedThinking { .. }
                );
                if !is_protected {
                    continue;
                }
                let marker = markers
                    .get(marker_index)
                    .expect("protected Thinking block has a History Marker");
                marker_index += 1;
                match block {
                    ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                        preview.push(AiStreamDelta::ThinkingDelta(
                            crate::history_marker::render_preview_projection_span(
                                &marker.reference,
                                0,
                                thinking,
                            ),
                        ));
                    }
                    ContentBlock::Reasoning {
                        summary, content, ..
                    } => {
                        for (ordinal, text) in summary.iter().chain(content).enumerate() {
                            preview.push(if ordinal < summary.len() {
                                AiStreamDelta::ReasoningSummaryDelta {
                                    text: crate::history_marker::render_preview_projection_span(
                                        &marker.reference,
                                        ordinal,
                                        text,
                                    ),
                                    obfuscation: None,
                                    output_index: Some(index),
                                    content_index: Some(ordinal),
                                }
                            } else {
                                AiStreamDelta::ThinkingDeltaWithMetadata {
                                    text: crate::history_marker::render_preview_projection_span(
                                        &marker.reference,
                                        ordinal,
                                        text,
                                    ),
                                    obfuscation: None,
                                    output_index: Some(index),
                                    content_index: Some(ordinal - summary.len()),
                                }
                            });
                        }
                    }
                    ContentBlock::RedactedThinking { .. } => {}
                    _ => unreachable!("protected Thinking preview remains a reasoning block"),
                }
            }
        }
        preview
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

    fn project_text_delta(
        &mut self,
        carrier: ProjectionSpanCarrier,
        mut text: String,
        obfuscation: Option<String>,
    ) -> Vec<AiStreamDelta> {
        if let ProjectionSpanCarrier::Indexed {
            output_index: Some(index),
            ..
        } = carrier
        {
            self.projected_text_output_indices.insert(index);
        }
        let mut projected = Vec::new();
        if self.projection_span_carrier != Some(carrier) {
            projected.extend(self.close_projected_span());
            let reference = self
                .projection_reference
                .as_deref()
                .expect("Platform projection requires a History Marker");
            text = format!(
                "{}{text}",
                crate::history_marker::render_text_projection_start(
                    reference,
                    self.projection_span_ordinal,
                )
            );
            self.projection_span_carrier = Some(carrier);
        }
        projected.push(carrier.delta(text, obfuscation));
        projected
    }

    fn project_delta(&mut self, delta: AiStreamDelta) -> Vec<AiStreamDelta> {
        match delta {
            AiStreamDelta::TextDelta(text) => {
                self.project_text_delta(ProjectionSpanCarrier::Unindexed, text, None)
            }
            AiStreamDelta::TextDeltaWithMetadata {
                text,
                obfuscation,
                output_index,
                content_index,
                ..
            } => self.project_text_delta(
                ProjectionSpanCarrier::Indexed {
                    output_index,
                    content_index,
                },
                text,
                obfuscation,
            ),
            AiStreamDelta::ItemDone { index, item } => {
                let unindexed_text_item = self.projection_span_carrier
                    == Some(ProjectionSpanCarrier::Unindexed)
                    && item.role == crate::protocol::ir::Role::Assistant
                    && item.tool_calls.is_none()
                    && item.reasoning_ref().is_none()
                    && item.thinking_ref().is_none()
                    && item.unknown_ref().is_none();
                let projected_text_item =
                    self.projected_text_output_indices.remove(&index) || unindexed_text_item;
                let mut projected = self.close_projected_span();
                if !projected_text_item {
                    projected.push(AiStreamDelta::ItemDone { index, item });
                }
                projected
            }
            other => {
                let mut projected = self.close_projected_span();
                projected.push(other);
                projected
            }
        }
    }

    fn close_projected_span(&mut self) -> Vec<AiStreamDelta> {
        let Some(carrier) = self.projection_span_carrier.take() else {
            return Vec::new();
        };
        let reference = self
            .projection_reference
            .as_deref()
            .expect("Platform projection requires a History Marker");
        let ordinal = self.projection_span_ordinal;
        self.projection_span_ordinal += 1;
        vec![carrier.delta(
            crate::history_marker::render_text_projection_end(reference, ordinal),
            None,
        )]
    }

    fn project_platform_marker(&mut self, reference: &str, rendered: String) -> Vec<AiStreamDelta> {
        self.contains_platform = true;
        if self.projection_reference.is_none() {
            self.projection_reference = Some(reference.to_owned());
        }
        self.ambiguous_suffix = false;
        let pending = std::mem::take(&mut self.pending_suffix);
        let mut projected = pending
            .into_iter()
            .flat_map(|delta| self.project_delta(delta))
            .collect::<Vec<_>>();
        projected.extend(self.close_projected_span());
        projected.push(AiStreamDelta::ThinkingDelta(rendered));
        self.commit_visible(projected)
    }

    fn complete_model_leg(&mut self) -> Vec<AiStreamDelta> {
        let pending_thinking = self.flush_unindexed_thinking();
        let mut suffix = std::mem::take(&mut self.pending_suffix);
        suffix.extend(pending_thinking);
        self.ambiguous_suffix = false;
        if self.contains_platform {
            suffix = suffix
                .into_iter()
                .flat_map(|delta| self.project_delta(delta))
                .collect();
        }
        suffix.extend(self.close_projected_span());
        self.commit_visible(suffix)
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

    fn route_visible_deltas(
        &mut self,
        has_exposed_tools: bool,
        deltas: Vec<AiStreamDelta>,
    ) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for delta in deltas {
            if self.contains_platform {
                visible.extend(self.project_delta(delta));
                continue;
            }
            if has_exposed_tools {
                let starts_ambiguous_suffix = matches!(
                    delta,
                    AiStreamDelta::TextDelta(_) | AiStreamDelta::TextDeltaWithMetadata { .. }
                );
                if self.ambiguous_suffix || starts_ambiguous_suffix {
                    self.ambiguous_suffix = true;
                    self.pending_suffix.push(delta);
                    continue;
                }
            }
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
            visible.push(delta);
        }
        self.response_started |= visible
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::MessageStart { .. }));
        visible
    }

    fn filter(
        &mut self,
        run: &crate::hook::InferenceRun,
        deltas: Vec<AiStreamDelta>,
    ) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for delta in deltas {
            if matches!(delta, AiStreamDelta::Usage(_)) {
                continue;
            }
            if let Some(index) = self.protected_candidate_index(&delta) {
                if let Some(count) = self.prebuffered_protected_counts.get(&index).copied() {
                    if count <= 1 {
                        self.prebuffered_protected_counts.remove(&index);
                    } else {
                        self.prebuffered_protected_counts.insert(index, count - 1);
                    }
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
                        .push(delta);
                } else {
                    match self.pending_unindexed_thinking.as_mut() {
                        Some((pending_index, pending)) if *pending_index == index => {
                            pending.push(delta);
                        }
                        _ => {
                            self.pending_unindexed_thinking = Some((index, vec![delta]));
                        }
                    }
                }
                continue;
            }
            let kind = Self::unindexed_item_kind(&delta);
            if kind != Some(UnindexedItemKind::Thinking)
                && self.pending_unindexed_signature.is_none()
                && self.pending_unindexed_thinking.is_some()
            {
                let pending_thinking = self.flush_unindexed_thinking();
                visible
                    .extend(self.route_visible_deltas(run.has_exposed_tools(), pending_thinking));
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
                        let is_platform = run.is_exposed_tool(accumulated);
                        let remains_ambiguous = run.could_be_exposed_tool_prefix(accumulated);
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
                                visible.extend(
                                    self.route_visible_deltas(run.has_exposed_tools(), pending),
                                );
                            }
                            self.pending_tool_names.remove(&index);
                        }
                        continue;
                    }
                    if run.is_exposed_tool(name) {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if run.could_be_exposed_tool_prefix(name) {
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
                    if run.is_exposed_tool(&tool_call.name) {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if let Some(pending) = self.pending_tool_deltas.remove(index) {
                        visible.extend(self.route_visible_deltas(run.has_exposed_tools(), pending));
                    }
                    self.pending_tool_names.remove(index);
                }
                AiStreamDelta::ItemDone { index, item } => {
                    let platform = item
                        .function_call_ref()
                        .is_some_and(|call| run.is_exposed_tool(&call.name));
                    if platform {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if let Some(pending) = self.pending_tool_deltas.remove(index) {
                        visible.extend(self.route_visible_deltas(run.has_exposed_tools(), pending));
                    }
                    self.pending_tool_names.remove(index);
                }
                _ => {}
            }
            let hidden_platform_delta = match &delta {
                AiStreamDelta::ToolCallStart { index, name, .. } if run.is_exposed_tool(name) => {
                    self.platform_tool_indices.insert(*index);
                    true
                }
                AiStreamDelta::ToolCallDelta { index, .. } => {
                    self.platform_tool_indices.contains(index)
                }
                AiStreamDelta::ToolCallComplete { index, tool_call } => {
                    let hidden = self.platform_tool_indices.contains(index)
                        || run.is_exposed_tool(&tool_call.name);
                    if hidden {
                        self.platform_tool_indices.insert(*index);
                    }
                    hidden
                }
                AiStreamDelta::ItemDone { index, item } => {
                    let hidden = self.platform_tool_indices.contains(index)
                        || item
                            .function_call_ref()
                            .is_some_and(|call| run.is_exposed_tool(&call.name));
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

            visible.extend(self.route_visible_deltas(run.has_exposed_tools(), vec![delta]));
        }
        visible
    }
}

fn projected_marker_carriers(response: &AiResponse) -> Vec<(String, String)> {
    let mut carriers = Vec::new();
    for item in &response.items {
        let text = item
            .thinking_ref()
            .map(|(text, _)| text)
            .or_else(|| item.output_text_ref());
        let Some(text) =
            text.filter(|text| text.contains(crate::history_marker::HISTORY_MARKER_PREFIX))
        else {
            continue;
        };
        for reference in
            crate::history_marker::history_marker_references(std::slice::from_ref(item))
        {
            carriers.push((reference, text.to_owned()));
        }
    }
    carriers
}

fn marker_deltas(
    response: &AiResponse,
    platform_references: &HashSet<String>,
    emitted: &mut HashSet<String>,
    gate: &mut LiveDeltaGate,
) -> Vec<AiStreamDelta> {
    let mut deltas = Vec::new();
    for (reference, rendered) in projected_marker_carriers(response) {
        if !emitted.insert(rendered.clone()) {
            continue;
        }
        if platform_references.contains(&reference) {
            deltas.extend(gate.project_platform_marker(&reference, rendered));
        } else {
            deltas.extend(gate.commit_visible(vec![AiStreamDelta::ThinkingDelta(rendered)]));
        }
    }
    deltas
}

pub(super) async fn handle_model_turn_stream(input: ModelTurnStreamInput) -> RoundOutcome {
    let ModelTurnStreamInput {
        mut turn,
        executor,
        gateway,
        headers,
        ingress,
        request_context,
        mut request,
        generation,
        mut inference_run,
        mut phase,
        start,
        mut turn_started,
        request_extras,
        log,
    } = input;
    let egress = turn.route.egress;
    let previous_response_id = generation.previous_response_id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Infallible>>(64);
    let receiver_watch = tx.clone();
    let (preflight_tx, preflight_rx) = tokio::sync::oneshot::channel::<Result<(), RoundOutcome>>();
    let (commit_tx, commit_rx) = tokio::sync::oneshot::channel();
    let (terminal_delivery_tx, terminal_delivery_rx) = tokio::sync::oneshot::channel();
    let cancellation = request_context.cancellation.clone();
    let redact_payloads = request.meta.media_routing.is_some();
    let fixed_media_plan = request.meta.media_routing.clone();

    tokio::spawn(async move {
        let mut delivery = DeliveryAdapter::live_stream(LiveStreamRequest {
            ingress,
            egress,
            tx,
            cancellation: cancellation.clone(),
            preflight: preflight_tx,
            terminal_delivery: terminal_delivery_rx,
            commit: commit_rx,
            capture_payload: true,
        });
        delivery.set_response_profile(&request, previous_response_id.as_deref());
        let mut live_delta_gate = LiveDeltaGate::default();
        let mut emitted_marker_texts = HashSet::new();
        'model_legs: loop {
            live_delta_gate.begin_model_leg(turn.route.egress.protocol);
            let attempt_trace = turn.transport.clone();
            let mut completion_context = CompletionContext::from_model_turn(
                gateway.clone(),
                generation.clone(),
                ingress,
                &turn.target,
                turn.route.egress,
            );
            let mut output = turn.output;
            let buffer_terminal_hooks = inference_run.requires_terminal_buffering();
            let mut hook_leg = HookLegGuard::new(&mut inference_run);
            let mut accumulator = StreamResponseAccumulator::default();
            let mut terminal_deltas = Vec::new();
            let mut completed_response = None;
            let mut upstream_response_id = None;
            let mut aborted = false;
            let mut committed_failure_delivered = false;
            let mut final_client_status = None;
            let mut cancelled = false;
            let mut receiver_closed = false;
            let mut protocol_failed = false;
            let mut preflight_failure = None;
            let mut leg_client_output_committed = false;
            let mut early_platform_executions = Vec::new();
            let mut early_thinking_markers = Vec::new();
            let mut deferred_thinking_publish_references = Vec::new();

            while !aborted && !cancelled && !receiver_closed && !protocol_failed {
                let event = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    _ = receiver_watch.closed() => {
                        receiver_closed = true;
                        break;
                    }
                    event = output.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    Ok(CanonicalEvent::Delta(delta)) => {
                        let (terminal, deltas) = partition_terminal_deltas(vec![delta]);
                        let tool_calls_complete = terminal.iter().any(|delta| {
                            matches!(
                                delta,
                                AiStreamDelta::Done { stop_reason }
                                    if stop_reason == "tool_calls"
                            )
                        });
                        if terminal_deltas_failed(&terminal) {
                            aborted = true;
                            preflight_failure = Some(buffered_response(error_response(
                                502,
                                "upstream stream error",
                            )));
                        }
                        terminal_deltas.extend(terminal);
                        let mut transformed =
                            match transform_stream_deltas(hook_leg.run_mut(), deltas) {
                                Ok(deltas) => deltas,
                                Err(error) => {
                                    aborted = true;
                                    preflight_failure =
                                        Some(buffered_response(hook_failure_response(error)));
                                    break;
                                }
                            };
                        if upstream_response_id.is_none() {
                            upstream_response_id =
                                transformed.iter().find_map(|delta| match delta {
                                    AiStreamDelta::MessageStart { id, .. } if !id.is_empty() => {
                                        Some(id.clone())
                                    }
                                    _ => None,
                                });
                        }
                        apply_response_identity(
                            &mut transformed,
                            completion_context.generation_chain_identity(),
                        );
                        if !buffer_terminal_hooks {
                            live_delta_gate.capture_protected_candidates(&transformed);
                        }
                        if !buffer_terminal_hooks && live_delta_gate.buffer_unindexed_protected {
                            live_delta_gate.capture_unindexed_signatures(&mut transformed);
                            let thinking_completed = !terminal_deltas.is_empty()
                                || transformed
                                    .iter()
                                    .any(LiveDeltaGate::ends_unindexed_thinking);
                            if thinking_completed
                                && let Some((index, item)) =
                                    live_delta_gate.synthetic_signed_thinking_item()
                            {
                                transformed.insert(0, AiStreamDelta::ItemDone { index, item });
                            }
                        }
                        accumulator.apply_all(&transformed);
                        if !buffer_terminal_hooks {
                            let completed_thinking_items = transformed
                                .iter()
                                .filter_map(|delta| match delta {
                                    AiStreamDelta::ItemDone { index, item }
                                        if !early_thinking_markers.iter().any(
                                            |early: &EarlyThinkingMarkers| {
                                                early.output_index == *index
                                            },
                                        ) =>
                                    {
                                        Some((*index, item))
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            for (output_index, item) in completed_thinking_items {
                                let markers = match prepare_thinking_markers(
                                    &completion_context,
                                    &item,
                                )
                                .await
                                {
                                    Ok(markers) => markers,
                                    Err(error) => {
                                        aborted = true;
                                        preflight_failure =
                                            Some(buffered_response(render_completion_failure(
                                                CompletionFailure::hook(
                                                    error,
                                                    completion_context.client_output_commit(),
                                                ),
                                                ingress,
                                                true,
                                            )));
                                        break;
                                    }
                                };
                                let mut marker_deltas = live_delta_gate.resolve_completed_item(
                                    output_index,
                                    &item,
                                    &markers,
                                );
                                if markers.is_empty() {
                                    if marker_deltas.is_empty() {
                                        continue;
                                    }
                                    let marker_deltas = live_delta_gate.route_visible_deltas(
                                        hook_leg.run_mut().has_exposed_tools(),
                                        marker_deltas,
                                    );
                                    if marker_deltas.is_empty() {
                                        continue;
                                    }
                                    match delivery.send_deltas(&marker_deltas).await {
                                        DeliveryProgress::Sent => {
                                            leg_client_output_committed = true;
                                        }
                                        DeliveryProgress::Cancelled => {
                                            cancelled = true;
                                            break;
                                        }
                                        DeliveryProgress::ReceiverClosed => {
                                            receiver_closed = true;
                                            break;
                                        }
                                        DeliveryProgress::ProtocolFailed => {
                                            protocol_failed = true;
                                            break;
                                        }
                                    }
                                    continue;
                                }
                                let references = markers
                                    .iter()
                                    .map(|marker| marker.reference.clone())
                                    .collect::<Vec<_>>();
                                marker_deltas.extend(markers.iter().map(|marker| {
                                    let rendered =
                                        crate::history_marker::render_history_marker(marker);
                                    emitted_marker_texts.insert(rendered.clone());
                                    AiStreamDelta::ThinkingDelta(rendered)
                                }));
                                let marker_deltas = live_delta_gate.route_visible_deltas(
                                    hook_leg.run_mut().has_exposed_tools(),
                                    marker_deltas,
                                );
                                if marker_deltas.is_empty() {
                                    deferred_thinking_publish_references
                                        .extend(references.iter().cloned());
                                    early_thinking_markers.push(EarlyThinkingMarkers {
                                        output_index,
                                        markers,
                                    });
                                    continue;
                                }
                                match delivery.send_deltas(&marker_deltas).await {
                                    DeliveryProgress::Sent => {
                                        leg_client_output_committed = true;
                                    }
                                    DeliveryProgress::Cancelled => {
                                        cancelled = true;
                                        break;
                                    }
                                    DeliveryProgress::ReceiverClosed => {
                                        receiver_closed = true;
                                        break;
                                    }
                                    DeliveryProgress::ProtocolFailed => {
                                        protocol_failed = true;
                                        break;
                                    }
                                }
                                if let Err(error) =
                                    publish_markers(&completion_context, &references).await
                                {
                                    tracing::error!(
                                        "failed to publish streamed thinking marker: {error}"
                                    );
                                    aborted = true;
                                    break;
                                }
                                early_thinking_markers.push(EarlyThinkingMarkers {
                                    output_index,
                                    markers,
                                });
                            }
                            if aborted || cancelled || receiver_closed || protocol_failed {
                                break;
                            }
                            let mut completed_platform_calls = transformed
                                .iter()
                                .filter_map(|delta| match delta {
                                    AiStreamDelta::ToolCallComplete { tool_call, .. } => {
                                        Some(tool_call.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            if tool_calls_complete {
                                completed_platform_calls.extend(accumulator.tool_calls().cloned());
                            }
                            let mut completed_call_ids = HashSet::new();
                            completed_platform_calls.retain(|call| {
                                hook_leg.run_mut().is_exposed_tool(&call.name)
                                    && completed_call_ids.insert(call.id.clone())
                                    && !early_platform_executions.iter().any(
                                        |early: &EarlyPlatformExecution| {
                                            early.marker.call_id() == call.id
                                        },
                                    )
                            });
                            for call in completed_platform_calls {
                                let platform_call = hook_leg
                                    .run_mut()
                                    .classify_tool_calls(&AiResponse {
                                        items: vec![crate::protocol::ir::AiItem::function_call(
                                            call,
                                        )],
                                        ..completion_context.empty_response()
                                    })
                                    .platform
                                    .into_iter()
                                    .next()
                                    .expect("classified Platform Tool call");
                                let execution = hook_leg.run_mut().detached_platform_execution(
                                    platform_call,
                                    crate::proxy::context::CancellationToken::new(),
                                );
                                let (markers, jobs) = match prepare_platform_markers(
                                    &completion_context,
                                    vec![execution],
                                )
                                .await
                                {
                                    Ok(prepared) => prepared,
                                    Err(error) => {
                                        aborted = true;
                                        preflight_failure =
                                            Some(buffered_response(render_completion_failure(
                                                CompletionFailure::hook(
                                                    error,
                                                    completion_context.client_output_commit(),
                                                ),
                                                ingress,
                                                true,
                                            )));
                                        break;
                                    }
                                };
                                let references = markers
                                    .iter()
                                    .map(|marker| marker.reference().to_owned())
                                    .collect::<Vec<_>>();
                                let marker_deltas = markers
                                    .iter()
                                    .flat_map(|marker| {
                                        let rendered = marker.render();
                                        emitted_marker_texts.insert(rendered.clone());
                                        live_delta_gate
                                            .project_platform_marker(marker.reference(), rendered)
                                    })
                                    .collect::<Vec<_>>();
                                match delivery.send_deltas(&marker_deltas).await {
                                    DeliveryProgress::Sent => {
                                        leg_client_output_committed = true;
                                    }
                                    DeliveryProgress::Cancelled => {
                                        cancelled = true;
                                        break;
                                    }
                                    DeliveryProgress::ReceiverClosed => {
                                        receiver_closed = true;
                                        break;
                                    }
                                    DeliveryProgress::ProtocolFailed => {
                                        protocol_failed = true;
                                        break;
                                    }
                                }
                                let mut delivered_references =
                                    std::mem::take(&mut deferred_thinking_publish_references);
                                delivered_references.extend(references.iter().cloned());
                                if let Err(error) =
                                    publish_markers(&completion_context, &delivered_references)
                                        .await
                                {
                                    tracing::error!(
                                        "failed to publish streamed history marker: {error}"
                                    );
                                    aborted = true;
                                    break;
                                }
                                let mut published = request_context
                                    .extensions
                                    .get::<PublishedPlatformExecutions>()
                                    .unwrap_or_default();
                                published.references.extend(references);
                                request_context.extensions.insert(published);
                                let started = gateway.start_history_marker_executions(
                                    completion_context.principal().clone(),
                                    jobs,
                                );
                                early_platform_executions.extend(
                                    markers.into_iter().zip(started).map(|(marker, execution)| {
                                        EarlyPlatformExecution { marker, execution }
                                    }),
                                );
                            }
                        }
                        if !buffer_terminal_hooks {
                            let visible = live_delta_gate.filter(hook_leg.run_mut(), transformed);
                            let has_visible = !visible.is_empty();
                            match delivery.send_deltas(&visible).await {
                                DeliveryProgress::Sent => {
                                    leg_client_output_committed |= has_visible;
                                }
                                DeliveryProgress::Cancelled => cancelled = true,
                                DeliveryProgress::ReceiverClosed => receiver_closed = true,
                                DeliveryProgress::ProtocolFailed => protocol_failed = true,
                            }
                        }
                    }
                    Ok(CanonicalEvent::Completed(response)) => {
                        completed_response = Some(*response);
                    }
                    Err(error) => {
                        aborted = true;
                        preflight_failure = Some(super::model_turn_error_outcome(error));
                    }
                }
            }

            match hook_leg.close().await {
                Ok(mut flushed) => {
                    apply_response_identity(
                        &mut flushed,
                        completion_context.generation_chain_identity(),
                    );
                    accumulator.apply_all(&flushed);
                    if !buffer_terminal_hooks && !cancelled && !receiver_closed && !protocol_failed
                    {
                        let visible = live_delta_gate.filter(hook_leg.run_mut(), flushed);
                        let has_visible = !visible.is_empty();
                        match delivery.send_deltas(&visible).await {
                            DeliveryProgress::Sent => {
                                leg_client_output_committed |= has_visible;
                            }
                            DeliveryProgress::Cancelled => cancelled = true,
                            DeliveryProgress::ReceiverClosed => receiver_closed = true,
                            DeliveryProgress::ProtocolFailed => protocol_failed = true,
                        }
                    }
                }
                Err(error) => {
                    aborted = true;
                    preflight_failure = Some(buffered_response(hook_failure_response(error)));
                }
            }
            accumulator.apply_all(&terminal_deltas);
            let mut response = accumulator.into_ai_response();
            if let Some(completed) = completed_response {
                if response.usage.prompt_tokens == 0 && response.usage.completion_tokens == 0 {
                    response.usage = completed.usage;
                }
                if response.stop_reason.is_none() {
                    response.stop_reason = completed.stop_reason;
                }
                if response.id.is_empty() {
                    response.id = completed.id;
                }
            } else if !aborted && !cancelled && !receiver_closed {
                aborted = true;
                preflight_failure = Some(buffered_response(error_response(
                    502,
                    "Model Turn ended without a completion",
                )));
            }
            let leg_egress = turn.route.egress;
            let stream_metrics = attempt_trace.stream_metrics();
            let mut model_turn_log = Some(
                log.clone()
                    .model_turn(&turn.route, &turn.target)
                    .status(200)
                    .usage(response.usage.clone())
                    .upstream_protocol(&leg_egress.to_string())
                    .upstream_url(&attempt_trace.upstream_url)
                    .with_upstream_request(
                        attempt_trace.request_headers.clone(),
                        attempt_trace.request_body.clone(),
                    )
                    .with_upstream_response(
                        200,
                        attempt_trace
                            .response_headers
                            .lock()
                            .expect("response headers")
                            .clone(),
                        (!redact_payloads).then(|| {
                            String::from_utf8_lossy(
                                &attempt_trace.response_body.lock().expect("response body"),
                            )
                            .into_owned()
                        }),
                        None,
                    )
                    .stream_metrics(stream_metrics.chunks_count, stream_metrics.first_chunk_ms)
                    .model_turn_completed(turn_started),
            );

            let mut pending_generation_chain = None;
            let mut background_executions = Vec::new();
            let mut started_executions = Vec::new();
            let mut publish_references = Vec::new();
            if !aborted && !cancelled && !receiver_closed && !protocol_failed {
                if leg_client_output_committed {
                    completion_context.mark_client_output_committed();
                }
                let commit = completion_context.client_output_commit();
                match complete_canonical_response(
                    &completion_context,
                    CompletionInput {
                        request_context: &request_context,
                        request: &mut request,
                        run: hook_leg.run_mut(),
                        phase: &mut phase,
                        response,
                        upstream_response_id,
                        early_platform_executions,
                        early_thinking_markers,
                        allow_platform_only: true,
                    },
                )
                .await
                {
                    CompletionOutcome::PlatformOnly(continuation) => {
                        model_turn_log
                            .take()
                            .expect("current Model Turn log")
                            .without_client_exchange()
                            .emit();
                        let platform_references =
                            projected_marker_carriers(continuation.projected_response())
                                .into_iter()
                                .map(|(reference, _)| reference)
                                .collect::<HashSet<_>>();
                        let marker_deltas = marker_deltas(
                            continuation.projected_response(),
                            &platform_references,
                            &mut emitted_marker_texts,
                            &mut live_delta_gate,
                        );
                        match delivery.send_deltas(&marker_deltas).await {
                            DeliveryProgress::Sent => {}
                            DeliveryProgress::Cancelled => cancelled = true,
                            DeliveryProgress::ReceiverClosed => receiver_closed = true,
                            DeliveryProgress::ProtocolFailed => protocol_failed = true,
                        }
                        response = completion_context.empty_response();
                        if !cancelled && !receiver_closed && !protocol_failed {
                            if let Err(failure) = continuation.publish(&completion_context).await {
                                if commit == ClientOutputCommit::Pending {
                                    preflight_failure = Some(buffered_response(
                                        render_completion_failure(failure, ingress, true),
                                    ));
                                }
                                aborted = true;
                            } else if let Err(failure) = continuation
                                .finish(
                                    &completion_context,
                                    &request_context,
                                    &mut request,
                                    hook_leg.run_mut(),
                                    &mut phase,
                                )
                                .await
                            {
                                if commit == ClientOutputCommit::Pending {
                                    preflight_failure = Some(buffered_response(
                                        render_completion_failure(failure, ingress, true),
                                    ));
                                }
                                aborted = true;
                            }
                        } else {
                            aborted = true;
                        }
                        if !aborted {
                            match acquire_followup_model_turn(
                                executor.as_ref(),
                                &gateway,
                                &headers,
                                &mut request,
                                ingress,
                                &request_context,
                                hook_leg.run_mut(),
                                &mut phase,
                                completion_context.principal(),
                                &generation,
                                fixed_media_plan.as_ref(),
                                start,
                                &request_extras,
                            )
                            .await
                            {
                                Ok(FollowupModelTurn::Turn(next_turn, next_turn_started)) => {
                                    turn = next_turn;
                                    turn_started = next_turn_started;
                                    continue 'model_legs;
                                }
                                Ok(FollowupModelTurn::HookResponse {
                                    response: hook_response,
                                    pending_generation_chain: hook_generation_chain,
                                }) => {
                                    model_turn_log = Some(
                                        LogBuilder::from_dispatch(
                                            &gateway,
                                            &ingress.to_string(),
                                            &request.model,
                                            request.reasoning.level,
                                            request_context.auth_subject.as_ref(),
                                            start,
                                        )
                                        .stream_flag(true)
                                        .status(200)
                                        .with_req_extras(&request_extras),
                                    );
                                    response = hook_response;
                                    pending_generation_chain = hook_generation_chain;
                                    if !buffer_terminal_hooks {
                                        live_delta_gate.begin_model_leg(ingress.protocol);
                                        let mut deltas = ai_response_to_deltas(&response);
                                        terminal_deltas = deltas
                                            .iter()
                                            .filter(|delta| {
                                                matches!(
                                                    delta,
                                                    AiStreamDelta::ResponseTerminal { .. }
                                                )
                                            })
                                            .cloned()
                                            .collect();
                                        deltas.retain(|delta| {
                                            !matches!(
                                                delta,
                                                AiStreamDelta::Usage(_)
                                                    | AiStreamDelta::ResponseTerminal { .. }
                                                    | AiStreamDelta::Done { .. }
                                            )
                                        });
                                        let deltas =
                                            live_delta_gate.route_visible_deltas(false, deltas);
                                        match delivery.send_deltas(&deltas).await {
                                            DeliveryProgress::Sent => {}
                                            DeliveryProgress::Cancelled => cancelled = true,
                                            DeliveryProgress::ReceiverClosed => {
                                                receiver_closed = true
                                            }
                                            DeliveryProgress::ProtocolFailed => {
                                                protocol_failed = true
                                            }
                                        }
                                    }
                                }
                                Ok(FollowupModelTurn::StreamError(error)) => {
                                    final_client_status = error.status_code;
                                    model_turn_log = Some(
                                        LogBuilder::from_dispatch(
                                            &gateway,
                                            &ingress.to_string(),
                                            &request.model,
                                            request.reasoning.level,
                                            request_context.auth_subject.as_ref(),
                                            start,
                                        )
                                        .stream_flag(true)
                                        .status(error.status_code.unwrap_or(500))
                                        .with_req_extras(&request_extras),
                                    );
                                    let error = [AiStreamDelta::StreamError { error }];
                                    if delivery.send_deltas(&error).await == DeliveryProgress::Sent
                                        && delivery.finish_stream("failed".into()).await
                                            == DeliveryProgress::Sent
                                    {
                                        committed_failure_delivered = true;
                                    }
                                    aborted = true;
                                }
                                Err(outcome) => {
                                    preflight_failure = Some(outcome);
                                    aborted = true;
                                }
                            }
                        }
                    }
                    CompletionOutcome::PlatformOnlyRejected => {
                        unreachable!("live delivery permits Platform-only continuation")
                    }
                    CompletionOutcome::Ready(lease) => match (*lease).prepare(&mut phase) {
                        Ok(prepared) => {
                            response = prepared.response;
                            pending_generation_chain = prepared.pending_generation_chain;
                            background_executions = prepared.background_executions;
                            started_executions = prepared.started_executions;
                            publish_references = prepared.publish_references;
                        }
                        Err(failure) => {
                            if commit == ClientOutputCommit::Pending {
                                preflight_failure = Some(buffered_response(
                                    render_completion_failure(failure, ingress, true),
                                ));
                            }
                            response = completion_context.empty_response();
                            aborted = true;
                        }
                    },
                    CompletionOutcome::Failed(failure) => {
                        if commit == ClientOutputCommit::Pending {
                            preflight_failure = Some(buffered_response(render_completion_failure(
                                failure, ingress, true,
                            )));
                        }
                        response = completion_context.empty_response();
                        aborted = true;
                    }
                }
            }
            publish_references.extend(deferred_thinking_publish_references);
            drop(hook_leg);
            let mut owned_run = Some(inference_run);
            let mut owned_phase = Some(phase);
            let mut marker_output_delivered = false;

            if !buffer_terminal_hooks
                && preflight_failure.is_none()
                && !aborted
                && !cancelled
                && !receiver_closed
                && !protocol_failed
            {
                let platform_references = request_context
                    .extensions
                    .get::<PublishedPlatformExecutions>()
                    .unwrap_or_default()
                    .references
                    .into_iter()
                    .collect::<HashSet<_>>();
                let markers = marker_deltas(
                    &response,
                    &platform_references,
                    &mut emitted_marker_texts,
                    &mut live_delta_gate,
                );
                if !markers.is_empty() {
                    match delivery.send_deltas(&markers).await {
                        DeliveryProgress::Sent => {}
                        DeliveryProgress::Cancelled => cancelled = true,
                        DeliveryProgress::ReceiverClosed => receiver_closed = true,
                        DeliveryProgress::ProtocolFailed => protocol_failed = true,
                    }
                }
                if !cancelled && !receiver_closed && !protocol_failed {
                    let suffix = live_delta_gate.complete_model_leg();
                    if !suffix.is_empty() {
                        match delivery.send_deltas(&suffix).await {
                            DeliveryProgress::Sent => {}
                            DeliveryProgress::Cancelled => cancelled = true,
                            DeliveryProgress::ReceiverClosed => receiver_closed = true,
                            DeliveryProgress::ProtocolFailed => protocol_failed = true,
                        }
                    }
                }
                if !cancelled && !receiver_closed && !protocol_failed {
                    let usage = [AiStreamDelta::Usage(response.usage.clone())];
                    match delivery.send_deltas(&usage).await {
                        DeliveryProgress::Sent => {}
                        DeliveryProgress::Cancelled => cancelled = true,
                        DeliveryProgress::ReceiverClosed => receiver_closed = true,
                        DeliveryProgress::ProtocolFailed => protocol_failed = true,
                    }
                }
                if !cancelled && !receiver_closed && !protocol_failed {
                    let response_terminal = terminal_deltas
                        .iter()
                        .filter(|delta| matches!(delta, AiStreamDelta::ResponseTerminal { .. }))
                        .cloned()
                        .collect::<Vec<_>>();
                    match delivery.send_deltas(&response_terminal).await {
                        DeliveryProgress::Sent => {}
                        DeliveryProgress::Cancelled => cancelled = true,
                        DeliveryProgress::ReceiverClosed => receiver_closed = true,
                        DeliveryProgress::ProtocolFailed => protocol_failed = true,
                    }
                }
                marker_output_delivered = !cancelled && !receiver_closed && !protocol_failed;
            }

            if buffer_terminal_hooks
                && preflight_failure.is_none()
                && !aborted
                && !cancelled
                && !receiver_closed
                && !protocol_failed
            {
                delivery.reset_stream_encoder();
                let mut final_deltas = ai_response_to_deltas(&response);
                final_deltas.retain(|delta| !matches!(delta, AiStreamDelta::Done { .. }));
                match delivery.send_deltas(&final_deltas).await {
                    DeliveryProgress::Sent => {}
                    DeliveryProgress::Cancelled => cancelled = true,
                    DeliveryProgress::ReceiverClosed => receiver_closed = true,
                    DeliveryProgress::ProtocolFailed => protocol_failed = true,
                }
                marker_output_delivered = !cancelled && !receiver_closed && !protocol_failed;
            }

            if marker_output_delivered && !publish_references.is_empty() {
                if let Err(error) = publish_markers(&completion_context, &publish_references).await
                {
                    tracing::error!("failed to publish delivered history markers: {error}");
                    aborted = true;
                }
            }
            if marker_output_delivered && !aborted && !background_executions.is_empty() {
                started_executions.extend(gateway.start_history_marker_executions(
                    completion_context.principal().clone(),
                    background_executions,
                ));
            }
            if marker_output_delivered && !aborted && !started_executions.is_empty() {
                gateway.spawn_started_history_marker_executions(
                    started_executions,
                    owned_run
                        .take()
                        .expect("background Platform execution requires its Inference Run"),
                );
            }

            let preflight_failed = if let Some(outcome) = preflight_failure.take() {
                let outcome = match (owned_run.take(), owned_phase.take()) {
                    (Some(run), Some(phase)) => outcome.with_lifecycle(run, phase),
                    _ => outcome,
                };
                delivery.fail_before_commit(outcome)
            } else if cancelled {
                let response = if request_context.deadline.is_exceeded() {
                    error_response(504, "request deadline exceeded")
                } else {
                    error_response(499, "request cancelled")
                };
                delivery.fail_before_commit(buffered_response(response))
            } else {
                false
            };

            let mut terminal_delivered = false;
            if aborted && !preflight_failed && !committed_failure_delivered {
                if !cancelled && !receiver_closed && !protocol_failed {
                    let error = [AiStreamDelta::StreamError {
                        error: crate::protocol::ir::AiError::new(
                            crate::protocol::ir::AiErrorKind::StreamMidError,
                            "stream aborted",
                        ),
                    }];
                    if delivery.send_deltas(&error).await == DeliveryProgress::Sent {
                        let _ = delivery.finish_stream("failed".into()).await;
                    }
                }
            } else if !aborted
                && !preflight_failed
                && !cancelled
                && !receiver_closed
                && !protocol_failed
                && delivery
                    .finish_stream(
                        response
                            .stop_reason
                            .clone()
                            .unwrap_or_else(|| "stop".into()),
                    )
                    .await
                    == DeliveryProgress::Sent
            {
                terminal_delivered =
                    delivery.wait_for_terminal_delivery().await == DeliveryProgress::Sent;
            }

            if terminal_delivered {
                let generation_committed = if let Some(mut pending) =
                    pending_generation_chain.take()
                {
                    match pending.persist().await {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::error!(
                                "failed to commit Generation Chain node after terminal delivery: {error}"
                            );
                            false
                        }
                    }
                } else {
                    true
                };
                let _ = generation_committed;
            }
            if let Some(mut phase) = owned_phase.take() {
                phase.finish();
            }
            let client_response_body = if redact_payloads {
                None
            } else {
                delivery.captured_body()
            };
            if let Some(model_turn_log) = model_turn_log.take() {
                model_turn_log
                    .status(final_client_status.unwrap_or(if aborted { 500 } else { 200 }))
                    .with_client_response(None, client_response_body)
                    .emit();
            }
            break 'model_legs;
        }
    });

    match preflight_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(response)) => return response,
        Err(_) => {
            return buffered_response(error_response(
                502,
                "Model Turn stream ended before delivery",
            ));
        }
    }
    live_response(DeliveryAdapter::response_from_receiver(
        rx,
        commit_tx,
        terminal_delivery_tx,
        ingress,
    ))
}

// ── Streaming response handler ────────────────────────────────────────────────

pub(super) fn apply_response_identity(
    deltas: &mut [AiStreamDelta],
    identity: Option<(&str, &str)>,
) {
    let Some((response_id, logical_model)) = identity else {
        return;
    };
    for delta in deltas {
        if let AiStreamDelta::MessageStart { id, model } = delta {
            *id = response_id.to_owned();
            *model = logical_model.to_owned();
        }
    }
}

pub(super) fn partition_terminal_deltas(
    deltas: Vec<AiStreamDelta>,
) -> (Vec<AiStreamDelta>, Vec<AiStreamDelta>) {
    deltas.into_iter().partition(|delta| {
        matches!(
            delta,
            AiStreamDelta::ResponseTerminal { .. }
                | AiStreamDelta::Done { .. }
                | AiStreamDelta::StreamError { .. }
                | AiStreamDelta::UnexpectedEof
        )
    })
}

pub(super) fn terminal_deltas_failed(deltas: &[AiStreamDelta]) -> bool {
    deltas.iter().any(|delta| {
        matches!(
            delta,
            AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
        )
    })
}

pub(super) fn transform_stream_deltas(
    inference_run: &mut crate::hook::InferenceRun,
    deltas: Vec<AiStreamDelta>,
) -> Result<Vec<AiStreamDelta>, crate::hook::HookError> {
    let mut transformed = Vec::new();
    for delta in deltas {
        transformed.extend(inference_run.transform_stream(delta)?);
    }
    Ok(transformed)
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn model_leg_completion_flushes_pending_unindexed_thinking() {
        let mut gate = LiveDeltaGate {
            pending_unindexed_thinking: Some((
                0,
                vec![AiStreamDelta::ThinkingDelta("reasoning".into())],
            )),
            ..Default::default()
        };

        let visible = gate.complete_model_leg();

        assert!(matches!(
            visible.as_slice(),
            [AiStreamDelta::ThinkingDelta(text)] if text == "reasoning"
        ));
    }

    #[test]
    fn unindexed_thinking_keeps_ambiguous_suffix_order() {
        let mut gate = LiveDeltaGate {
            pending_suffix: vec![AiStreamDelta::TextDelta("earlier text".into())],
            ambiguous_suffix: true,
            ..Default::default()
        };

        let visible = gate.route_visible_deltas(
            true,
            vec![AiStreamDelta::ThinkingDelta("later reasoning".into())],
        );
        assert!(visible.is_empty());

        let visible = gate.complete_model_leg();
        assert!(matches!(
            visible.as_slice(),
            [
                AiStreamDelta::TextDelta(text),
                AiStreamDelta::ThinkingDelta(reasoning),
            ] if text == "earlier text" && reasoning == "later reasoning"
        ));
    }

    #[test]
    fn indexed_projection_closes_on_the_same_reasoning_item_and_consumes_text_item_done() {
        let mut gate = LiveDeltaGate {
            projection_reference: Some("hm_0123456789abcdefghij".into()),
            ..Default::default()
        };
        let mut visible = gate.project_delta(AiStreamDelta::TextDeltaWithMetadata {
            text: "projected".into(),
            logprobs: Vec::new(),
            obfuscation: None,
            output_index: Some(3),
            content_index: Some(1),
        });
        visible.extend(gate.project_delta(AiStreamDelta::ItemDone {
            index: 3,
            item: AiItem {
                role: crate::protocol::ir::Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "first".into(),
                        cache_control: None,
                    },
                    ContentBlock::Text {
                        text: "second".into(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        }));

        assert_eq!(visible.len(), 2);
        let texts = visible
            .iter()
            .map(|delta| match delta {
                AiStreamDelta::ThinkingDeltaWithMetadata {
                    text,
                    output_index: Some(3),
                    content_index: Some(1),
                    ..
                } => text.as_str(),
                other => panic!("unexpected projected delta: {other:?}"),
            })
            .collect::<String>();
        assert!(texts.contains(":text:0:start -->projected"), "{texts}");
        assert!(texts.contains(":text:0:end -->"), "{texts}");
    }

    #[test]
    fn signature_fragments_remain_buffered_until_the_thinking_item_completes() {
        let mut gate = LiveDeltaGate {
            pending_unindexed_thinking: Some((
                0,
                vec![AiStreamDelta::ThinkingDelta("reasoning".into())],
            )),
            ..Default::default()
        };
        let mut signatures = vec![
            AiStreamDelta::ThinkingSignature("opaque-".into()),
            AiStreamDelta::ThinkingSignature("signature".into()),
        ];

        gate.capture_unindexed_signatures(&mut signatures);

        assert!(signatures.is_empty());
        let (_, item) = gate
            .synthetic_signed_thinking_item()
            .expect("completed signed Thinking item");
        assert!(matches!(
            item.content,
            MessageContent::Blocks(ref blocks)
                if matches!(
                    blocks.as_slice(),
                    [ContentBlock::Thinking { thinking, signature: Some(signature) }]
                        if thinking == "reasoning" && signature == "opaque-signature"
                )
        ));
    }

    #[test]
    fn response_terminal_is_dispatched_with_done() {
        let (terminal, content) = partition_terminal_deltas(vec![
            AiStreamDelta::TextDelta("partial".into()),
            AiStreamDelta::ResponseTerminal {
                status: "incomplete".into(),
                incomplete_details: Some(serde_json::json!({"reason": "max_output_tokens"})),
            },
            AiStreamDelta::Done {
                stop_reason: "length".into(),
            },
        ]);

        assert_eq!(content.len(), 1);
        assert!(matches!(
            terminal.as_slice(),
            [
                AiStreamDelta::ResponseTerminal { status, .. },
                AiStreamDelta::Done { .. },
            ] if status == "incomplete"
        ));
    }
}

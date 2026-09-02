//! Client Projection for one Inference Run.
//!
//! Canonical Text is delivered unchanged. OpenAI-compatible clients keep
//! Thinking on the reasoning carrier until the first non-empty Text, then use
//! quoted `content` previews bound to authoritative Thinking History Markers.
//! Other protocols retain their native carriers.

use std::collections::{BTreeMap, HashMap, VecDeque};
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

pub(super) struct ClosedThinking {
    pub(super) finish_deltas: Vec<AiStreamDelta>,
    pub(super) preview_deltas: Vec<AiStreamDelta>,
    pub(super) marker_deltas: Vec<AiStreamDelta>,
    pub(super) markers: Vec<HistoryMarker>,
    pub(super) had_live_projection: bool,
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
        }
    }

    pub(super) fn begin_model_leg(&mut self) {
        self.state.begin_model_leg();
        debug_assert!(
            self.early_thinking.is_empty() && self.live_platform_carriers.is_empty(),
            "the previous Model Leg must consume its live Client Projection"
        );
        self.leg_started_post_text = self.state.post_text_started();
    }

    pub(super) fn post_text_started(&self) -> bool {
        self.state.post_text_started()
    }

    pub(super) fn project_live_delta(
        &mut self,
        output_index: usize,
        delta: AiStreamDelta,
    ) -> Vec<AiStreamDelta> {
        self.state.project_delta(output_index, delta)
    }

    pub(super) fn begin_protected_thinking(&mut self, output_index: usize) {
        self.state.begin_protected_thinking(output_index);
    }

    pub(super) fn project_protected_delta(
        &mut self,
        output_index: usize,
        delta: AiStreamDelta,
    ) -> Vec<AiStreamDelta> {
        self.state.project_protected_delta(output_index, delta)
    }

    pub(super) fn reserved_thinking_marker(&self, output_index: usize) -> Option<&HistoryMarker> {
        self.state.reserved_thinking_marker(output_index)
    }

    pub(super) fn synthetic_thinking_item(&self, output_index: usize) -> Option<AiItem> {
        self.state.synthetic_thinking_item(output_index)
    }

    pub(super) async fn close_thinking(
        &mut self,
        output_index: usize,
        item: &AiItem,
    ) -> Result<ClosedThinking, HistoryMarkerError> {
        if self.early_thinking.contains_key(&output_index) {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        let post_text = self.state.post_text_started();
        let reserved = self.reserved_thinking_marker(output_index).cloned();
        let had_live_projection = reserved.is_some();
        let preview_started = self.state.thinking_preview_started(output_index);
        let markers = self
            .persist_thinking_blocks(item, reserved.as_ref(), post_text)
            .await?;
        let finish_deltas = self.state.close_thinking_preview(output_index);
        let mut preview_deltas = Vec::new();
        let mut marker_deltas = Vec::with_capacity(markers.len());
        if let MessageContent::Blocks(blocks) = &item.content {
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

    pub(super) fn project_platform_marker_delta(
        &mut self,
        reference: &str,
        rendered: String,
    ) -> AiStreamDelta {
        let post_text = self.state.post_text_started();
        self.live_platform_carriers
            .insert(reference.to_owned(), post_text);
        self.state.marker_delta(rendered)
    }

    pub(super) async fn publish(&self, references: &[String]) -> Result<(), HistoryMarkerError> {
        self.marker_store
            .publish(&self.principal, references, PUBLISHED_MARKER_RETENTION)
            .await
    }

    pub(super) async fn project_staged(
        &mut self,
        response: &mut AiResponse,
        platform: &[(&str, &HistoryMarker)],
    ) -> Result<Vec<String>, HistoryMarkerError> {
        let by_call_id = platform
            .iter()
            .copied()
            .collect::<HashMap<&str, &HistoryMarker>>();
        let mut post_text = self.leg_started_post_text;
        let mut projected = Vec::with_capacity(response.items.len() + platform.len());
        let mut new_references = Vec::new();

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
                        let marker = if let Some(entry) = prepared.pop_front() {
                            entry.marker
                        } else {
                            let marker = self.persist_thinking_block(block.clone(), None).await?;
                            new_references.push(marker.reference.clone());
                            marker
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
                    }
                }
            }

            if let Some(calls) = item.tool_calls.take() {
                for call in calls {
                    let mut call_item = if let Some(marker) = by_call_id.get(call.id.as_str()) {
                        let marker_post_text = self
                            .live_platform_carriers
                            .remove(&marker.reference)
                            .unwrap_or(post_text);
                        if marker_post_text != post_text {
                            return Err(HistoryMarkerError::InvalidPayload);
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
        Ok(new_references)
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
pub(super) async fn test_projection_session() -> ClientProjectionSession {
    projection_session_fixture("projection-gate-owner").await.0
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

    #[tokio::test]
    async fn one_session_projects_live_and_staged_with_the_same_marker() {
        let (mut session, store, principal) = projection_session_fixture("projection-owner").await;
        session.begin_model_leg();
        let mut live = session.project_live_delta(0, AiStreamDelta::TextDelta("answer".into()));
        live.extend(session.project_live_delta(1, AiStreamDelta::ThinkingDelta("reason".into())));
        let reserved = session
            .reserved_thinking_marker(1)
            .expect("reserved Thinking Marker")
            .reference
            .clone();
        assert!(
            store
                .resolve(&principal, &reserved)
                .await
                .expect("resolve reserved marker")
                .is_none()
        );

        let closed = session
            .close_thinking(1, &AiItem::thinking("reason", None))
            .await
            .expect("close Thinking");
        live.extend(closed.finish_deltas.clone());
        live.extend(closed.marker_deltas.clone());
        let references = closed
            .markers
            .iter()
            .map(|marker| marker.reference.clone())
            .collect::<Vec<_>>();
        assert_eq!(references, vec![reserved.clone()]);
        let persisted = store
            .resolve(&principal, &reserved)
            .await
            .expect("resolve persisted marker")
            .expect("persisted marker");
        assert!(!persisted.published);
        session.publish(&references).await.expect("publish marker");

        let mut staged = AiResponse::new("response", "model");
        staged.items = vec![
            AiItem::output_text("answer"),
            AiItem::thinking("reason", None),
        ];
        assert!(
            session
                .project_staged(&mut staged, &[])
                .await
                .expect("project staged response")
                .is_empty(),
            "staged projection must consume the live Marker"
        );
        let staged_text = staged
            .items
            .iter()
            .filter_map(AiItem::output_text_ref)
            .collect::<String>();

        assert_eq!(text_of(&live), staged_text);
        assert_eq!(
            crate::history_marker::history_marker_references(&staged.items),
            vec![reserved.clone()]
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
    async fn disconnect_during_preview_abandons_the_reserved_marker() {
        let (mut session, store, principal) = projection_session_fixture("disconnect-owner").await;
        session.begin_model_leg();
        session.project_live_delta(0, AiStreamDelta::TextDelta("answer".into()));
        let preview = session.project_live_delta(1, AiStreamDelta::ThinkingDelta("reason".into()));
        let reference = session
            .reserved_thinking_marker(1)
            .expect("reserved Thinking Marker")
            .reference
            .clone();

        assert!(
            matches!(preview.as_slice(), [AiStreamDelta::TextDelta(text)] if
            text.contains(PROJECTION_DELIMITER_PREFIX) && text.contains("> reason"))
        );
        drop(session);
        assert!(
            store
                .resolve(&principal, &reference)
                .await
                .expect("resolve abandoned marker")
                .is_none()
        );
    }

    #[tokio::test]
    async fn persist_failure_abandons_preview_without_retyping_it_as_canonical_text() {
        let (mut session, store, principal) =
            projection_session_fixture("persist-failure-owner").await;
        session.begin_model_leg();
        session.project_live_delta(0, AiStreamDelta::TextDelta("answer".into()));
        let preview = session.project_live_delta(1, AiStreamDelta::ThinkingDelta("reason".into()));
        let reference = session
            .reserved_thinking_marker(1)
            .expect("reserved Thinking Marker")
            .reference
            .clone();

        assert!(matches!(
            session
                .close_thinking(1, &AiItem::thinking(String::new(), None))
                .await,
            Err(HistoryMarkerError::InvalidPayload)
        ));
        assert!(
            matches!(preview.as_slice(), [AiStreamDelta::TextDelta(text)] if
            text.contains(PROJECTION_DELIMITER_PREFIX) && text != "reason")
        );
        assert!(
            store
                .resolve(&principal, &reference)
                .await
                .expect("resolve failed marker")
                .is_none()
        );
    }

    #[tokio::test]
    async fn publish_failure_leaves_the_persisted_marker_unpublished() {
        let (mut session, store, principal) =
            projection_session_fixture("publish-failure-owner").await;
        session.begin_model_leg();
        session.project_live_delta(0, AiStreamDelta::TextDelta("answer".into()));
        session.project_live_delta(1, AiStreamDelta::ThinkingDelta("reason".into()));
        let closed = session
            .close_thinking(1, &AiItem::thinking("reason", None))
            .await
            .expect("persist Thinking Marker");
        let reference = closed.markers[0].reference.clone();

        let error = session
            .publish(&[reference.clone(), "hm_0123456789missingref".to_owned()])
            .await
            .expect_err("missing sibling reference must roll publication back");
        assert!(matches!(error, HistoryMarkerError::Storage(_)));
        assert!(
            closed.finish_deltas.iter().all(|delta| {
                !matches!(delta, AiStreamDelta::TextDelta(text) if text == "reason")
            })
        );
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
        session.begin_model_leg();
        let mut first_leg = AiResponse::new("first", "model");
        first_leg.items = vec![AiItem::output_text("visible answer")];
        assert!(
            session
                .project_staged(&mut first_leg, &[])
                .await
                .expect("project first leg")
                .is_empty()
        );

        session.begin_model_leg();
        let mut hidden_leg = AiResponse::new("hidden", "model");
        hidden_leg.items = vec![AiItem::thinking("later reasoning", None)];
        let references = session
            .project_staged(&mut hidden_leg, &[])
            .await
            .expect("project hidden leg");

        assert_eq!(references.len(), 1);
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

    #[test]
    fn post_text_thinking_streams_as_quoted_content_with_stable_marker() {
        let mut projector = ProjectionState::default();
        projector.project_delta(0, AiStreamDelta::TextDelta("C1".into()));
        let first = projector.project_delta(1, AiStreamDelta::ThinkingDelta("R1\n".into()));
        let second = projector.project_delta(1, AiStreamDelta::ThinkingDelta("\nR2".into()));
        let reference = projector
            .reserved_thinking_marker(1)
            .expect("reserved marker")
            .reference
            .clone();
        let end = projector.close_thinking_preview(1);
        let rendered = format!("{}{}{}", text_of(&first), text_of(&second), text_of(&end));

        assert!(matches!(first.as_slice(), [AiStreamDelta::TextDelta(_)]));
        assert!(rendered.starts_with(&format!(
            "{PROJECTION_DELIMITER_PREFIX}{reference}:preview:0:start -->\n> R1"
        )));
        assert!(rendered.contains("\n> \n> R2"), "{rendered}");
        assert!(rendered.ends_with(&format!(
            "\n{PROJECTION_DELIMITER_PREFIX}{reference}:preview:0:end -->"
        )));
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
        session.begin_model_leg();
        let platform = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker_delta(
                &platform.reference,
                render_history_marker(&platform)
            ),
            AiStreamDelta::ThinkingDelta(_)
        ));
        session.project_live_delta(0, AiStreamDelta::TextDelta("C1".into()));
        let second = marker("hm_0123456789abcdefghi2", HistoryMarkerKind::Platform);
        assert!(matches!(
            session
                .project_platform_marker_delta(&second.reference, render_history_marker(&second)),
            AiStreamDelta::TextDelta(_)
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
        session.begin_model_leg();
        let third = marker("hm_0123456789abcdefghi3", HistoryMarkerKind::Platform);
        assert!(matches!(
            session.project_platform_marker_delta(&third.reference, render_history_marker(&third)),
            AiStreamDelta::TextDelta(_)
        ));
    }

    #[tokio::test]
    async fn only_non_empty_text_starts_post_text_state() {
        let (mut session, _, _) = projection_session_fixture("text-state-owner").await;
        session.begin_model_leg();
        session.project_live_delta(0, AiStreamDelta::TextDelta(String::new()));
        assert!(!session.post_text_started());

        session.project_live_delta(0, AiStreamDelta::TextDelta(" ".into()));
        assert!(session.post_text_started());
    }

    #[tokio::test]
    async fn protected_post_text_blocks_expose_only_public_preview_bytes() {
        let (mut session, _, _) = projection_session_fixture("protected-owner").await;
        session.begin_model_leg();
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

        let references = session
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
        assert_eq!(references.len(), 3);
        assert_eq!(
            crate::history_marker::history_marker_references(&response.items).len(),
            3
        );
    }

    #[tokio::test]
    async fn each_post_text_thinking_block_reserves_a_distinct_marker() {
        let (mut session, _, _) = projection_session_fixture("block-owner").await;
        session.begin_model_leg();
        session.project_live_delta(0, AiStreamDelta::TextDelta("C1".into()));
        session.project_live_delta(1, AiStreamDelta::ThinkingDelta("R1".into()));
        let first = session
            .reserved_thinking_marker(1)
            .expect("first marker")
            .reference
            .clone();
        session
            .close_thinking(1, &AiItem::thinking("R1", None))
            .await
            .expect("close first block");
        let mut first_response = AiResponse::new("first", "model");
        first_response.items = vec![AiItem::output_text("C1"), AiItem::thinking("R1", None)];
        session
            .project_staged(&mut first_response, &[])
            .await
            .expect("consume first live projection");

        session.project_live_delta(2, AiStreamDelta::ThinkingDelta("R2".into()));
        let second = session
            .reserved_thinking_marker(2)
            .expect("second marker")
            .reference
            .clone();

        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn platform_projection_never_retypes_text() {
        let platform = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Platform);
        let (mut session, _, _) = projection_session_fixture("platform-staged-owner").await;
        session.begin_model_leg();
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

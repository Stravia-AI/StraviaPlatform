//! Client Projection for one Model Leg: the canonical response becomes the
//! client-visible view. Platform Tool call/result pairs and protected Thinking
//! become History Markers, and canonical Text in a leg containing Platform
//! Tool calls is carried by Thinking wrapped in Projection Delimiters.
//!
//! One stateful projector serves both delivery paths so their projections
//! cannot drift: `project_delta` streams live deltas (span boundaries follow
//! the delta carrier), `project_items` projects the completed leg in one shot.
//! Marker creation, claiming, publishing, and live buffering stay outside this
//! module.

use std::collections::{HashMap, HashSet};

use crate::history_marker::{
    HistoryMarker, render_history_marker, render_preview_projection_span,
    render_text_projection_end, render_text_projection_span, render_text_projection_start,
};
use crate::protocol::ir::{AiItem, AiStreamDelta, ContentBlock, MessageContent, Role};

/// The delta carrier a projected Text span is delivered on.
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

/// Client Projection state for one Model Leg.
#[derive(Default)]
pub(super) struct ClientProjector {
    /// Every Text span in the leg binds the first Platform Marker reference.
    reference: Option<String>,
    /// Count of already-closed Text spans; a span's ordinal is assigned when
    /// it closes (live) or is staged.
    span_ordinal: usize,
    /// Live path: the currently open span's carrier.
    span_carrier: Option<ProjectionSpanCarrier>,
    /// Live path: indexed items whose Text deltas were already projected, so
    /// their completion must not re-emit the superseded item.
    projected_text_items: HashSet<usize>,
}

impl ClientProjector {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Live path: register a delivered Platform Marker. The first one owns
    /// every Text span of the leg.
    pub(super) fn note_platform_reference(&mut self, reference: &str) {
        if self.reference.is_none() {
            self.reference = Some(reference.to_owned());
        }
    }

    pub(super) fn contains_platform(&self) -> bool {
        self.reference.is_some()
    }

    /// Reset per-leg state. A completed leg must have closed its open span.
    pub(super) fn begin_model_leg(&mut self) {
        debug_assert!(
            self.span_carrier.is_none(),
            "a completed Model Leg must close its projected Text span"
        );
        self.reference = None;
        self.span_ordinal = 0;
        self.span_carrier = None;
        self.projected_text_items.clear();
    }

    fn reference(&self) -> &str {
        self.reference
            .as_deref()
            .expect("Platform projection requires a History Marker")
    }

    // ── Live projection ─────────────────────────────────────────────────────

    /// Project one stream delta. Text deltas open or continue a delimited
    /// Thinking span on their carrier; any other delta closes the open span;
    /// the completion of an item whose Text was projected is consumed.
    pub(super) fn project_delta(&mut self, delta: AiStreamDelta) -> Vec<AiStreamDelta> {
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
                let unindexed_text_item = self.span_carrier
                    == Some(ProjectionSpanCarrier::Unindexed)
                    && item.role == Role::Assistant
                    && item.tool_calls.is_none()
                    && item.reasoning_ref().is_none()
                    && item.thinking_ref().is_none()
                    && item.unknown_ref().is_none();
                let projected_text_item =
                    self.projected_text_items.remove(&index) || unindexed_text_item;
                let mut projected = self.close_span();
                if !projected_text_item {
                    projected.push(AiStreamDelta::ItemDone { index, item });
                }
                projected
            }
            other => {
                let mut projected = self.close_span();
                projected.push(other);
                projected
            }
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
            self.projected_text_items.insert(index);
        }
        let mut projected = Vec::new();
        if self.span_carrier != Some(carrier) {
            projected.extend(self.close_span());
            text = format!(
                "{}{text}",
                render_text_projection_start(self.reference(), self.span_ordinal)
            );
            self.span_carrier = Some(carrier);
        }
        projected.push(carrier.delta(text, obfuscation));
        projected
    }

    /// Close the open span, if any, assigning its ordinal.
    pub(super) fn close_span(&mut self) -> Vec<AiStreamDelta> {
        let Some(carrier) = self.span_carrier.take() else {
            return Vec::new();
        };
        let ordinal = self.span_ordinal;
        self.span_ordinal += 1;
        vec![carrier.delta(render_text_projection_end(self.reference(), ordinal), None)]
    }

    /// Visible preview deltas for a completed protected Thinking block. The
    /// parts are rendered by the same in-place projection the staged path
    /// uses, then wrapped in their delta carriers.
    pub(super) fn preview_deltas(
        &self,
        output_index: usize,
        block: &ContentBlock,
        marker: &HistoryMarker,
    ) -> Vec<AiStreamDelta> {
        if matches!(block, ContentBlock::RedactedThinking { .. }) {
            return Vec::new();
        }
        let mut projected = block.clone();
        self.render_preview(&mut projected, marker);
        match &projected {
            ContentBlock::Thinking { thinking, .. } => {
                vec![AiStreamDelta::ThinkingDelta(thinking.clone())]
            }
            ContentBlock::Reasoning {
                summary, content, ..
            } => summary
                .iter()
                .enumerate()
                .map(|(ordinal, text)| AiStreamDelta::ReasoningSummaryDelta {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(output_index),
                    content_index: Some(ordinal),
                })
                .chain(content.iter().enumerate().map(|(ordinal, text)| {
                    AiStreamDelta::ThinkingDeltaWithMetadata {
                        text: text.clone(),
                        obfuscation: None,
                        output_index: Some(output_index),
                        content_index: Some(ordinal),
                    }
                }))
                .collect(),
            other => {
                unreachable!("protected Thinking preview remains a reasoning block: {other:?}")
            }
        }
    }

    // ── Staged projection ───────────────────────────────────────────────────

    /// Project the completed leg's items in one shot. `platform` maps Platform
    /// Tool call ids to their markers; its first marker owns the leg's Text
    /// spans. Platform outputs are dropped, Platform calls become marker
    /// items, and every Text part becomes a delimited Thinking span.
    pub(super) fn project_items(
        &mut self,
        items: Vec<AiItem>,
        platform: &[(&str, &HistoryMarker)],
    ) -> Vec<AiItem> {
        if platform.is_empty() {
            return items;
        }
        if let Some((_, marker)) = platform.first() {
            self.note_platform_reference(&marker.reference);
        }
        let by_call_id = platform
            .iter()
            .copied()
            .collect::<HashMap<&str, &HistoryMarker>>();
        let mut projected = Vec::with_capacity(items.len() + platform.len());
        for mut item in items {
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

            let calls = item.tool_calls.take().unwrap_or_default();
            let had_calls = !calls.is_empty();
            let content = std::mem::replace(&mut item.content, MessageContent::Text(String::new()));
            let mut meta = item.meta.take();
            let mut emitted_content = false;
            match content {
                MessageContent::Text(text) if !text.is_empty() => {
                    push_projected_content(
                        &mut projected,
                        MessageContent::Blocks(vec![ContentBlock::Thinking {
                            thinking: self.text_span(&text),
                            signature: None,
                        }]),
                        &mut meta,
                    );
                    emitted_content = true;
                }
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        let projected_block = match block {
                            ContentBlock::Text { text, .. } => ContentBlock::Thinking {
                                thinking: self.text_span(&text),
                                signature: None,
                            },
                            other => other,
                        };
                        push_projected_content(
                            &mut projected,
                            MessageContent::Blocks(vec![projected_block]),
                            &mut meta,
                        );
                        emitted_content = true;
                    }
                }
                MessageContent::Text(_) => {}
            }
            for call in calls {
                if let Some(marker) = by_call_id.get(call.id.as_str()) {
                    projected.push(Self::marker_item(marker));
                } else {
                    projected.push(AiItem {
                        role: Role::Assistant,
                        content: MessageContent::Text(String::new()),
                        tool_calls: Some(vec![call]),
                        tool_call_id: None,
                        meta: meta.take(),
                    });
                }
            }
            if !emitted_content && !had_calls && meta.is_some() {
                item.meta = meta;
                projected.push(item);
            }
        }
        projected
    }

    /// One staged Text span: the full delimiter pair around the visible bytes.
    fn text_span(&mut self, visible: &str) -> String {
        let rendered = render_text_projection_span(self.reference(), self.span_ordinal, visible);
        self.span_ordinal += 1;
        rendered
    }

    /// The client-visible form of a completed protected block: protection is
    /// removed and visible bytes become preview spans. Redacted Thinking is
    /// hidden entirely. Only call for protected blocks.
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
        self.render_preview(&mut visible, marker);
        Some(visible)
    }

    /// The single preview rendering: protection-stripped visible bytes wrapped
    /// in preview spans. Preview span ordinals are block-local — summary parts
    /// enumerate from zero, content parts continue after them; a Thinking
    /// block is a single ordinal-zero part.
    fn render_preview(&self, block: &mut ContentBlock, marker: &HistoryMarker) {
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

    /// A delivered History Marker as its own unindexed Thinking item.
    pub(super) fn marker_item(marker: &HistoryMarker) -> AiItem {
        AiItem::thinking(render_history_marker(marker), None)
    }
}

/// Protected reasoning the client must not receive in the clear: signed
/// Thinking, encrypted Reasoning, or redacted Thinking.
pub(super) fn is_protected_thinking(block: &ContentBlock) -> bool {
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

fn push_projected_content(
    projected: &mut Vec<AiItem>,
    content: MessageContent,
    meta: &mut Option<serde_json::Value>,
) {
    projected.push(AiItem {
        role: Role::Assistant,
        content,
        tool_calls: None,
        tool_call_id: None,
        meta: meta.take(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_marker::{HistoryMarkerKind, PROJECTION_DELIMITER_PREFIX};

    fn platform_marker(reference: &str) -> HistoryMarker {
        HistoryMarker {
            reference: reference.into(),
            kind: HistoryMarkerKind::Platform,
            activity: "Running a platform tool".into(),
        }
    }

    fn text_delta(text: &str, output_index: usize, content_index: usize) -> AiStreamDelta {
        AiStreamDelta::TextDeltaWithMetadata {
            text: text.into(),
            logprobs: Vec::new(),
            obfuscation: None,
            output_index: Some(output_index),
            content_index: Some(content_index),
        }
    }

    fn delta_texts(deltas: &[AiStreamDelta]) -> String {
        deltas
            .iter()
            .map(|delta| match delta {
                AiStreamDelta::ThinkingDelta(text) => text.as_str(),
                AiStreamDelta::ThinkingDeltaWithMetadata { text, .. } => text.as_str(),
                AiStreamDelta::ReasoningSummaryDelta { text, .. } => text.as_str(),
                other => panic!("unexpected projected delta: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn delta_projection_matches_staged_items() {
        let item = AiItem {
            role: Role::Assistant,
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
        };

        let mut live = ClientProjector::new();
        live.note_platform_reference("hm_0123456789abcdefghij");
        let mut deltas = Vec::new();
        deltas.extend(live.project_delta(text_delta("first", 3, 0)));
        deltas.extend(live.project_delta(text_delta("second", 3, 1)));
        deltas.extend(live.project_delta(AiStreamDelta::ItemDone {
            index: 3,
            item: item.clone(),
        }));
        deltas.extend(live.close_span());
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::ItemDone { .. })),
            "the projected Text item's completion must be consumed"
        );

        let mut staged = ClientProjector::new();
        let staged_items = staged.project_items(
            vec![item],
            &[("call-1", &platform_marker("hm_0123456789abcdefghij"))],
        );

        // Staged projection frames each Text block as its own Thinking item;
        // the identity both paths share is the delivered byte sequence.
        let staged_text = staged_items
            .iter()
            .flat_map(|item| match &item.content {
                MessageContent::Blocks(blocks) => blocks.iter().map(|block| match block {
                    ContentBlock::Thinking { thinking, .. } => thinking.as_str(),
                    other => panic!("unexpected staged block: {other:?}"),
                }),
                other => panic!("projected content is block-shaped: {other:?}"),
            })
            .collect::<String>();

        // Live flushes the same delimited bytes the staged projection wraps
        // per block: start, visible, end — with identical span ordinals.
        let live_text = delta_texts(&deltas);
        assert_eq!(live_text, staged_text);
        assert!(
            staged_text.contains(":text:0:start -->first"),
            "{staged_text}"
        );
        assert!(staged_text.contains(":text:1:end -->"), "{staged_text}");
    }

    #[test]
    fn first_platform_reference_owns_all_text_spans() {
        let mut projector = ClientProjector::new();
        projector.note_platform_reference("hm_first00000000000000");
        projector.note_platform_reference("hm_second0000000000000");

        let projected = projector.project_delta(AiStreamDelta::TextDelta("visible".into()));

        assert!(delta_texts(&projected).contains("hm_first00000000000000"));
    }

    #[test]
    fn items_without_platform_markers_pass_through() {
        let mut projector = ClientProjector::new();
        let projected = projector.project_items(vec![AiItem::output_text("answer")], &[]);

        let [item] = &projected[..] else {
            panic!("no platform markers means no projection: {projected:?}")
        };
        assert_eq!(item.output_text_ref(), Some("answer"));
    }

    #[test]
    fn preview_ordinals_are_block_local() {
        let marker = HistoryMarker {
            reference: "hm_0123456789abcdefghij".into(),
            kind: HistoryMarkerKind::Thinking,
            activity: "Preserving protected reasoning".into(),
        };
        let block = ContentBlock::Reasoning {
            summary: vec!["summary".into()],
            content: vec!["content".into()],
            encrypted_content: Some("opaque".into()),
        };

        let projector = ClientProjector::new();
        let preview = projector.preview_deltas(2, &block, &marker);

        assert!(matches!(
            preview.as_slice(),
            [
                AiStreamDelta::ReasoningSummaryDelta {
                    output_index: Some(2),
                    content_index: Some(0),
                    ..
                },
                AiStreamDelta::ThinkingDeltaWithMetadata {
                    output_index: Some(2),
                    content_index: Some(0),
                    ..
                },
            ]
        ));
        let texts = delta_texts(&preview);
        assert!(texts.contains(":preview:0:"), "{texts}");
        assert!(texts.contains(":preview:1:"), "{texts}");
    }

    #[test]
    fn empty_protected_thinking_keeps_its_preview_span() {
        let marker = HistoryMarker {
            reference: "hm_0123456789abcdefghij".into(),
            kind: HistoryMarkerKind::Thinking,
            activity: "Preserving protected reasoning".into(),
        };
        let empty = ContentBlock::Thinking {
            thinking: String::new(),
            signature: Some("opaque".into()),
        };

        let projector = ClientProjector::new();
        let live_text = delta_texts(&projector.preview_deltas(0, &empty, &marker));
        let staged = projector
            .visible_protected_block(&empty, &marker)
            .expect("empty signed Thinking keeps a visible preview");

        match staged {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(signature, None);
                assert_eq!(live_text, thinking);
            }
            other => panic!("unexpected visible block: {other:?}"),
        }
        assert!(live_text.contains(":preview:0:start"), "{live_text}");
    }

    #[test]
    fn visible_protected_block_strips_protection_and_hides_redacted() {
        let marker = HistoryMarker {
            reference: "hm_0123456789abcdefghij".into(),
            kind: HistoryMarkerKind::Thinking,
            activity: "Preserving protected reasoning".into(),
        };
        let signed = ContentBlock::Thinking {
            thinking: "hidden".into(),
            signature: Some("opaque".into()),
        };

        let projector = ClientProjector::new();
        let visible = projector
            .visible_protected_block(&signed, &marker)
            .expect("signed Thinking keeps a visible preview");

        match visible {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(signature, None);
                assert!(thinking.contains(&format!(
                    "{PROJECTION_DELIMITER_PREFIX}hm_0123456789abcdefghij:preview:0:start"
                )));
                assert!(thinking.contains("hidden"));
            }
            other => panic!("unexpected visible block: {other:?}"),
        }

        assert!(
            projector
                .visible_protected_block(
                    &ContentBlock::RedactedThinking {
                        data: "opaque".into(),
                    },
                    &marker,
                )
                .is_none(),
            "redacted Thinking is hidden entirely"
        );
    }
}

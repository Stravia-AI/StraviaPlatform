//! Client Projection for one Inference Run.
//!
//! Canonical Text is delivered unchanged. OpenAI-compatible clients keep
//! Thinking on the reasoning carrier until the first non-empty Text, then use
//! quoted `content` previews bound to authoritative Thinking History Markers.
//! Other protocols retain their native carriers.

use std::collections::HashMap;

use crate::history_marker::{
    HISTORY_MARKER_PREFIX, HistoryMarker, PROJECTION_DELIMITER_PREFIX, render_history_marker,
    render_preview_projection_end, render_preview_projection_span, render_preview_projection_start,
};
use crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use crate::protocol::ir::{AiItem, AiStreamDelta, ContentBlock, MessageContent, Role};

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

/// Run-wide Client Projection state. `begin_model_leg` deliberately does not
/// reset `post_text_started`.
pub(super) struct ClientProjector {
    openai_compatible: bool,
    post_text_started: bool,
    live_previews: HashMap<usize, LiveThinkingPreview>,
}

impl Default for ClientProjector {
    fn default() -> Self {
        Self {
            openai_compatible: true,
            post_text_started: false,
            live_previews: HashMap::new(),
        }
    }
}

impl ClientProjector {
    pub(super) fn for_ingress(ingress: crate::protocol::ids::ProtocolId) -> Self {
        Self {
            openai_compatible: ingress == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ..Self::default()
        }
    }

    pub(super) fn staged(openai_compatible: bool, post_text_started: bool) -> Self {
        Self {
            openai_compatible,
            post_text_started,
            live_previews: HashMap::new(),
        }
    }

    pub(super) fn begin_model_leg(&mut self) {
        debug_assert!(
            self.live_previews.is_empty(),
            "a completed Model Leg must finalize every Thinking Marker"
        );
    }

    pub(super) fn post_text_started(&self) -> bool {
        self.post_text_started
    }

    pub(super) fn observe_text(&mut self, text: &str) {
        if self.openai_compatible && !text.is_empty() {
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
    }

    pub(super) fn synthetic_thinking_item(&self, output_index: usize) -> Option<AiItem> {
        self.live_previews
            .get(&output_index)
            .map(|preview| AiItem::thinking(preview.canonical_text.clone(), None))
    }

    pub(super) fn close_thinking_preview(&mut self, output_index: usize) -> Vec<AiStreamDelta> {
        let Some(preview) = self.live_previews.remove(&output_index) else {
            return Vec::new();
        };
        vec![preview.carrier.text_delta(preview.encoder.finish())]
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

    pub(super) fn marker_item(&self, marker: &HistoryMarker) -> AiItem {
        let rendered = render_history_marker(marker);
        if self.openai_compatible && self.post_text_started {
            AiItem::output_text(rendered)
        } else {
            AiItem::thinking(rendered, None)
        }
    }

    /// Replace Platform calls with markers without retyping canonical Text.
    pub(super) fn project_items(
        &mut self,
        items: Vec<AiItem>,
        platform: &[(&str, &HistoryMarker)],
    ) -> Vec<AiItem> {
        if platform.is_empty() {
            for item in &items {
                self.observe_item_text(item);
            }
            return items;
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
            if item.role == Role::Assistant {
                self.observe_item_text(&item);
            }
            let calls = item.tool_calls.take().unwrap_or_default();
            let keep_item = !message_content_is_empty(&item.content)
                || item.meta.is_some()
                || (calls.is_empty() && item.tool_call_id.is_some());
            if keep_item {
                projected.push(item);
            }
            for call in calls {
                if let Some(marker) = by_call_id.get(call.id.as_str()) {
                    projected.push(self.marker_item(marker));
                } else {
                    projected.push(AiItem::function_call(call));
                }
            }
        }
        projected
    }

    fn observe_item_text(&mut self, item: &AiItem) {
        match &item.content {
            MessageContent::Text(text) => self.observe_text(text),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if let ContentBlock::Text { text, .. } = block {
                        self.observe_text(text);
                    }
                }
            }
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

pub(super) fn is_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::RedactedThinking { .. }
    )
}

/// Protected reasoning the client must not receive in its authoritative form.
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

fn message_content_is_empty(content: &MessageContent) -> bool {
    match content {
        MessageContent::Text(text) => text.is_empty(),
        MessageContent::Blocks(blocks) => blocks.is_empty(),
    }
}

fn escape_private_syntax(text: &str) -> String {
    text.replace(HISTORY_MARKER_PREFIX, "&lt;!-- stravia-history-marker:")
        .replace(PROJECTION_DELIMITER_PREFIX, "&lt;!-- stravia-projection:")
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

    #[test]
    fn post_text_thinking_streams_as_quoted_content_with_stable_marker() {
        let mut projector = ClientProjector::default();
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

    #[test]
    fn platform_markers_follow_run_wide_post_text_carrier() {
        let mut projector = ClientProjector::default();
        let platform = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Platform);
        assert!(matches!(
            projector.marker_delta(render_history_marker(&platform)),
            AiStreamDelta::ThinkingDelta(_)
        ));
        projector.observe_text("C1");
        assert!(matches!(
            projector.marker_delta(render_history_marker(&platform)),
            AiStreamDelta::TextDelta(_)
        ));
        projector.begin_model_leg();
        assert!(matches!(
            projector.marker_delta(render_history_marker(&platform)),
            AiStreamDelta::TextDelta(_)
        ));
    }

    #[test]
    fn only_non_empty_text_starts_post_text_state() {
        let marker = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Thinking);
        let mut projector = ClientProjector::default();
        projector.project_delta(0, AiStreamDelta::TextDelta(String::new()));
        assert!(matches!(
            projector.marker_delta(render_history_marker(&marker)),
            AiStreamDelta::ThinkingDelta(_)
        ));

        projector.project_delta(0, AiStreamDelta::TextDelta(" ".into()));
        assert!(matches!(
            projector.marker_delta(render_history_marker(&marker)),
            AiStreamDelta::TextDelta(_)
        ));
    }

    #[test]
    fn protected_post_text_blocks_expose_only_public_preview_bytes() {
        let marker = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Thinking);
        let projector = ClientProjector::staged(true, true);
        let preview = projector
            .post_text_preview(
                &ContentBlock::Reasoning {
                    summary: vec!["public summary".into()],
                    content: Vec::new(),
                    encrypted_content: Some("opaque-encrypted-payload".into()),
                },
                &marker,
            )
            .expect("public summary Preview");
        let text = preview.output_text_ref().expect("content Preview");
        assert!(text.contains("> public summary"), "{text}");
        assert!(!text.contains("opaque-encrypted-payload"), "{text}");

        assert!(
            projector
                .post_text_preview(
                    &ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: Some("opaque-signature".into()),
                    },
                    &marker,
                )
                .is_none()
        );
        assert!(
            projector
                .post_text_preview(
                    &ContentBlock::Reasoning {
                        summary: Vec::new(),
                        content: Vec::new(),
                        encrypted_content: Some("opaque-encrypted-payload".into()),
                    },
                    &marker,
                )
                .is_none()
        );
        assert!(
            projector
                .post_text_preview(
                    &ContentBlock::RedactedThinking {
                        data: "opaque-redacted-payload".into(),
                    },
                    &marker,
                )
                .is_none()
        );
        assert!(
            projector
                .marker_item(&marker)
                .output_text_ref()
                .is_some_and(|text| text.contains(HISTORY_MARKER_PREFIX))
        );
    }

    #[test]
    fn each_post_text_thinking_block_reserves_a_distinct_marker() {
        let mut projector = ClientProjector::default();
        projector.observe_text("C1");
        projector.project_delta(1, AiStreamDelta::ThinkingDelta("R1".into()));
        let first = projector
            .reserved_thinking_marker(1)
            .expect("first marker")
            .reference
            .clone();
        projector.close_thinking_preview(1);

        projector.project_delta(2, AiStreamDelta::ThinkingDelta("R2".into()));
        let second = projector
            .reserved_thinking_marker(2)
            .expect("second marker")
            .reference
            .clone();

        assert_ne!(first, second);
    }

    #[test]
    fn platform_projection_never_retypes_text() {
        let platform = marker("hm_0123456789abcdefghij", HistoryMarkerKind::Platform);
        let mut projector = ClientProjector::default();
        let projected = projector.project_items(
            vec![
                AiItem::output_text("C1"),
                AiItem::function_call(crate::protocol::ir::ToolCall {
                    id: "call-1".into(),
                    name: "web_search".into(),
                    arguments: "{}".into(),
                }),
            ],
            &[("call-1", &platform)],
        );

        assert_eq!(projected[0].output_text_ref(), Some("C1"));
        assert!(projected[0].thinking_ref().is_none());
        assert!(
            projected[1]
                .output_text_ref()
                .is_some_and(|text| text.contains(HISTORY_MARKER_PREFIX))
        );
    }
}

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnindexedItemKind {
    Text,
    Thinking,
    Tool,
}

#[derive(Default)]
pub(super) struct LiveDeltaGate {
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
    projector: ClientProjector,
    client_output_started: bool,
    response_started: bool,
}

impl LiveDeltaGate {
    pub(super) fn buffers_unindexed_protected(&self) -> bool {
        self.buffer_unindexed_protected
    }

    pub(super) fn begin_model_leg(&mut self, egress: crate::protocol::ids::Protocol) {
        debug_assert!(
            self.pending_suffix.is_empty(),
            "a completed Model Leg must resolve its ambiguous suffix"
        );
        self.projector.begin_model_leg();
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

    pub(super) fn capture_unindexed_signatures(&mut self, deltas: &mut Vec<AiStreamDelta>) {
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

    pub(super) fn synthetic_signed_thinking_item(&self) -> Option<(usize, AiItem)> {
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

    pub(super) fn capture_protected_candidates(&mut self, deltas: &[AiStreamDelta]) {
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

    pub(super) fn resolve_completed_item(
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
                if !is_protected_thinking(block) {
                    continue;
                }
                let marker = markers
                    .get(marker_index)
                    .expect("protected Thinking block has a History Marker");
                marker_index += 1;
                preview.extend(self.projector.preview_deltas(index, block, marker));
            }
        }
        preview
    }

    pub(super) fn commit_visible(&mut self, mut deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
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

    pub(super) fn project_platform_marker(
        &mut self,
        reference: &str,
        rendered: String,
    ) -> Vec<AiStreamDelta> {
        self.projector.note_platform_reference(reference);
        self.ambiguous_suffix = false;
        let pending = std::mem::take(&mut self.pending_suffix);
        let mut projected = pending
            .into_iter()
            .flat_map(|delta| self.projector.project_delta(delta))
            .collect::<Vec<_>>();
        projected.extend(self.projector.close_span());
        projected.push(AiStreamDelta::ThinkingDelta(rendered));
        self.commit_visible(projected)
    }

    pub(super) fn complete_model_leg(&mut self) -> Vec<AiStreamDelta> {
        let pending_thinking = self.flush_unindexed_thinking();
        let mut suffix = std::mem::take(&mut self.pending_suffix);
        suffix.extend(pending_thinking);
        self.ambiguous_suffix = false;
        if self.projector.contains_platform() {
            suffix = suffix
                .into_iter()
                .flat_map(|delta| self.projector.project_delta(delta))
                .collect();
        }
        suffix.extend(self.projector.close_span());
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

    pub(super) fn ends_unindexed_thinking(delta: &AiStreamDelta) -> bool {
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

    pub(super) fn route_visible_deltas(
        &mut self,
        has_exposed_tools: bool,
        deltas: Vec<AiStreamDelta>,
    ) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for delta in deltas {
            if self.projector.contains_platform() {
                visible.extend(self.projector.project_delta(delta));
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

    pub(super) fn filter(
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

#[cfg(test)]
mod terminal_tests {
    use super::super::partition_terminal_deltas;
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
                    [crate::protocol::ir::ContentBlock::Thinking { thinking, signature: Some(signature) }]
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

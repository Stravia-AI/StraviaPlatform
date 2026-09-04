//! Stream response accumulator: buffers streaming deltas into a complete
//! `AiResponse` for caching and formatted response aggregation.

use std::collections::BTreeMap;

use crate::protocol::ir::request::ToolCall;
use crate::protocol::ir::{AiItem, AiResponse, AiStreamDelta, ContentBlock, MessageContent, Usage};

enum AccumulatedItem {
    Text(String),
    Refusal(String),
    Thinking {
        text: String,
        signature: String,
    },
    Reasoning {
        summary: String,
        content: String,
        signature: String,
    },
    ToolCall(usize),
    Unknown(AiItem),
}

#[derive(Default)]
pub(crate) struct StreamResponseAccumulator {
    pub(crate) id: String,
    pub(crate) model: String,
    response_metadata: Option<serde_json::Value>,
    items: Vec<AccumulatedItem>,
    tool_calls: Vec<Option<ToolCall>>,
    completed_items: BTreeMap<usize, AiItem>,
    indexed_text: BTreeMap<(usize, usize), String>,
    indexed_refusal: BTreeMap<(usize, usize), String>,
    indexed_reasoning_summary: BTreeMap<(usize, usize), String>,
    indexed_reasoning_content: BTreeMap<(usize, usize), String>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) terminal: Option<(String, Option<serde_json::Value>)>,
    pub(crate) usage: Usage,
}

fn completed_item_semantic_shell(item: &AiItem) -> AiItem {
    let mut shell = item.clone();
    if shell.role != crate::protocol::ir::Role::Assistant {
        return shell;
    }
    match &mut shell.content {
        MessageContent::Text(text) => text.clear(),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => text.clear(),
                    ContentBlock::Refusal { refusal } => refusal.clear(),
                    ContentBlock::Thinking { thinking, .. } => thinking.clear(),
                    ContentBlock::Reasoning {
                        summary, content, ..
                    } => {
                        summary.clear();
                        content.clear();
                    }
                    _ => {}
                }
            }
        }
    }
    shell
}
impl StreamResponseAccumulator {
    pub(crate) fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.tool_calls.iter().filter_map(Option::as_ref)
    }

    pub(crate) fn apply_all(&mut self, deltas: &[AiStreamDelta]) {
        for delta in deltas {
            self.apply(delta);
        }
    }

    pub(crate) fn apply(&mut self, delta: &AiStreamDelta) {
        match delta {
            AiStreamDelta::MessageStart { id, model } => {
                if self.id.is_empty() {
                    self.id = id.clone();
                }
                if self.model.is_empty() {
                    self.model = model.clone();
                }
            }
            AiStreamDelta::ResponseMetadata { metadata } => {
                self.response_metadata = Some(metadata.clone());
            }
            AiStreamDelta::ProtectedThinkingStart { .. } => {}
            AiStreamDelta::ThinkingDelta(text) => match self.items.last_mut() {
                Some(AccumulatedItem::Thinking { text: current, .. }) => current.push_str(text),
                _ => self.items.push(AccumulatedItem::Thinking {
                    text: text.clone(),
                    signature: String::new(),
                }),
            },
            AiStreamDelta::ThinkingDeltaWithMetadata {
                text,
                output_index: Some(output_index),
                content_index: Some(content_index),
                ..
            } => self
                .indexed_reasoning_content
                .entry((*output_index, *content_index))
                .or_default()
                .push_str(text),
            AiStreamDelta::ThinkingDeltaWithMetadata { text, .. } => match self.items.last_mut() {
                Some(AccumulatedItem::Reasoning {
                    content: current, ..
                }) => current.push_str(text),
                _ => self.items.push(AccumulatedItem::Reasoning {
                    summary: String::new(),
                    content: text.clone(),
                    signature: String::new(),
                }),
            },
            AiStreamDelta::ThinkingSignature(signature) => {
                match self.items.iter_mut().rev().find_map(|item| match item {
                    AccumulatedItem::Thinking { signature, .. }
                    | AccumulatedItem::Reasoning { signature, .. } => Some(signature),
                    _ => None,
                }) {
                    Some(current) => current.push_str(signature),
                    None => self.items.push(AccumulatedItem::Thinking {
                        text: String::new(),
                        signature: signature.clone(),
                    }),
                }
            }
            AiStreamDelta::TextDelta(text) => self.push_text(text),
            AiStreamDelta::TextDeltaWithMetadata {
                text,
                output_index: Some(output_index),
                content_index: Some(content_index),
                ..
            } => self
                .indexed_text
                .entry((*output_index, *content_index))
                .or_default()
                .push_str(text),
            AiStreamDelta::TextDeltaWithMetadata { text, .. } => self.push_text(text),
            AiStreamDelta::ReasoningSummaryDelta {
                text,
                output_index: Some(output_index),
                content_index: Some(content_index),
                ..
            } => self
                .indexed_reasoning_summary
                .entry((*output_index, *content_index))
                .or_default()
                .push_str(text),
            AiStreamDelta::ReasoningSummaryDelta { text, .. } => match self.items.last_mut() {
                Some(AccumulatedItem::Reasoning {
                    summary: current, ..
                }) => current.push_str(text),
                _ => self.items.push(AccumulatedItem::Reasoning {
                    summary: text.clone(),
                    content: String::new(),
                    signature: String::new(),
                }),
            },
            AiStreamDelta::RefusalDelta(text) => self.push_refusal(text),
            AiStreamDelta::RefusalDeltaWithIndex {
                text,
                output_index,
                content_index,
            } => self
                .indexed_refusal
                .entry((*output_index, *content_index))
                .or_default()
                .push_str(text),
            AiStreamDelta::ToolCallStart { index, id, name } => {
                ensure_tool_index(&mut self.tool_calls, *index);
                if let Some(call) = self.tool_calls[*index].as_mut() {
                    if call.id.is_empty() && !id.is_empty() {
                        call.id = id.clone();
                    }
                    call.name.push_str(name);
                } else {
                    self.tool_calls[*index] = Some(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                    });
                }
                if !self.items.iter().any(
                    |item| matches!(item, AccumulatedItem::ToolCall(current) if current == index),
                ) {
                    self.items.push(AccumulatedItem::ToolCall(*index));
                }
            }
            AiStreamDelta::ToolCallDelta { index, arguments } => {
                ensure_tool_index(&mut self.tool_calls, *index);
                if let Some(tc) = self.tool_calls[*index].as_mut() {
                    tc.arguments.push_str(arguments);
                } else {
                    self.tool_calls[*index] = Some(ToolCall {
                        id: format!("tool-{index}"),
                        name: String::new(),
                        arguments: arguments.clone(),
                    });
                }
                if !self.items.iter().any(
                    |item| matches!(item, AccumulatedItem::ToolCall(current) if current == index),
                ) {
                    self.items.push(AccumulatedItem::ToolCall(*index));
                }
            }
            AiStreamDelta::ToolCallComplete { index, tool_call } => {
                ensure_tool_index(&mut self.tool_calls, *index);
                self.tool_calls[*index] = Some(tool_call.clone());
                if !self.items.iter().any(
                    |item| matches!(item, AccumulatedItem::ToolCall(current) if current == index),
                ) {
                    self.items.push(AccumulatedItem::ToolCall(*index));
                }
            }
            AiStreamDelta::ItemDone { index, item } => {
                self.completed_items
                    .insert(*index, completed_item_semantic_shell(item));
            }
            AiStreamDelta::Usage(usage) => self.usage = usage.clone(),
            AiStreamDelta::ResponseTerminal {
                status,
                incomplete_details,
            } => {
                self.terminal = Some((status.clone(), incomplete_details.clone()));
            }
            AiStreamDelta::Done { stop_reason } => self.stop_reason = Some(stop_reason.clone()),
            AiStreamDelta::StreamError { error } => {
                self.stop_reason = Some("error".to_string());
                tracing::warn!(error = ?error, "stream error delta received");
            }
            AiStreamDelta::UnexpectedEof => {
                if self.stop_reason.is_none() {
                    self.stop_reason = Some("error".to_string());
                }
            }
            AiStreamDelta::Unknown { raw } => {
                if let Ok(raw) = serde_json::from_str::<serde_json::Value>(raw)
                    && raw.get("__open_responses_event").is_none()
                {
                    self.items
                        .push(AccumulatedItem::Unknown(AiItem::unknown(raw)));
                }
            }
        }
    }
    fn push_text(&mut self, text: &str) {
        match self.items.last_mut() {
            Some(AccumulatedItem::Text(current)) => current.push_str(text),
            _ => self.items.push(AccumulatedItem::Text(text.to_owned())),
        }
    }

    fn push_refusal(&mut self, text: &str) {
        match self.items.last_mut() {
            Some(AccumulatedItem::Refusal(current)) => current.push_str(text),
            _ => self.items.push(AccumulatedItem::Refusal(text.to_owned())),
        }
    }

    pub(crate) fn into_ai_response(self) -> AiResponse {
        let Self {
            id,
            model,
            response_metadata,
            items,
            tool_calls,
            completed_items,
            indexed_text,
            stop_reason,
            terminal,
            indexed_refusal,
            indexed_reasoning_summary,
            indexed_reasoning_content,
            usage,
        } = self;
        let mut resp = AiResponse::new(id, model);
        let has_indexed = !indexed_text.is_empty()
            || !indexed_refusal.is_empty()
            || !indexed_reasoning_summary.is_empty()
            || !indexed_reasoning_content.is_empty();
        let mut indexed_tools = BTreeMap::new();
        let mut derived = Vec::new();
        for item in items {
            match item {
                AccumulatedItem::ToolCall(index) if completed_items.is_empty() && has_indexed => {
                    if let Some(tool) = tool_calls
                        .get(index)
                        .and_then(Option::as_ref)
                        .filter(|call| !call.name.is_empty())
                        .cloned()
                    {
                        indexed_tools.insert(index, AiItem::function_call(tool));
                    }
                }
                item => {
                    if let Some(item) = accumulated_item_to_ai(item, &tool_calls) {
                        derived.push(item);
                    }
                }
            }
        }
        if completed_items.is_empty() {
            let mut indexed_items = materialize_indexed_items(
                &indexed_text,
                &indexed_refusal,
                &indexed_reasoning_summary,
                &indexed_reasoning_content,
            );
            indexed_items.extend(indexed_tools);
            resp.items = indexed_items.into_values().collect();
            resp.items.extend(derived);
        } else {
            let mut context = ReconciliationContext {
                indexed_text: &indexed_text,
                indexed_refusal: &indexed_refusal,
                indexed_reasoning_summary: &indexed_reasoning_summary,
                indexed_reasoning_content: &indexed_reasoning_content,
                tool_calls: &tool_calls,
                remaining: derived.into_iter().map(Some).collect(),
            };
            resp.items = completed_items
                .into_iter()
                .map(|(index, completed)| reconcile_completed_item(completed, index, &mut context))
                .collect();
            resp.items.extend(context.remaining.into_iter().flatten());
        }
        resp.stop_reason = stop_reason;
        resp.usage = usage;
        if let Some(metadata) = response_metadata {
            resp.vendor
                .ingress
                .insert("__open_responses_response_profile".into(), metadata);
        }
        if let Some((status, incomplete_details)) = terminal {
            resp.vendor.egress.insert(
                "__open_responses_terminal".into(),
                serde_json::json!({
                    "status": status,
                    "incomplete_details": incomplete_details,
                }),
            );
        }
        resp
    }
}

type ReasoningParts = (BTreeMap<usize, String>, BTreeMap<usize, String>);

fn materialize_indexed_items(
    indexed_text: &BTreeMap<(usize, usize), String>,
    indexed_refusal: &BTreeMap<(usize, usize), String>,
    indexed_reasoning_summary: &BTreeMap<(usize, usize), String>,
    indexed_reasoning_content: &BTreeMap<(usize, usize), String>,
) -> BTreeMap<usize, AiItem> {
    let mut messages: BTreeMap<usize, BTreeMap<usize, ContentBlock>> = BTreeMap::new();
    for ((output_index, content_index), text) in indexed_text {
        messages.entry(*output_index).or_default().insert(
            *content_index,
            ContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            },
        );
    }
    for ((output_index, content_index), refusal) in indexed_refusal {
        messages.entry(*output_index).or_default().insert(
            *content_index,
            ContentBlock::Refusal {
                refusal: refusal.clone(),
            },
        );
    }
    let mut reasoning: BTreeMap<usize, ReasoningParts> = BTreeMap::new();
    for ((output_index, content_index), text) in indexed_reasoning_summary {
        reasoning
            .entry(*output_index)
            .or_default()
            .0
            .insert(*content_index, text.clone());
    }
    for ((output_index, content_index), text) in indexed_reasoning_content {
        reasoning
            .entry(*output_index)
            .or_default()
            .1
            .insert(*content_index, text.clone());
    }
    let mut output = BTreeMap::new();
    for (output_index, parts) in messages {
        output.insert(
            output_index,
            AiItem {
                role: crate::protocol::ir::Role::Assistant,
                content: MessageContent::Blocks(parts.into_values().collect()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        );
    }
    for (output_index, (summary, content)) in reasoning {
        output.insert(
            output_index,
            AiItem::reasoning(
                summary.into_values().collect(),
                content.into_values().collect(),
                None,
            ),
        );
    }
    output
}

fn accumulated_item_to_ai(
    item: AccumulatedItem,
    tool_calls: &[Option<ToolCall>],
) -> Option<AiItem> {
    match item {
        AccumulatedItem::Text(text) if !text.is_empty() => Some(AiItem::output_text(text)),
        AccumulatedItem::Refusal(refusal) if !refusal.is_empty() => Some(AiItem::refusal(refusal)),
        AccumulatedItem::Thinking { text, signature }
            if !text.is_empty() || !signature.is_empty() =>
        {
            Some(AiItem::thinking(
                text,
                (!signature.is_empty()).then_some(signature),
            ))
        }
        AccumulatedItem::Reasoning {
            summary,
            content,
            signature,
        } if !summary.is_empty() || !content.is_empty() || !signature.is_empty() => {
            Some(AiItem::reasoning(
                (!summary.is_empty())
                    .then_some(summary)
                    .into_iter()
                    .collect(),
                (!content.is_empty())
                    .then_some(content)
                    .into_iter()
                    .collect(),
                (!signature.is_empty()).then_some(signature),
            ))
        }
        AccumulatedItem::ToolCall(index) => tool_calls
            .get(index)
            .and_then(Option::as_ref)
            .filter(|call| !call.name.is_empty())
            .cloned()
            .map(AiItem::function_call),
        AccumulatedItem::Unknown(item) => Some(item),
        AccumulatedItem::Text(_)
        | AccumulatedItem::Refusal(_)
        | AccumulatedItem::Thinking { .. }
        | AccumulatedItem::Reasoning { .. } => None,
    }
}

fn take_matching(
    remaining: &mut [Option<AiItem>],
    predicate: impl Fn(&AiItem) -> bool,
) -> Option<AiItem> {
    remaining
        .iter_mut()
        .find(|item| item.as_ref().is_some_and(&predicate))
        .and_then(Option::take)
}

struct ReconciliationContext<'a> {
    indexed_text: &'a BTreeMap<(usize, usize), String>,
    indexed_refusal: &'a BTreeMap<(usize, usize), String>,
    indexed_reasoning_summary: &'a BTreeMap<(usize, usize), String>,
    indexed_reasoning_content: &'a BTreeMap<(usize, usize), String>,
    tool_calls: &'a [Option<ToolCall>],
    remaining: Vec<Option<AiItem>>,
}

fn reconcile_completed_item(
    mut completed: AiItem,
    output_index: usize,
    context: &mut ReconciliationContext<'_>,
) -> AiItem {
    let indexed_text = context.indexed_text;
    let indexed_refusal = context.indexed_refusal;
    let indexed_reasoning_summary = context.indexed_reasoning_summary;
    let indexed_reasoning_content = context.indexed_reasoning_content;
    let tool_calls = context.tool_calls;
    let remaining = &mut context.remaining;
    if completed.function_call_ref().is_some() {
        if let Some(call) = tool_calls
            .get(output_index)
            .and_then(Option::as_ref)
            .filter(|call| !call.name.is_empty())
        {
            let call_id = call.id.as_str();
            let _ = take_matching(remaining, |item| {
                item.function_call_ref()
                    .is_some_and(|derived| derived.id == call_id)
            });
            let mut derived = AiItem::function_call(call.clone());
            derived.meta = completed.meta;
            return derived;
        }
        if let Some(mut derived) =
            take_matching(remaining, |item| item.function_call_ref().is_some())
        {
            derived.meta = completed.meta;
            return derived;
        }
    }
    if completed.unknown_ref().is_some()
        && let Some(mut derived) = take_matching(remaining, |item| item.unknown_ref().is_some())
    {
        derived.meta = completed.meta;
        return derived;
    }
    match &mut completed.content {
        MessageContent::Text(text) => {
            if let Some(replacement) = indexed_text.get(&(output_index, 0)) {
                text.clone_from(replacement);
            } else if let Some(derived) =
                take_matching(remaining, |item| item.output_text_ref().is_some())
                && let Some(replacement) = derived.output_text_ref()
            {
                text.clear();
                text.push_str(replacement);
            }
        }
        MessageContent::Blocks(blocks) => {
            for (content_index, block) in blocks.iter_mut().enumerate() {
                match block {
                    ContentBlock::Text { text, .. } => {
                        if let Some(replacement) = indexed_text.get(&(output_index, content_index))
                        {
                            text.clone_from(replacement);
                        } else if let Some(derived) =
                            take_matching(remaining, |item| item.output_text_ref().is_some())
                            && let Some(replacement) = derived.output_text_ref()
                        {
                            text.clear();
                            text.push_str(replacement);
                        }
                    }
                    ContentBlock::Refusal { refusal } => {
                        if let Some(replacement) =
                            indexed_refusal.get(&(output_index, content_index))
                        {
                            refusal.clone_from(replacement);
                        } else if let Some(derived) =
                            take_matching(remaining, |item| item.refusal_ref().is_some())
                            && let Some(replacement) = derived.refusal_ref()
                        {
                            refusal.clear();
                            refusal.push_str(replacement);
                        }
                    }
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        if let Some(derived) =
                            take_matching(remaining, |item| item.thinking_ref().is_some())
                            && let Some((replacement, derived_signature)) = derived.thinking_ref()
                        {
                            thinking.clear();
                            thinking.push_str(replacement);
                            if signature.is_none() {
                                *signature = derived_signature.map(str::to_owned);
                            }
                        }
                    }
                    ContentBlock::Reasoning {
                        summary,
                        content,
                        encrypted_content,
                    } => {
                        let indexed_summary = indexed_reasoning_summary
                            .range((output_index, 0)..=(output_index, usize::MAX))
                            .map(|(_, text)| text.clone())
                            .collect::<Vec<_>>();
                        let indexed_content = indexed_reasoning_content
                            .range((output_index, 0)..=(output_index, usize::MAX))
                            .map(|(_, text)| text.clone())
                            .collect::<Vec<_>>();
                        if !indexed_summary.is_empty() {
                            summary.clone_from(&indexed_summary);
                        }
                        if !indexed_content.is_empty() {
                            content.clone_from(&indexed_content);
                        }
                        if let Some(derived) =
                            take_matching(remaining, |item| item.reasoning_ref().is_some())
                            && let Some((
                                derived_summary,
                                derived_content,
                                derived_encrypted_content,
                            )) = derived.reasoning_ref()
                        {
                            if indexed_summary.is_empty() {
                                summary.clone_from(&derived_summary.to_vec());
                            }
                            if indexed_content.is_empty() {
                                content.clone_from(&derived_content.to_vec());
                            }
                            if encrypted_content.is_none() {
                                *encrypted_content = derived_encrypted_content.map(str::to_owned);
                            }
                        }
                        let expected_signature = encrypted_content.as_deref();
                        if let Some(derived) = take_matching(remaining, |item| {
                            item.thinking_ref().is_some_and(|(text, signature)| {
                                text.is_empty()
                                    && signature.is_some()
                                    && expected_signature
                                        .is_none_or(|expected| signature == Some(expected))
                            })
                        }) && encrypted_content.is_none()
                            && let Some((_, signature)) = derived.thinking_ref()
                        {
                            *encrypted_content = signature.map(str::to_owned);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    completed
}

pub(crate) fn ensure_tool_index(tool_calls: &mut Vec<Option<ToolCall>>, index: usize) {
    if tool_calls.len() <= index {
        tool_calls.resize(index + 1, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_stream_item_arrival_order() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            AiStreamDelta::MessageStart {
                id: "resp_1".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::TextDelta("before tool".into()),
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "lookup".into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{}".into(),
            },
            AiStreamDelta::ThinkingDelta("after tool".into()),
        ]);

        let response = accumulator.into_ai_response();

        assert!(response.items[0].output_text_ref().is_some());
        assert!(response.items[1].function_call_ref().is_some());
        assert!(response.items[2].thinking_ref().is_some());
    }
    #[test]
    fn merges_completed_item_metadata_without_reverting_transformed_text() {
        let mut accumulator = StreamResponseAccumulator::default();
        let completed = AiItem::output_text("before").with_graph_metadata(
            Some("msg_1".into()),
            Some(crate::protocol::ir::AiItemStatus::Completed),
            crate::protocol::ir::AiItemProvenance::Provider,
            crate::protocol::ir::AiItemAudience::Client,
        );
        accumulator.apply_all(&[
            AiStreamDelta::TextDelta("after".into()),
            AiStreamDelta::ItemDone {
                index: 0,
                item: completed,
            },
        ]);

        let response = accumulator.into_ai_response();

        assert_eq!(response.items[0].output_text_ref(), Some("after"));
        assert_eq!(response.items[0].id_ref(), Some("msg_1"));
    }
    #[test]
    fn preserves_reasoning_summary_content_and_encrypted_content() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            AiStreamDelta::ReasoningSummaryDelta {
                text: "summary".into(),
                obfuscation: None,
                output_index: None,
                content_index: None,
            },
            AiStreamDelta::ThinkingDeltaWithMetadata {
                text: "full reasoning".into(),
                obfuscation: None,
                output_index: None,
                content_index: None,
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: AiItem::reasoning(
                    vec!["provider summary".into()],
                    vec!["provider content".into()],
                    Some("opaque".into()),
                ),
            },
        ]);

        let response = accumulator.into_ai_response();
        let (summary, content, encrypted) = response.items[0]
            .reasoning_ref()
            .expect("typed reasoning item");

        assert_eq!(summary, ["summary"]);
        assert_eq!(content, ["full reasoning"]);
        assert_eq!(encrypted, Some("opaque"));
    }

    #[test]
    fn indexed_reasoning_signature_does_not_create_duplicate_history_item() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            AiStreamDelta::ReasoningSummaryDelta {
                text: "summary".into(),
                obfuscation: None,
                output_index: Some(0),
                content_index: Some(0),
            },
            AiStreamDelta::ThinkingSignature("opaque".into()),
            AiStreamDelta::ItemDone {
                index: 0,
                item: AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("opaque".into())),
            },
        ]);

        let response = accumulator.into_ai_response();

        assert_eq!(response.items.len(), 1);
        assert_eq!(
            response.items[0].reasoning_ref(),
            Some((&["summary".to_string()][..], &[][..], Some("opaque")))
        );
    }

    #[test]
    fn open_responses_completed_reasoning_items_remain_one_to_one() {
        let in_progress_response =
            crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
                "resp_1",
                "gpt-5.6-luna",
                "in_progress",
                Vec::new(),
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
            );
        let events = [
            serde_json::json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": in_progress_response
            }),
            serde_json::json!({
                "type": "response.output_item.added",
                "sequence_number": 1,
                "output_index": 0,
                "item": {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [],
                    "content": []
                }
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "sequence_number": 2,
                "output_index": 0,
                "item": {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [],
                    "content": [],
                    "encrypted_content": "first-ciphertext"
                }
            }),
            serde_json::json!({
                "type": "response.output_item.added",
                "sequence_number": 3,
                "output_index": 1,
                "item": {
                    "type": "reasoning",
                    "id": "rs_2",
                    "summary": [],
                    "content": []
                }
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "sequence_number": 4,
                "output_index": 1,
                "item": {
                    "type": "reasoning",
                    "id": "rs_2",
                    "summary": [],
                    "content": [],
                    "encrypted_content": "second-ciphertext"
                }
            }),
        ]
        .into_iter()
        .map(|event| {
            let event_type = event["type"].as_str().expect("event type");
            format!("event: {event_type}\ndata: {event}\n\n")
        })
        .collect::<String>();
        let deltas = crate::protocol::codec::open_responses::parser::ResponsesStreamParser::new()
            .parse_chunk(&events)
            .expect("Open Responses reasoning events");
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::ThinkingSignature(_)))
        );

        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&deltas);
        let response = accumulator.into_ai_response();
        let encrypted = response
            .items
            .iter()
            .filter_map(AiItem::reasoning_ref)
            .filter_map(|(_, _, encrypted)| encrypted)
            .collect::<Vec<_>>();

        assert_eq!(encrypted, ["first-ciphertext", "second-ciphertext"]);
        assert_eq!(response.items.len(), 2);
    }

    #[test]
    fn retains_completed_encrypted_only_reasoning_by_output_index() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply(&AiStreamDelta::ItemDone {
            index: 2,
            item: AiItem::reasoning(Vec::new(), Vec::new(), Some("opaque".into())),
        });

        let response = accumulator.into_ai_response();

        assert_eq!(response.items.len(), 1);
        assert_eq!(
            response.items[0]
                .reasoning_ref()
                .and_then(|(_, _, encrypted)| encrypted),
            Some("opaque")
        );
    }

    #[test]
    fn groups_multiple_message_parts_under_the_completed_output_item() {
        let mut accumulator = StreamResponseAccumulator::default();
        let completed = AiItem {
            role: crate::protocol::ir::Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "provider text".into(),
                    cache_control: None,
                },
                ContentBlock::Refusal {
                    refusal: "provider refusal".into(),
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        accumulator.apply_all(&[
            AiStreamDelta::TextDelta("transformed text".into()),
            AiStreamDelta::RefusalDelta("transformed refusal".into()),
            AiStreamDelta::ItemDone {
                index: 0,
                item: completed,
            },
        ]);

        let response = accumulator.into_ai_response();

        assert_eq!(response.items.len(), 1);
        assert!(matches!(
            &response.items[0].content,
            MessageContent::Blocks(blocks)
                if matches!(
                    blocks.as_slice(),
                    [
                        ContentBlock::Text { text, .. },
                        ContentBlock::Refusal { refusal },
                    ] if text == "transformed text" && refusal == "transformed refusal"
                )
        ));
    }
    #[test]
    fn overlays_indexed_text_without_merging_completed_messages() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            AiStreamDelta::TextDeltaWithMetadata {
                text: "first".into(),
                logprobs: Vec::new(),
                obfuscation: None,
                output_index: Some(0),
                content_index: Some(0),
            },
            AiStreamDelta::RefusalDeltaWithIndex {
                text: "second".into(),
                output_index: 1,
                content_index: 0,
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: AiItem::output_text("provider first"),
            },
            AiStreamDelta::ItemDone {
                index: 1,
                item: AiItem::refusal("provider second"),
            },
        ]);

        let response = accumulator.into_ai_response();

        assert_eq!(response.items.len(), 2);
        assert_eq!(response.items[0].output_text_ref(), Some("first"));
        assert_eq!(response.items[1].refusal_ref(), Some("second"));
    }
    #[test]
    fn indexed_reasoning_deltas_remain_separate_items() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            AiStreamDelta::ReasoningSummaryDelta {
                text: "first".into(),
                obfuscation: None,
                output_index: Some(0),
                content_index: Some(0),
            },
            AiStreamDelta::ReasoningSummaryDelta {
                text: "second".into(),
                obfuscation: None,
                output_index: Some(1),
                content_index: Some(0),
            },
        ]);

        let response = accumulator.into_ai_response();

        assert_eq!(response.items.len(), 2);
        assert_eq!(response.items[0].reasoning_ref().unwrap().0, ["first"]);
        assert_eq!(response.items[1].reasoning_ref().unwrap().0, ["second"]);
    }
    #[test]
    fn completed_item_does_not_restore_dropped_semantic_text() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply(&AiStreamDelta::ItemDone {
            index: 0,
            item: AiItem::output_text("provider secret"),
        });

        let response = accumulator.into_ai_response();

        assert_eq!(response.items[0].output_text_ref(), Some(""));
    }

    #[test]
    fn stream_event_metadata_does_not_become_an_output_item() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply(&AiStreamDelta::Unknown {
            raw: serde_json::json!({
                "__open_responses_event": {
                    "type": "response.output_text.annotation.added",
                    "annotation": {"type": "url_citation", "url": "https://example.test"}
                }
            })
            .to_string(),
        });

        assert!(accumulator.into_ai_response().items.is_empty());
    }

    #[test]
    fn reconciles_out_of_order_completed_tool_calls_by_output_index() {
        let mut accumulator = StreamResponseAccumulator::default();
        let call_b = ToolCall {
            id: "call_b".into(),
            name: "second".into(),
            arguments: r#"{"b":2}"#.into(),
        };
        let call_a = ToolCall {
            id: "call_a".into(),
            name: "first".into(),
            arguments: r#"{"a":1}"#.into(),
        };
        accumulator.apply_all(&[
            AiStreamDelta::ToolCallComplete {
                index: 1,
                tool_call: call_b.clone(),
            },
            AiStreamDelta::ToolCallComplete {
                index: 0,
                tool_call: call_a.clone(),
            },
            AiStreamDelta::ItemDone {
                index: 1,
                item: AiItem::function_call(call_b),
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: AiItem::function_call(call_a),
            },
        ]);

        let response = accumulator.into_ai_response();
        assert_eq!(response.items.len(), 2);
        assert_eq!(response.items[0].function_call_ref().unwrap().id, "call_a");
        assert_eq!(response.items[0].function_call_ref().unwrap().name, "first");
        assert_eq!(response.items[1].function_call_ref().unwrap().id, "call_b");
        assert_eq!(
            response.items[1].function_call_ref().unwrap().name,
            "second"
        );
    }
}

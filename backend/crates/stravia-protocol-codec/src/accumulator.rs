//! Stream response accumulator: buffers streaming deltas into a complete
//! `AiResponse` for caching and formatted response aggregation.

use std::collections::BTreeMap;
use std::sync::Arc;

use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Usage;
use stravia_runtime_contract::protocol::ir::request::ToolCall;
use stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;

/// Ordered source part identity: `(is_reasoning_content, index)`.
/// The explicit category keeps summaries before content without reserving bits
/// from a platform-dependent `usize` index.
pub type CanonicalPartIndex = (bool, usize);

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
pub struct StreamResponseAccumulator {
    pub id: String,
    pub model: String,
    response_metadata: Option<serde_json::Value>,
    chat_reasoning_field: Option<serde_json::Value>,
    google_response_metadata: Option<serde_json::Map<String, serde_json::Value>>,
    items: Vec<AccumulatedItem>,
    tool_calls: Vec<Option<ToolCall>>,
    completed_items: BTreeMap<usize, AiItem>,
    item_closed: bool,
    indexed_text: BTreeMap<(usize, usize), String>,
    indexed_refusal: BTreeMap<(usize, usize), String>,
    indexed_reasoning_summary: BTreeMap<(usize, usize), String>,
    indexed_reasoning_content: BTreeMap<(usize, usize), String>,
    pub stop_reason: Option<String>,
    pub terminal: Option<(String, Option<serde_json::Value>)>,
    pub usage: Usage,
    next_item_ordinal: usize,
    indexed_mode: bool,
    indexed_ordinals: BTreeMap<usize, usize>,
    derived_ordinals: BTreeMap<usize, usize>,
}

fn completed_item_semantic_shell(item: &AiItem) -> AiItem {
    let mut shell = item.clone();
    if shell.role != stravia_runtime_contract::protocol::ir::Role::Assistant {
        return shell;
    }
    // The shell discards text, so replace the shared payload instead of copying it to clear it.
    match &mut shell.content {
        MessageContent::Text(text) => *text = String::new().into(),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => *text = String::new().into(),
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
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.tool_calls.iter().filter_map(Option::as_ref)
    }

    pub fn apply_all(&mut self, deltas: &[AiStreamDelta]) {
        for delta in deltas {
            self.apply(delta);
        }
    }

    pub fn apply(&mut self, delta: &AiStreamDelta) {
        self.apply_content(delta);
    }

    /// Apply a delta, retaining the identity assigned to its canonical item.
    /// Source indices and unindexed slots have separate namespaces; completion
    /// and late IDs retain published identity. Use this for every observed delta.
    pub fn apply_with_identity(
        &mut self,
        delta: &AiStreamDelta,
    ) -> Option<(usize, CanonicalPartIndex)> {
        self.apply_content(delta);
        let indexed = match delta {
            AiStreamDelta::TextDeltaWithMetadata {
                output_index: Some(index),
                content_index: Some(part),
                ..
            }
            | AiStreamDelta::ThinkingDeltaWithMetadata {
                output_index: Some(index),
                content_index: Some(part),
                ..
            }
            | AiStreamDelta::ReasoningSummaryDelta {
                output_index: Some(index),
                content_index: Some(part),
                ..
            } => Some((*index, *part)),
            AiStreamDelta::RefusalDeltaWithIndex {
                output_index,
                content_index,
                ..
            } => Some((*output_index, *content_index)),
            AiStreamDelta::ProtectedThinkingStart { index } => Some((*index, 0)),
            // Completion-only items (including Gemini media) still own a source
            // index and must retain an ordinal through response materialization.
            AiStreamDelta::ItemDone { index, .. } => Some((*index, 0)),
            _ => None,
        };
        if let Some((index, part)) = indexed {
            if !matches!(delta, AiStreamDelta::ItemDone { .. }) {
                self.indexed_mode = true;
            }
            if !self.indexed_ordinals.contains_key(&index) {
                let ordinal =
                    if !self.indexed_mode && matches!(delta, AiStreamDelta::ItemDone { .. }) {
                        self.derived_ordinals.get(&index).copied()
                    } else {
                        None
                    }
                    .unwrap_or_else(|| {
                        let ordinal = self.next_item_ordinal;
                        self.next_item_ordinal += 1;
                        ordinal
                    });
                self.indexed_ordinals.insert(index, ordinal);
            }
            return Some((
                self.indexed_ordinals[&index],
                (
                    matches!(delta, AiStreamDelta::ThinkingDeltaWithMetadata { .. }),
                    part,
                ),
            ));
        }
        // Allocate from the accumulator's real item slots, including tools and
        // opaque items. Indexed and unindexed namespaces share one allocator.
        for index in self.derived_ordinals.len()..self.items.len() {
            self.derived_ordinals.insert(index, self.next_item_ordinal);
            self.next_item_ordinal += 1;
        }
        match delta {
            AiStreamDelta::ThinkingDelta(_)
            | AiStreamDelta::ThinkingDeltaWithMetadata { .. }
            | AiStreamDelta::ReasoningSummaryDelta { .. }
            | AiStreamDelta::TextDelta(_)
            | AiStreamDelta::TextDeltaWithMetadata { .. }
            | AiStreamDelta::RefusalDelta(_) => self.items.len().checked_sub(1).map(|index| {
                (
                    self.derived_ordinals[&index],
                    (
                        matches!(delta, AiStreamDelta::ThinkingDeltaWithMetadata { .. }),
                        0,
                    ),
                )
            }),
            _ => None,
        }
    }

    fn apply_content(&mut self, delta: &AiStreamDelta) {
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
                if let Some(object) = metadata.as_object()
                    && let Some(value) = object.get(CHAT_REASONING_FIELD_META)
                {
                    if value.is_null() || value.as_str() == Some("") {
                        self.chat_reasoning_field = Some(value.clone());
                    }
                    // Chat 字段存在性不是 Responses profile；单独信号不能抹掉上游 profile。
                    if object.len() > 1 {
                        let mut profile = object.clone();
                        profile.remove(CHAT_REASONING_FIELD_META);
                        self.response_metadata = Some(serde_json::Value::Object(profile));
                    }
                } else {
                    self.response_metadata = Some(metadata.clone());
                }
            }
            AiStreamDelta::ProtectedThinkingStart { .. } => {}
            AiStreamDelta::ThinkingDelta(text) => {
                match self.items.last_mut() {
                    Some(AccumulatedItem::Thinking { text: current, .. }) if !self.item_closed => {
                        current.push_str(text);
                    }
                    _ => self.items.push(AccumulatedItem::Thinking {
                        text: text.clone(),
                        signature: String::new(),
                    }),
                }
                self.item_closed = false;
            }
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
            AiStreamDelta::ThinkingDeltaWithMetadata { text, .. } => {
                match self.items.last_mut() {
                    Some(AccumulatedItem::Reasoning {
                        content: current, ..
                    }) if !self.item_closed => current.push_str(text),
                    _ => self.items.push(AccumulatedItem::Reasoning {
                        summary: String::new(),
                        content: text.clone(),
                        signature: String::new(),
                    }),
                }
                self.item_closed = false;
            }
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
            AiStreamDelta::ReasoningSummaryDelta { text, .. } => {
                match self.items.last_mut() {
                    Some(AccumulatedItem::Reasoning {
                        summary: current, ..
                    }) if !self.item_closed => current.push_str(text),
                    _ => self.items.push(AccumulatedItem::Reasoning {
                        summary: text.clone(),
                        content: String::new(),
                        signature: String::new(),
                    }),
                }
                self.item_closed = false;
            }
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
                        call.id = id.clone().into();
                    }
                    call.name.push_str(name);
                } else {
                    self.tool_calls[*index] = Some(ToolCall {
                        id: (id.clone()).into(),
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
                        id: (stravia_runtime_contract::identifier::new_id()).into(),
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
                self.item_closed = true;
                self.completed_items
                    .insert(*index, completed_item_semantic_shell(item));
            }
            AiStreamDelta::Usage(usage) => {
                // 用量帧是快照而非增量；未知字段不抹掉已报告值，明确的零仍可修订。
                if usage.required_components_known {
                    self.usage.prompt_tokens = usage.prompt_tokens;
                    self.usage.completion_tokens = usage.completion_tokens;
                    self.usage.total_tokens = usage.total_tokens;
                    self.usage.required_components_known = true;
                } else if !self.usage.required_components_known {
                    if usage.prompt_tokens > 0 {
                        self.usage.prompt_tokens = usage.prompt_tokens;
                    }
                    if usage.completion_tokens > 0 {
                        self.usage.completion_tokens = usage.completion_tokens;
                    }
                    if usage.total_tokens > 0 {
                        self.usage.total_tokens = usage.total_tokens;
                    }
                }
                self.usage.cache_read_tokens =
                    usage.cache_read_tokens.or(self.usage.cache_read_tokens);
                self.usage.cache_creation_tokens = usage
                    .cache_creation_tokens
                    .or(self.usage.cache_creation_tokens);
                self.usage.reasoning_tokens =
                    usage.reasoning_tokens.or(self.usage.reasoning_tokens);
                if let Some(server_tool_use) = &usage.server_tool_use {
                    self.usage.server_tool_use = Some(server_tool_use.clone());
                }
            }
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
                if let Ok(mut raw) = serde_json::from_str::<serde_json::Value>(raw) {
                    if let Some(metadata) = raw
                        .get_mut("__google_response_metadata")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        // Gemini metadata 属于响应，不参与内容项或正文分片的排序。
                        let target = self.google_response_metadata.get_or_insert_default();
                        for (key, value) in std::mem::take(metadata) {
                            target.entry(key).or_insert(value);
                        }
                    } else if raw.get("__open_responses_event").is_none() {
                        self.items
                            .push(AccumulatedItem::Unknown(AiItem::unknown(raw)));
                    }
                }
            }
        }
    }
    fn push_text(&mut self, text: &str) {
        match self.items.last_mut() {
            Some(AccumulatedItem::Text(current)) if !self.item_closed => current.push_str(text),
            _ => self.items.push(AccumulatedItem::Text(text.to_owned())),
        }
        self.item_closed = false;
    }

    fn push_refusal(&mut self, text: &str) {
        match self.items.last_mut() {
            Some(AccumulatedItem::Refusal(current)) if !self.item_closed => current.push_str(text),
            _ => self.items.push(AccumulatedItem::Refusal(text.to_owned())),
        }
        self.item_closed = false;
    }

    pub fn into_ai_response(self) -> AiResponse {
        self.materialize(false).0
    }

    /// Return response items with identities established by apply_with_identity.
    pub fn into_ai_response_with_ordinals(self) -> (AiResponse, Vec<usize>) {
        self.materialize(true)
    }

    fn materialize(self, collect_ordinals: bool) -> (AiResponse, Vec<usize>) {
        let Self {
            id,
            model,
            response_metadata,
            chat_reasoning_field,
            google_response_metadata,
            items,
            tool_calls,
            completed_items,
            item_closed: _,
            indexed_text,
            stop_reason,
            terminal,
            indexed_refusal,
            indexed_reasoning_summary,
            indexed_reasoning_content,
            usage,
            next_item_ordinal: _,
            indexed_mode: _,
            indexed_ordinals,
            derived_ordinals,
        } = self;
        let mut resp = AiResponse::new(id, model);
        let has_indexed = !indexed_text.is_empty()
            || !indexed_refusal.is_empty()
            || !indexed_reasoning_summary.is_empty()
            || !indexed_reasoning_content.is_empty();
        let mut indexed_tools = BTreeMap::new();
        let mut tool_ordinals = BTreeMap::new();
        let mut derived = Vec::new();
        let mut derived_ids = Vec::new();
        let mut ordinals = Vec::new();
        for (source_index, item) in items.into_iter().enumerate() {
            match item {
                AccumulatedItem::ToolCall(index) if completed_items.is_empty() && has_indexed => {
                    if let Some(tool) = tool_calls
                        .get(index)
                        .and_then(Option::as_ref)
                        .filter(|call| !call.name.is_empty())
                        .cloned()
                    {
                        indexed_tools.insert(index, AiItem::function_call(tool));
                        if collect_ordinals {
                            tool_ordinals.insert(index, derived_ordinals[&source_index]);
                        }
                    }
                }
                item => {
                    if let Some(item) = accumulated_item_to_ai(item, &tool_calls) {
                        derived.push(item);
                        if collect_ordinals {
                            derived_ids.push(derived_ordinals[&source_index]);
                        }
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
            if collect_ordinals {
                ordinals.extend(indexed_items.keys().map(|index| {
                    indexed_ordinals
                        .get(index)
                        .or_else(|| tool_ordinals.get(index))
                        .copied()
                        .expect("indexed item identity")
                }));
            }
            resp.items = indexed_items.into_values().collect();
            ordinals.extend(derived_ids);
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
                .map(|(index, completed)| {
                    if collect_ordinals {
                        ordinals.push(indexed_ordinals[&index]);
                    }
                    reconcile_completed_item(completed, index, &mut context)
                })
                .collect();
            if collect_ordinals {
                for (item, ordinal) in context.remaining.into_iter().zip(derived_ids) {
                    if let Some(item) = item {
                        resp.items.push(item);
                        ordinals.push(ordinal);
                    }
                }
            } else {
                resp.items.extend(context.remaining.into_iter().flatten());
            }
        }
        resp.stop_reason = stop_reason;
        resp.usage = usage;
        if let Some(value) = chat_reasoning_field {
            resp.vendor
                .ingress
                .insert(CHAT_REASONING_FIELD_META.into(), value);
        }
        if let Some(metadata) = google_response_metadata {
            resp.vendor.ingress.insert(
                "__google_response_metadata".into(),
                serde_json::Value::Object(metadata),
            );
        }
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
        (resp, ordinals)
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
                text: text.clone().into(),
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
                role: stravia_runtime_contract::protocol::ir::Role::Assistant,
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

fn take_output_text(remaining: &mut [Option<AiItem>]) -> Option<Arc<String>> {
    let item = take_matching(remaining, |item| item.output_text_ref().is_some())?;
    match item.content {
        MessageContent::Text(text) => Some(text),
        MessageContent::Blocks(mut blocks) => match blocks.pop() {
            Some(ContentBlock::Text { text, .. }) => Some(text),
            _ => unreachable!("output_text_ref only accepts a single text block"),
        },
    }
}

fn replace_indexed_text(text: &mut Arc<String>, replacement: &String) {
    // Reuse unique storage, but never copy the old shared payload just to overwrite it.
    if let Some(text) = Arc::get_mut(text) {
        text.clone_from(replacement);
    } else {
        *text = replacement.clone().into();
    }
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
                replace_indexed_text(text, replacement);
            } else if let Some(replacement) = take_output_text(remaining) {
                *text = replacement;
            }
        }
        MessageContent::Blocks(blocks) => {
            for (content_index, block) in blocks.iter_mut().enumerate() {
                match block {
                    ContentBlock::Text { text, .. } => {
                        if let Some(replacement) = indexed_text.get(&(output_index, content_index))
                        {
                            replace_indexed_text(text, replacement);
                        } else if let Some(replacement) = take_output_text(remaining) {
                            *text = replacement;
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

pub fn ensure_tool_index(tool_calls: &mut Vec<Option<ToolCall>>, index: usize) {
    if tool_calls.len() <= index {
        tool_calls.resize(index + 1, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_snapshots_preserve_known_fields_and_accept_reported_zero() {
        let mut accumulator = StreamResponseAccumulator::default();
        accumulator.apply_all(&[
            AiStreamDelta::Usage(Usage {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 10,
                required_components_known: true,
                ..Default::default()
            }),
            AiStreamDelta::Usage(Usage {
                cache_read_tokens: Some(0),
                ..Default::default()
            }),
            AiStreamDelta::Usage(Usage::default()),
        ]);
        let usage = &accumulator.usage;
        assert!(usage.required_components_known);
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens
            ),
            (7, 3, 10)
        );
        assert_eq!(usage.cache_read_tokens, Some(0));
        accumulator.apply(&AiStreamDelta::Usage(Usage {
            required_components_known: true,
            ..Default::default()
        }));
        let response = accumulator.into_ai_response();
        assert!(response.usage.required_components_known);
        assert_eq!(
            (
                response.usage.prompt_tokens,
                response.usage.completion_tokens,
                response.usage.total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(response.usage.cache_read_tokens, Some(0));
        assert_eq!(response.usage.reasoning_tokens, None);
    }

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
    fn separates_explicit_chat_reasoning_from_response_profile() {
        use stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;

        for value in [serde_json::json!(""), serde_json::Value::Null] {
            let mut accumulator = StreamResponseAccumulator::default();
            accumulator.apply_all(&[
                AiStreamDelta::ResponseMetadata {
                    metadata: serde_json::json!({"temperature": 0.4, "metadata": {"trace": "kept"}}),
                },
                AiStreamDelta::ResponseMetadata {
                    metadata: serde_json::json!({CHAT_REASONING_FIELD_META: value.clone()}),
                },
                AiStreamDelta::TextDelta("answer".into()),
            ]);

            let response = accumulator.into_ai_response();
            assert_eq!(
                response.vendor.ingress.get(CHAT_REASONING_FIELD_META),
                Some(&value)
            );
            assert_eq!(
                response.vendor.ingress["__open_responses_response_profile"],
                serde_json::json!({"temperature": 0.4, "metadata": {"trace": "kept"}})
            );
            assert_eq!(response.items.len(), 1);
            assert_eq!(response.items[0].output_text_ref(), Some("answer"));
            let formatted = crate::codec::open_responses::formatter::ResponsesResponseFormatter
                .format_response(&response);
            assert!(formatted.get(CHAT_REASONING_FIELD_META).is_none());
            assert_eq!(formatted["temperature"], 0.4);
            assert_eq!(formatted["output"].as_array().unwrap().len(), 1);
            assert_eq!(formatted["output"][0]["type"], "message");
        }
    }

    #[test]
    fn merges_completed_item_metadata_without_reverting_transformed_text() {
        let mut accumulator = StreamResponseAccumulator::default();
        let completed = AiItem::output_text("before").with_graph_metadata(
            Some("msg_1".into()),
            Some(stravia_runtime_contract::protocol::ir::AiItemStatus::Completed),
            stravia_runtime_contract::protocol::ir::AiItemProvenance::Provider,
            stravia_runtime_contract::protocol::ir::AiItemAudience::Client,
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
            crate::codec::open_responses::formatter::response_resource_snapshot(
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
        let deltas = crate::codec::open_responses::parser::ResponsesStreamParser::new()
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
            role: stravia_runtime_contract::protocol::ir::Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "provider text".to_owned().into(),
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
                    ] if text.as_str() == "transformed text" && refusal == "transformed refusal"
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
    fn late_item_id_preserves_published_canonical_identity() {
        let mut accumulator = StreamResponseAccumulator::default();
        let identity = accumulator
            .apply_with_identity(&AiStreamDelta::ThinkingDelta("reason".into()))
            .unwrap();
        let mut item = AiItem::thinking("reason", None);
        item.meta = Some(
            stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
                serde_json::json!({"id": "late-id"}),
            ),
        );
        let completed = accumulator
            .apply_with_identity(&AiStreamDelta::ItemDone { index: 0, item })
            .unwrap();
        let (response, ordinals) = accumulator.into_ai_response_with_ordinals();
        assert_eq!(completed.0, identity.0);
        assert_eq!(ordinals, [identity.0]);
        assert_eq!(response.items[0].id_ref(), Some("late-id"));
    }

    #[test]
    fn sparse_indexed_and_unindexed_items_keep_distinct_identities() {
        let mut accumulator = StreamResponseAccumulator::default();
        let late_source = accumulator
            .apply_with_identity(&AiStreamDelta::ReasoningSummaryDelta {
                text: "second".into(),
                obfuscation: None,
                output_index: Some(9),
                content_index: Some(0),
            })
            .unwrap();
        let derived = accumulator
            .apply_with_identity(&AiStreamDelta::ThinkingDelta("unindexed".into()))
            .unwrap();
        let early_source = accumulator
            .apply_with_identity(&AiStreamDelta::ReasoningSummaryDelta {
                text: "first".into(),
                obfuscation: None,
                output_index: Some(2),
                content_index: Some(0),
            })
            .unwrap();
        assert_ne!(late_source.0, derived.0);
        assert_ne!(early_source.0, derived.0);
        assert_ne!(early_source.0, late_source.0);
        let (response, ordinals) = accumulator.into_ai_response_with_ordinals();
        assert_eq!(ordinals, [early_source.0, late_source.0, derived.0]);
        assert_eq!(response.items[0].reasoning_ref().unwrap().0, ["first"]);
        assert_eq!(response.items[1].reasoning_ref().unwrap().0, ["second"]);
        assert!(
            matches!(&response.items[2].content, MessageContent::Blocks(parts) if matches!(&parts[0], ContentBlock::Thinking { thinking, .. } if thinking == "unindexed"))
        );
    }

    #[test]
    fn reasoning_summary_maximum_index_still_precedes_content() {
        let mut accumulator = StreamResponseAccumulator::default();
        let content = accumulator
            .apply_with_identity(&AiStreamDelta::ThinkingDeltaWithMetadata {
                text: "content".into(),
                obfuscation: None,
                output_index: Some(3),
                content_index: Some(0),
            })
            .unwrap();
        let summary = accumulator
            .apply_with_identity(&AiStreamDelta::ReasoningSummaryDelta {
                text: "summary".into(),
                obfuscation: None,
                output_index: Some(3),
                content_index: Some(usize::MAX),
            })
            .unwrap();
        assert_eq!(summary.0, content.0);
        assert!(summary.1 < content.1);
        let response = accumulator.into_ai_response();
        let (summary, content, _) = response.items[0].reasoning_ref().unwrap();
        assert_eq!(summary, ["summary"]);
        assert_eq!(content, ["content"]);
    }

    #[test]
    fn reasoning_parts_order_summary_before_content_despite_arrival_order() {
        let mut accumulator = StreamResponseAccumulator::default();
        let content = accumulator
            .apply_with_identity(&AiStreamDelta::ThinkingDeltaWithMetadata {
                text: "content".into(),
                obfuscation: None,
                output_index: Some(3),
                content_index: Some(0),
            })
            .unwrap();
        let second = accumulator
            .apply_with_identity(&AiStreamDelta::ReasoningSummaryDelta {
                text: "second".into(),
                obfuscation: None,
                output_index: Some(3),
                content_index: Some(1),
            })
            .unwrap();
        let first = accumulator
            .apply_with_identity(&AiStreamDelta::ReasoningSummaryDelta {
                text: "first".into(),
                obfuscation: None,
                output_index: Some(3),
                content_index: Some(0),
            })
            .unwrap();
        assert_eq!(content.0, first.0);
        assert_eq!(second.0, first.0);
        assert!(first.1 < second.1 && second.1 < content.1);
        let (response, ordinals) = accumulator.into_ai_response_with_ordinals();
        assert_eq!(ordinals, [first.0]);
        let (summary, reasoning, _) = response.items[0].reasoning_ref().unwrap();
        assert_eq!(summary, ["first", "second"]);
        assert_eq!(reasoning, ["content"]);
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

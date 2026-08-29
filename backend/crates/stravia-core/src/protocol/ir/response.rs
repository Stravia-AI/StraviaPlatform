//! `AiResponse` — the unified egress IR produced by all codec response parsers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::ir::error::AiError;
use crate::protocol::ir::request::AiItem;
use crate::protocol::ir::usage::Usage;
use crate::protocol::ir::vendor_ext::VendorExtensions;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingVector {
    Floats(Vec<f64>),
    Base64(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub object: Option<String>,
    pub index: u32,
    pub embedding: EmbeddingVector,
    #[serde(flatten)]
    pub extensions: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingOutput {
    pub object: Option<String>,
    pub data: Vec<EmbeddingData>,
    #[serde(flatten)]
    pub extensions: serde_json::Map<String, Value>,
}

// ── AiResponse ────────────────────────────────────────────────────────────────

/// Unified egress IR produced by all codec response parsers and the accumulator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiResponse {
    /// The unique response ID assigned by the provider.
    pub id: String,
    /// The model variant that was actually used.
    pub model: String,
    /// Ordered canonical item graph.
    pub items: Vec<AiItem>,
    /// Stop reason (e.g. `"stop"`, `"tool_use"`, `"length"`).
    pub stop_reason: Option<String>,
    /// Token usage.
    pub usage: Usage,
    /// Typed embedding payload for embedding responses.
    pub embedding_output: Option<EmbeddingOutput>,
    /// Normalized error — populated when the provider returns an error response
    /// or the parser detects a mid-stream error.
    pub error: Option<AiError>,
    /// Vendor-specific extra fields.
    pub vendor: VendorExtensions,
    #[serde(skip)]
    pub(crate) trusted_media_turn_ids: Vec<String>,
}

impl AiResponse {
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            items: Vec::new(),
            stop_reason: None,
            usage: Usage::default(),
            embedding_output: None,
            error: None,
            vendor: VendorExtensions::default(),
            trusted_media_turn_ids: Vec::new(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

impl AiResponse {
    pub fn output_texts(&self) -> impl Iterator<Item = &str> {
        self.items.iter().flat_map(|item| match &item.content {
            crate::protocol::ir::MessageContent::Text(text)
                if item.role == crate::protocol::ir::Role::Assistant && !text.is_empty() =>
            {
                EitherText::One(std::iter::once(text.as_str()))
            }
            crate::protocol::ir::MessageContent::Blocks(blocks)
                if item.role == crate::protocol::ir::Role::Assistant =>
            {
                EitherText::Many(blocks.iter().filter_map(|block| match block {
                    crate::protocol::ir::ContentBlock::Text { text, .. } if !text.is_empty() => {
                        Some(text.as_str())
                    }
                    _ => None,
                }))
            }
            _ => EitherText::None(std::iter::empty()),
        })
    }

    pub fn output_text(&self) -> String {
        self.output_texts().collect()
    }

    pub fn reasoning_items(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.items.iter().filter_map(AiItem::thinking_ref)
    }

    pub fn protected_reasoning_signatures(&self) -> impl Iterator<Item = Option<&str>> {
        self.items.iter().filter_map(|item| {
            item.thinking_ref()
                .map(|(_, signature)| signature)
                .or_else(|| {
                    item.reasoning_ref()
                        .map(|(_, _, encrypted_content)| encrypted_content)
                })
        })
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &crate::protocol::ir::ToolCall> {
        self.items.iter().filter_map(AiItem::function_call_ref)
    }

    pub fn tool_calls_mut(&mut self) -> impl Iterator<Item = &mut crate::protocol::ir::ToolCall> {
        self.items.iter_mut().filter_map(AiItem::function_call_mut)
    }

    pub fn push_output_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() {
            self.items.push(AiItem::output_text(text));
        }
    }

    pub fn push_reasoning(&mut self, text: impl Into<String>, signature: Option<String>) {
        let text = text.into();
        if !text.is_empty() || signature.is_some() {
            self.items.push(AiItem::thinking(text, signature));
        }
    }

    pub fn push_tool_call(&mut self, call: crate::protocol::ir::ToolCall) {
        self.items.push(AiItem::function_call(call));
    }

    pub fn extend_tool_calls(
        &mut self,
        calls: impl IntoIterator<Item = crate::protocol::ir::ToolCall>,
    ) {
        self.items
            .extend(calls.into_iter().map(AiItem::function_call));
    }

    pub fn replace_output_text(&mut self, text: impl Into<String>) {
        let mut replacement = text.into();
        let mut replaced = false;
        for item in &mut self.items {
            if item.role != crate::protocol::ir::Role::Assistant {
                continue;
            }
            match &mut item.content {
                crate::protocol::ir::MessageContent::Text(text)
                    if item.tool_calls.is_none() || !text.is_empty() =>
                {
                    if replaced {
                        text.clear();
                    } else {
                        *text = std::mem::take(&mut replacement);
                        replaced = true;
                    }
                }
                crate::protocol::ir::MessageContent::Blocks(blocks)
                    if item.tool_calls.is_none()
                        || blocks.iter().any(|block| {
                            matches!(
                                block,
                                crate::protocol::ir::ContentBlock::Text { text, .. }
                                    if !text.is_empty()
                            )
                        }) =>
                {
                    for block in blocks {
                        let crate::protocol::ir::ContentBlock::Text { text, .. } = block else {
                            continue;
                        };
                        if replaced {
                            text.clear();
                        } else {
                            *text = std::mem::take(&mut replacement);
                            replaced = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if !replaced && !replacement.is_empty() {
            self.items.push(AiItem::output_text(replacement));
        }
    }

    pub fn to_assistant_item(&self) -> AiItem {
        let mut blocks = Vec::new();
        let mut tool_calls = Vec::new();
        for item in &self.items {
            if item.role != crate::protocol::ir::Role::Assistant {
                continue;
            }
            match &item.content {
                crate::protocol::ir::MessageContent::Text(text) if !text.is_empty() => {
                    blocks.push(crate::protocol::ir::ContentBlock::Text {
                        text: text.clone(),
                        cache_control: None,
                    });
                }
                crate::protocol::ir::MessageContent::Blocks(item_blocks) => {
                    blocks.extend(item_blocks.iter().cloned());
                }
                crate::protocol::ir::MessageContent::Text(_) => {}
            }
            for call in item.tool_calls.iter().flatten() {
                if !blocks.iter().any(|block| {
                    matches!(
                        block,
                        crate::protocol::ir::ContentBlock::ToolUse { id, .. }
                            if id == &call.id
                    )
                }) {
                    blocks.push(crate::protocol::ir::ContentBlock::ToolUse {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: serde_json::from_str(&call.arguments)
                            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
                        cache_control: None,
                    });
                }
                tool_calls.push(call.clone());
            }
        }
        AiItem {
            role: crate::protocol::ir::Role::Assistant,
            content: crate::protocol::ir::MessageContent::Blocks(blocks),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
            meta: None,
        }
    }
}

enum EitherText<A, B, C> {
    One(A),
    Many(B),
    None(C),
}

impl<'a, A, B, C> Iterator for EitherText<A, B, C>
where
    A: Iterator<Item = &'a str>,
    B: Iterator<Item = &'a str>,
    C: Iterator<Item = &'a str>,
{
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::One(iterator) => iterator.next(),
            Self::Many(iterator) => iterator.next(),
            Self::None(iterator) => iterator.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{ContentBlock, MessageContent, ToolCall};

    #[test]
    fn assistant_item_keeps_tool_calls_in_client_visible_block_order() {
        let mut response = AiResponse::new("resp_1", "logical-model");
        response.items.push(AiItem::output_text("planning"));
        response.items.push(AiItem::function_call(ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: serde_json::json!({"query": "weather"}).to_string(),
        }));

        let item = response.to_assistant_item();

        let MessageContent::Blocks(blocks) = item.content else {
            panic!("assistant item must use content blocks");
        };
        assert!(matches!(
            blocks.as_slice(),
            [
                ContentBlock::Text { text, .. },
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    ..
                }
            ] if text == "planning"
                && id == "call_1"
                && name == "lookup"
                && input == &serde_json::json!({"query": "weather"})
        ));
        assert_eq!(item.tool_calls.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn replacing_text_keeps_the_existing_item_metadata_and_tool_calls() {
        let mut item = AiItem::output_text("before");
        item.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        }]);
        item.meta = Some(serde_json::json!({
            "__open_responses_content": [{
                "type": "output_text",
                "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                "logprobs": [{"token": "before", "logprob": -0.1}]
            }]
        }));
        let mut response = AiResponse::new("resp_1", "logical-model");
        response.items.push(item);

        response.replace_output_text("after");

        assert_eq!(response.items.len(), 1);
        assert!(matches!(
            &response.items[0].content,
            MessageContent::Text(text) if text == "after"
        ));
        assert_eq!(response.items[0].tool_calls.as_ref().map(Vec::len), Some(1));
        assert!(response.items[0].meta.is_some());
    }

    #[test]
    fn replacing_text_updates_every_text_block_without_removing_other_content() {
        let mut item = AiItem::output_text("unused");
        item.content = MessageContent::Blocks(vec![
            crate::protocol::ir::ContentBlock::Text {
                text: "before one".into(),
                cache_control: None,
            },
            crate::protocol::ir::ContentBlock::Refusal {
                refusal: "cannot comply".into(),
            },
            crate::protocol::ir::ContentBlock::Text {
                text: "before two".into(),
                cache_control: None,
            },
        ]);
        let mut response = AiResponse::new("resp_1", "logical-model");
        response.items.push(item);

        response.replace_output_text("after");

        let MessageContent::Blocks(blocks) = &response.items[0].content else {
            panic!("mixed content blocks");
        };
        assert!(matches!(
            blocks.as_slice(),
            [
                crate::protocol::ir::ContentBlock::Text { text: first, .. },
                crate::protocol::ir::ContentBlock::Refusal { refusal },
                crate::protocol::ir::ContentBlock::Text { text: second, .. },
            ] if first == "after" && refusal == "cannot comply" && second.is_empty()
        ));
    }
}

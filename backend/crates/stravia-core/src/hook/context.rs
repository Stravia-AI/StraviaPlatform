use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::ir::{
    AiItem, AiRequest, CacheControl, ContentBlock, MessageContent, ProtocolExt, Role, ToolCall,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextItemId(String);

impl ContextItemId {
    pub fn new() -> Self {
        Self(format!("ctx-{}", uuid::Uuid::new_v4()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn original(canonical: &[u8], occurrence: usize) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"stravia-context-item-v1\0");
        hasher.update(canonical);
        hasher.update((occurrence as u64).to_be_bytes());
        Self(format!("ctx-{}", hex_digest(hasher.finalize())))
    }
}

impl Default for ContextItemId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextCompleteness {
    Full,
    Partial { opaque_refs: Vec<OpaqueContextRef> },
}

impl ContextCompleteness {
    pub fn from_request(request: &AiRequest) -> Self {
        let mut opaque_refs = Vec::new();
        match request.ext.as_ref() {
            Some(ProtocolExt::Google(extension)) => {
                if let Some(value) = &extension.cached_content {
                    opaque_refs.push(OpaqueContextRef {
                        namespace: "google.cached_content".into(),
                        value: value.clone(),
                    });
                }
            }
            Some(ProtocolExt::Anthropic(extension)) => {
                if let Some(container) = &extension.container {
                    opaque_refs.push(OpaqueContextRef {
                        namespace: "anthropic.container".into(),
                        value: container.to_string(),
                    });
                }
            }
            _ => {}
        }
        if opaque_refs.is_empty() {
            Self::Full
        } else {
            Self::Partial { opaque_refs }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueContextRef {
    pub namespace: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextItem {
    Message {
        id: ContextItemId,
        message: AiItem,
    },
    Reasoning {
        id: ContextItemId,
        role: Role,
        text: String,
        signature: Option<String>,
        meta: Option<serde_json::Value>,
    },
    ToolCall {
        id: ContextItemId,
        role: Role,
        call: ToolCall,
        cache_control: Option<CacheControl>,
        meta: Option<serde_json::Value>,
    },
    ToolResult {
        id: ContextItemId,
        role: Role,
        tool_use_id: String,
        content: serde_json::Value,
        is_error: Option<bool>,
        cache_control: Option<CacheControl>,
        meta: Option<serde_json::Value>,
    },
}

impl ContextItem {
    pub fn message(message: AiItem) -> Self {
        Self::Message {
            id: ContextItemId::new(),
            message,
        }
    }
    pub fn id(&self) -> &ContextItemId {
        match self {
            Self::Message { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ToolResult { id, .. } => id,
        }
    }

    fn id_mut(&mut self) -> &mut ContextItemId {
        match self {
            Self::Message { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ToolResult { id, .. } => id,
        }
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = match self {
            Self::Message { message, .. } => serde_json::json!({
                "type": "message",
                "message": message,
            }),
            Self::Reasoning {
                role,
                text,
                signature,
                meta,
                ..
            } => serde_json::json!({
                "type": "reasoning",
                "role": role,
                "text": text,
                "signature": signature,
                "meta": meta,
            }),
            Self::ToolCall {
                role,
                call,
                cache_control,
                meta,
                ..
            } => serde_json::json!({
                "type": "tool_call",
                "role": role,
                "call": call,
                "cache_control": cache_control,
                "meta": meta,
            }),
            Self::ToolResult {
                role,
                tool_use_id,
                content,
                is_error,
                cache_control,
                meta,
                ..
            } => serde_json::json!({
                "type": "tool_result",
                "role": role,
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
                "cache_control": cache_control,
                "meta": meta,
            }),
        };
        serde_json::to_vec(&canonical).expect("ContextItem serialization is infallible")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCheckpoint {
    pub after: Option<ContextItemId>,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub system: Option<String>,
    pub items: Vec<ContextItem>,
    pub completeness: ContextCompleteness,
    pub checkpoints: Vec<ContextCheckpoint>,
}

#[derive(Debug, Clone)]
pub struct ReplaceContextSpan {
    pub start: ContextItemId,
    pub end: ContextItemId,
    pub replacement: Vec<ContextItem>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContextPatchError {
    #[error("context item not found: {0}")]
    ItemNotFound(String),
    #[error("context span start follows end")]
    ReversedSpan,
    #[error("context replacement spans overlap")]
    OverlappingSpans,
    #[error("replacement contains duplicate context item id")]
    DuplicateReplacementId,
}

fn normalized_context_items(messages: &[AiItem]) -> Vec<ContextItem> {
    let mut items = Vec::new();
    for message in messages {
        let first_item = items.len();
        let mut message_meta = message.meta.clone();
        let mut seen_tool_calls = HashSet::new();
        match &message.content {
            MessageContent::Text(text)
                if message.role == Role::Tool && message.tool_call_id.is_some() =>
            {
                items.push(ContextItem::ToolResult {
                    id: ContextItemId::new(),
                    role: message.role,
                    tool_use_id: message.tool_call_id.clone().unwrap_or_default(),
                    content: serde_json::Value::String(text.clone()),
                    is_error: None,
                    cache_control: None,
                    meta: message_meta.take(),
                });
            }
            MessageContent::Text(text) => {
                if !text.is_empty() || message.tool_calls.as_ref().is_none_or(Vec::is_empty) {
                    let mut plain = message.clone();
                    plain.tool_calls = None;
                    plain.meta = message_meta.take();
                    items.push(ContextItem::Message {
                        id: ContextItemId::new(),
                        message: plain,
                    });
                }
            }
            MessageContent::Blocks(blocks) => {
                let mut residual = Vec::new();
                for block in blocks {
                    let semantic = match block {
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => Some(ContextItem::Reasoning {
                            id: ContextItemId::new(),
                            role: message.role,
                            text: thinking.clone(),
                            signature: signature.clone(),
                            meta: None,
                        }),
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            cache_control,
                        } => {
                            seen_tool_calls.insert(id.clone());
                            Some(ContextItem::ToolCall {
                                id: ContextItemId::new(),
                                role: message.role,
                                call: ToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input)
                                        .expect("JSON value serialization is infallible"),
                                },
                                cache_control: cache_control.clone(),
                                meta: None,
                            })
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            cache_control,
                        } => Some(ContextItem::ToolResult {
                            id: ContextItemId::new(),
                            role: message.role,
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: *is_error,
                            cache_control: cache_control.clone(),
                            meta: None,
                        }),
                        _ => None,
                    };
                    if let Some(mut semantic) = semantic {
                        push_message_segment(
                            &mut items,
                            message,
                            std::mem::take(&mut residual),
                            &mut message_meta,
                        );
                        match &mut semantic {
                            ContextItem::Reasoning { meta, .. }
                            | ContextItem::ToolCall { meta, .. }
                            | ContextItem::ToolResult { meta, .. } => {
                                *meta = message_meta.take();
                            }
                            ContextItem::Message { .. } => unreachable!(),
                        }
                        items.push(semantic);
                    } else {
                        residual.push(block.clone());
                    }
                }
                push_message_segment(&mut items, message, residual, &mut message_meta);
            }
        }
        for call in message.tool_calls.iter().flatten() {
            if seen_tool_calls.insert(call.id.clone()) {
                items.push(ContextItem::ToolCall {
                    id: ContextItemId::new(),
                    role: message.role,
                    call: call.clone(),
                    cache_control: None,
                    meta: message_meta.take(),
                });
            }
        }
        if items.len() == first_item {
            items.push(ContextItem::Message {
                id: ContextItemId::new(),
                message: message.clone(),
            });
        }
    }
    items
}

fn push_message_segment(
    items: &mut Vec<ContextItem>,
    source: &AiItem,
    blocks: Vec<ContentBlock>,
    message_meta: &mut Option<serde_json::Value>,
) {
    if blocks.is_empty() {
        return;
    }
    let mut message = source.clone();
    message.content = MessageContent::Blocks(blocks);
    message.tool_calls = None;
    message.meta = message_meta.take();
    items.push(ContextItem::Message {
        id: ContextItemId::new(),
        message,
    });
}

fn assign_original_ids(items: &mut [ContextItem], previous: &[ContextItem]) {
    let mut previous_ids: HashMap<Vec<u8>, Vec<ContextItemId>> = HashMap::new();
    for item in previous {
        previous_ids
            .entry(item.canonical_bytes())
            .or_default()
            .push(item.id().clone());
    }
    let mut occurrences: HashMap<Vec<u8>, usize> = HashMap::new();
    for item in items {
        let canonical = item.canonical_bytes();
        let occurrence = occurrences.entry(canonical.clone()).or_default();
        let id = previous_ids
            .get(&canonical)
            .and_then(|ids| ids.get(*occurrence))
            .cloned()
            .unwrap_or_else(|| ContextItemId::original(&canonical, *occurrence));
        *occurrence += 1;
        *item.id_mut() = id;
    }
}

impl ContextSnapshot {
    pub fn from_request(request: &AiRequest, completeness: ContextCompleteness) -> Self {
        let mut items = normalized_context_items(&request.items);
        assign_original_ids(&mut items, &[]);
        let mut snapshot = Self {
            system: request.instructions.clone(),
            items,
            completeness,
            checkpoints: Vec::new(),
        };
        snapshot.rebuild_checkpoints();
        snapshot
    }

    pub fn checkpoint_after(&self, id: &ContextItemId) -> Option<&ContextCheckpoint> {
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.after.as_ref() == Some(id))
    }

    pub fn fingerprint(&self) -> String {
        self.checkpoints
            .last()
            .map(|checkpoint| checkpoint.digest.clone())
            .unwrap_or_else(|| initial_digest(self.system.as_deref()))
    }

    pub fn span_fingerprint(
        &self,
        start: &ContextItemId,
        end: &ContextItemId,
    ) -> Result<String, ContextPatchError> {
        let (start, end) = self.resolve_span(start, end)?;
        let mut hasher = Sha256::new();
        hasher.update(b"stravia-context-span-v1\0");
        for item in &self.items[start..=end] {
            hasher.update((item.canonical_bytes().len() as u64).to_be_bytes());
            hasher.update(item.canonical_bytes());
        }
        Ok(hex_digest(hasher.finalize()))
    }

    pub fn apply_rewrites(
        &mut self,
        rewrites: &[ReplaceContextSpan],
    ) -> Result<(), ContextPatchError> {
        if rewrites.is_empty() {
            return Ok(());
        }

        let mut resolved = Vec::with_capacity(rewrites.len());
        for rewrite in rewrites {
            let (start, end) = self.resolve_span(&rewrite.start, &rewrite.end)?;
            resolved.push((start, end, rewrite));
        }
        resolved.sort_by_key(|(start, _, _)| *start);
        if resolved.windows(2).any(|pair| pair[1].0 <= pair[0].1) {
            return Err(ContextPatchError::OverlappingSpans);
        }

        let original_ids: HashSet<ContextItemId> =
            self.items.iter().map(|item| item.id().clone()).collect();
        let mut replacement_ids = HashSet::new();
        for (_, _, rewrite) in &resolved {
            for item in &rewrite.replacement {
                if original_ids.contains(item.id()) || !replacement_ids.insert(item.id().clone()) {
                    return Err(ContextPatchError::DuplicateReplacementId);
                }
            }
        }

        let mut next = self.items.clone();
        for (start, end, rewrite) in resolved.into_iter().rev() {
            next.splice(start..=end, rewrite.replacement.clone());
        }
        self.items = next;
        self.rebuild_checkpoints();
        Ok(())
    }

    pub fn update_from_request(&mut self, request: &AiRequest, completeness: ContextCompleteness) {
        let previous = std::mem::take(&mut self.items);
        let mut items = normalized_context_items(&request.items);
        assign_original_ids(&mut items, &previous);
        self.system = request.instructions.clone();
        self.items = items;
        self.completeness = completeness;
        self.rebuild_checkpoints();
    }

    pub fn write_to_request(&self, request: &mut AiRequest) {
        request.instructions = self.system.clone();
        request.items = self
            .items
            .iter()
            .map(|item| match item {
                ContextItem::Message { message, .. } => message.clone(),
                ContextItem::Reasoning {
                    role,
                    text,
                    signature,
                    meta,
                    ..
                } => AiItem {
                    role: *role,
                    content: MessageContent::Blocks(vec![ContentBlock::Thinking {
                        thinking: text.clone(),
                        signature: signature.clone(),
                    }]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: meta.clone(),
                },
                ContextItem::ToolCall {
                    role,
                    call,
                    cache_control,
                    meta,
                    ..
                } => AiItem {
                    role: *role,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: serde_json::from_str(&call.arguments)
                            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
                        cache_control: cache_control.clone(),
                    }]),
                    tool_calls: Some(vec![call.clone()]),
                    tool_call_id: None,
                    meta: meta.clone(),
                },
                ContextItem::ToolResult {
                    role,
                    tool_use_id,
                    content,
                    is_error,
                    cache_control,
                    meta,
                    ..
                } => AiItem {
                    role: *role,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                        cache_control: cache_control.clone(),
                    }]),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.clone()),
                    meta: meta.clone(),
                },
            })
            .collect();
    }

    fn resolve_span(
        &self,
        start: &ContextItemId,
        end: &ContextItemId,
    ) -> Result<(usize, usize), ContextPatchError> {
        let start_index = self
            .items
            .iter()
            .position(|item| item.id() == start)
            .ok_or_else(|| ContextPatchError::ItemNotFound(start.as_str().to_string()))?;
        let end_index = self
            .items
            .iter()
            .position(|item| item.id() == end)
            .ok_or_else(|| ContextPatchError::ItemNotFound(end.as_str().to_string()))?;
        if start_index > end_index {
            return Err(ContextPatchError::ReversedSpan);
        }
        Ok((start_index, end_index))
    }

    fn rebuild_checkpoints(&mut self) {
        let mut digest = initial_digest(self.system.as_deref());
        self.checkpoints = self
            .items
            .iter()
            .map(|item| {
                let canonical = item.canonical_bytes();
                let mut hasher = Sha256::new();
                hasher.update(b"stravia-context-checkpoint-v1\0");
                hasher.update(digest.as_bytes());
                hasher.update((canonical.len() as u64).to_be_bytes());
                hasher.update(canonical);
                digest = hex_digest(hasher.finalize());
                ContextCheckpoint {
                    after: Some(item.id().clone()),
                    digest: digest.clone(),
                }
            })
            .collect();
    }
}

fn initial_digest(system: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"stravia-context-checkpoint-v1\0");
    match system {
        None => hasher.update([0_u8]),
        Some(system) => {
            hasher.update([1_u8]);
            hasher.update((system.len() as u64).to_be_bytes());
            hasher.update(system.as_bytes());
        }
    }
    hex_digest(hasher.finalize())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;

    bytes
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String is infallible");
            output
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{MessageContent, Role};

    fn message(text: &str) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    fn request(count: usize) -> AiRequest {
        AiRequest::new(
            "model",
            (1..=count)
                .map(|index| message(&format!("m{index}")))
                .collect(),
        )
    }

    #[test]
    fn checkpoint_is_stable_when_only_the_suffix_grows() {
        let short = ContextSnapshot::from_request(&request(8), ContextCompleteness::Full);
        let long = ContextSnapshot::from_request(&request(10), ContextCompleteness::Full);

        let short_m7 = short.items[6].id();
        let long_m7 = long.items[6].id();
        assert_eq!(short_m7, long_m7);
        assert_eq!(
            short.checkpoint_after(short_m7).unwrap().digest,
            long.checkpoint_after(long_m7).unwrap().digest
        );
    }

    #[test]
    fn replacement_is_atomic_and_rejects_overlapping_spans() {
        let mut snapshot = ContextSnapshot::from_request(&request(10), ContextCompleteness::Full);
        let original_ids: Vec<_> = snapshot
            .items
            .iter()
            .map(|item| item.id().clone())
            .collect();
        let replacement = ContextSnapshot::from_request(
            &AiRequest::new("model", vec![message("summary")]),
            ContextCompleteness::Full,
        )
        .items;

        let result = snapshot.apply_rewrites(&[
            ReplaceContextSpan {
                start: original_ids[2].clone(),
                end: original_ids[6].clone(),
                replacement: replacement.clone(),
            },
            ReplaceContextSpan {
                start: original_ids[5].clone(),
                end: original_ids[7].clone(),
                replacement,
            },
        ]);

        assert_eq!(result, Err(ContextPatchError::OverlappingSpans));
        assert_eq!(
            snapshot
                .items
                .iter()
                .map(|item| item.id())
                .collect::<Vec<_>>(),
            original_ids.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn replacement_requires_the_complete_span() {
        let mut short = ContextSnapshot::from_request(&request(6), ContextCompleteness::Full);
        let long = ContextSnapshot::from_request(&request(10), ContextCompleteness::Full);
        let result = short.apply_rewrites(&[ReplaceContextSpan {
            start: long.items[2].id().clone(),
            end: long.items[6].id().clone(),
            replacement: Vec::new(),
        }]);

        assert_eq!(
            result,
            Err(ContextPatchError::ItemNotFound(
                long.items[6].id().as_str().to_string()
            ))
        );
    }

    #[test]
    fn replacement_cannot_reuse_an_existing_item_id() {
        let mut snapshot = ContextSnapshot::from_request(&request(2), ContextCompleteness::Full);
        let old_id = snapshot.items[0].id().clone();
        let replacement = snapshot.items[1].clone();
        let result = snapshot.apply_rewrites(&[ReplaceContextSpan {
            start: old_id.clone(),
            end: old_id,
            replacement: vec![replacement],
        }]);

        assert_eq!(result, Err(ContextPatchError::DuplicateReplacementId));
    }

    #[test]
    fn empty_and_missing_system_have_distinct_fingerprints() {
        let mut empty = request(1);
        empty.instructions = Some(String::new());
        let missing = request(1);

        assert_ne!(
            ContextSnapshot::from_request(&empty, ContextCompleteness::Full).fingerprint(),
            ContextSnapshot::from_request(&missing, ContextCompleteness::Full).fingerprint()
        );
    }
    #[test]
    fn update_reuses_ids_by_canonical_occurrence_after_insertion() {
        let original_request = request(3);
        let mut snapshot =
            ContextSnapshot::from_request(&original_request, ContextCompleteness::Full);
        let original_ids: Vec<_> = snapshot
            .items
            .iter()
            .map(|item| item.id().clone())
            .collect();

        let mut inserted_messages = vec![message("inserted")];
        inserted_messages.extend(original_request.items.clone());
        let updated = AiRequest::new("model", inserted_messages);
        snapshot.update_from_request(&updated, ContextCompleteness::Full);

        assert_eq!(snapshot.items[1].id(), &original_ids[0]);
        assert_eq!(snapshot.items[2].id(), &original_ids[1]);
        assert_eq!(snapshot.items[3].id(), &original_ids[2]);
    }
    #[test]
    fn semantic_history_uses_typed_items_and_round_trips() {
        let call = ToolCall {
            id: "call-1".into(),
            name: "lookup".into(),
            arguments: r#"{"q":"rust"}"#.into(),
        };
        let assistant = AiItem {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Thinking {
                    thinking: "plan".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::Text {
                    text: "checking".into(),
                    cache_control: None,
                },
                ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: serde_json::json!({"q": "rust"}),
                    cache_control: None,
                },
            ]),
            tool_calls: Some(vec![call]),
            tool_call_id: None,
            meta: Some(serde_json::json!({"source": "provider"})),
        };
        let tool_result = AiItem {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".into(),
                content: serde_json::json!({"answer": 42}),
                is_error: Some(false),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call-1".into()),
            meta: None,
        };
        let request = AiRequest::new("model", vec![assistant, tool_result]);
        let snapshot = ContextSnapshot::from_request(&request, ContextCompleteness::Full);

        assert!(matches!(snapshot.items[0], ContextItem::Reasoning { .. }));
        assert!(matches!(snapshot.items[1], ContextItem::Message { .. }));
        assert!(matches!(snapshot.items[2], ContextItem::ToolCall { .. }));
        assert!(matches!(snapshot.items[3], ContextItem::ToolResult { .. }));

        let mut rebuilt = AiRequest::new("model", Vec::new());
        snapshot.write_to_request(&mut rebuilt);
        let rebuilt_snapshot = ContextSnapshot::from_request(&rebuilt, ContextCompleteness::Full);
        assert_eq!(
            snapshot
                .items
                .iter()
                .map(ContextItem::canonical_bytes)
                .collect::<Vec<_>>(),
            rebuilt_snapshot
                .items
                .iter()
                .map(ContextItem::canonical_bytes)
                .collect::<Vec<_>>()
        );
    }
}

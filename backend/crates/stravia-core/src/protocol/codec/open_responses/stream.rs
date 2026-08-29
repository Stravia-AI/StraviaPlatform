use std::collections::{BTreeMap, HashMap};

use uuid::Uuid;

use super::formatter::{gateway_item_id, response_resource_snapshot};
use crate::protocol::SseEvent;
use crate::protocol::ir::usage::Usage;
use crate::protocol::ir::{AiItemStatus, AiStreamDelta};

struct PendingFunctionCall {
    output_index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    status: Option<AiItemStatus>,
}

struct PendingUnknownItem {
    output_index: usize,
    item: serde_json::Value,
    done: bool,
}
struct PendingIndexedMessage {
    item_id: String,
    content: BTreeMap<usize, String>,
    refusals: BTreeMap<usize, String>,
    status: Option<AiItemStatus>,
}
struct PendingIndexedReasoning {
    item_id: String,
    summary: BTreeMap<usize, String>,
    content: BTreeMap<usize, String>,
    encrypted_content: Option<String>,
}

pub struct ResponsesStreamFormatter {
    resp_id: String,
    msg_id: String,
    message_output_index: Option<usize>,
    model: String,
    accumulated_text: String,
    accumulated_refusal: String,
    text_content_index: Option<usize>,
    refusal_content_index: Option<usize>,
    next_message_content_index: usize,
    accumulated_reasoning: String,
    accumulated_reasoning_content: String,
    reasoning_encrypted_content: Option<String>,
    reasoning_summary_started: bool,
    usage: Usage,
    started: bool,
    completed: bool,
    failed: bool,
    next_output_index: usize,
    next_sequence_number: u64,
    reasoning_item_id: Option<String>,
    reasoning_output_index: Option<usize>,
    tool_index_map: HashMap<usize, usize>,
    tool_calls: Vec<PendingFunctionCall>,
    unknown_items: Vec<PendingUnknownItem>,
    completed_message_content: HashMap<usize, Vec<serde_json::Value>>,
    indexed_reasoning: BTreeMap<usize, PendingIndexedReasoning>,
    indexed_messages: BTreeMap<usize, PendingIndexedMessage>,
    indexed_function_outputs: BTreeMap<usize, serde_json::Value>,
    pending_annotations: Vec<serde_json::Value>,
    response_profile: serde_json::Map<String, serde_json::Value>,
}

impl Default for ResponsesStreamFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesStreamFormatter {
    pub fn new() -> Self {
        Self {
            resp_id: format!("resp_{}", Uuid::new_v4().simple()),
            msg_id: String::new(),
            message_output_index: None,
            model: String::new(),
            accumulated_text: String::new(),
            accumulated_refusal: String::new(),
            text_content_index: None,
            refusal_content_index: None,
            next_message_content_index: 0,
            accumulated_reasoning: String::new(),
            accumulated_reasoning_content: String::new(),
            reasoning_encrypted_content: None,
            reasoning_summary_started: false,
            usage: Usage::default(),
            started: false,
            completed: false,
            failed: false,
            next_output_index: 0,
            next_sequence_number: 0,
            reasoning_item_id: None,
            reasoning_output_index: None,
            tool_index_map: HashMap::new(),
            tool_calls: Vec::new(),
            unknown_items: Vec::new(),
            completed_message_content: HashMap::new(),
            indexed_reasoning: BTreeMap::new(),
            indexed_messages: BTreeMap::new(),
            indexed_function_outputs: BTreeMap::new(),
            pending_annotations: Vec::new(),
            response_profile: serde_json::Map::new(),
        }
    }

    pub(crate) fn set_response_profile_from_request(
        &mut self,
        request: &crate::protocol::ir::AiRequest,
        previous_response_id: Option<&str>,
    ) {
        if let serde_json::Value::Object(mut profile) =
            super::encoder::response_profile_from_request(request)
        {
            if let Some(previous_response_id) = previous_response_id {
                profile.insert(
                    "previous_response_id".into(),
                    serde_json::Value::String(previous_response_id.to_owned()),
                );
            }
            self.response_profile.extend(profile);
        }
    }

    fn apply_response_profile(&self, response: &mut serde_json::Value) {
        if let Some(object) = response.as_object_mut() {
            object.extend(self.response_profile.clone());
        }
    }

    fn ensure_started(&mut self, events: &mut Vec<SseEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        events.extend(self.emit_preamble());
    }

    fn finalize_events(&mut self, events: &mut [SseEvent]) {
        for event in events {
            let Ok(mut body) = serde_json::from_str::<serde_json::Value>(&event.data) else {
                continue;
            };
            let Some(object) = body.as_object_mut() else {
                continue;
            };
            let Some(event_type) = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            object.insert(
                "sequence_number".into(),
                serde_json::Value::from(self.next_sequence_number),
            );
            self.next_sequence_number += 1;
            event.event = Some(event_type);
            event.data = body.to_string();
        }
    }

    fn emit_preamble(&mut self) -> Vec<SseEvent> {
        let model = if self.model.is_empty() {
            "unknown"
        } else {
            self.model.as_str()
        };
        let mut response = response_resource_snapshot(
            &self.resp_id,
            model,
            "in_progress",
            Vec::new(),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        );
        self.apply_response_profile(&mut response);
        vec![
            SseEvent::new(
                Some("response.created"),
                serde_json::json!({
                    "type": "response.created",
                    "response": response.clone(),
                })
                .to_string(),
            ),
            SseEvent::new(
                Some("response.in_progress"),
                serde_json::json!({
                    "type": "response.in_progress",
                    "response": response,
                })
                .to_string(),
            ),
        ]
    }

    fn ensure_message_started(&mut self, events: &mut Vec<SseEvent>) {
        self.ensure_started(events);
        if self.message_output_index.is_some() {
            return;
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.message_output_index = Some(output_index);
        self.msg_id = gateway_item_id("msg", &self.resp_id, output_index);
        events.push(SseEvent::new(
            Some("response.output_item.added"),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "message",
                    "id": self.msg_id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            })
            .to_string(),
        ));
    }

    fn ensure_text_started(&mut self, events: &mut Vec<SseEvent>) {
        self.ensure_message_started(events);
        if self.text_content_index.is_some() {
            return;
        }
        let content_index = self.next_message_content_index;
        self.next_message_content_index += 1;
        self.text_content_index = Some(content_index);
        events.push(SseEvent::new(
            Some("response.content_part.added"),
            serde_json::json!({
                "type": "response.content_part.added",
                "item_id": self.msg_id,
                "output_index": self.message_output_index.expect("message started"),
                "content_index": content_index,
                "part": {
                    "type": "output_text",
                    "text": "",
                    "annotations": []
                }
            })
            .to_string(),
        ));
    }

    fn ensure_refusal_started(&mut self, events: &mut Vec<SseEvent>) {
        self.ensure_message_started(events);
        if self.refusal_content_index.is_some() {
            return;
        }
        let content_index = self.next_message_content_index;
        self.next_message_content_index += 1;
        self.refusal_content_index = Some(content_index);
        events.push(SseEvent::new(
            Some("response.content_part.added"),
            serde_json::json!({
                "type": "response.content_part.added",
                "item_id": self.msg_id,
                "output_index": self.message_output_index.expect("message started"),
                "content_index": content_index,
                "part": {
                    "type": "refusal",
                    "refusal": ""
                }
            })
            .to_string(),
        ));
    }

    fn ensure_reasoning_started(&mut self, events: &mut Vec<SseEvent>) {
        self.ensure_started(events);
        if self.reasoning_item_id.is_some() {
            return;
        }
        let item_id = gateway_item_id("rs", &self.resp_id, self.next_output_index);
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.reasoning_item_id = Some(item_id.clone());
        self.reasoning_output_index = Some(output_index);
        events.push(SseEvent::new(
            Some("response.output_item.added"),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [],
                    "content": []
                }
            })
            .to_string(),
        ));
    }

    fn reasoning_item(&self) -> serde_json::Value {
        let summary = if self.accumulated_reasoning.is_empty() {
            Vec::new()
        } else {
            vec![serde_json::json!({
                "type": "summary_text",
                "text": self.accumulated_reasoning
            })]
        };
        let mut item = serde_json::json!({
            "type": "reasoning",
            "id": self.reasoning_item_id,
            "summary": summary
        });
        if !self.accumulated_reasoning_content.is_empty() {
            item["content"] = serde_json::json!([{
                "type": "reasoning_text",
                "text": self.accumulated_reasoning_content
            }]);
        }
        if let Some(encrypted_content) = &self.reasoning_encrypted_content {
            item["encrypted_content"] = serde_json::Value::String(encrypted_content.clone());
        }
        item
    }
    fn emit_reasoning_delta(
        &mut self,
        events: &mut Vec<SseEvent>,
        text: &str,
        obfuscation: Option<&str>,
    ) {
        self.ensure_reasoning_started(events);
        self.accumulated_reasoning_content.push_str(text);
        let mut event = serde_json::json!({
            "type": "response.reasoning.delta",
            "item_id": self.reasoning_item_id,
            "output_index": self.reasoning_output_index,
            "content_index": 0,
            "delta": text
        });
        if let Some(obfuscation) = obfuscation {
            event["obfuscation"] = serde_json::Value::String(obfuscation.to_owned());
        }
        events.push(SseEvent::new(
            Some("response.reasoning.delta"),
            event.to_string(),
        ));
    }

    fn emit_reasoning_summary_delta(
        &mut self,
        events: &mut Vec<SseEvent>,
        text: &str,
        obfuscation: Option<&str>,
    ) {
        self.ensure_reasoning_started(events);
        if !self.reasoning_summary_started {
            self.reasoning_summary_started = true;
            events.push(SseEvent::new(
                Some("response.reasoning_summary_part.added"),
                serde_json::json!({
                    "type": "response.reasoning_summary_part.added",
                    "item_id": self.reasoning_item_id,
                    "output_index": self.reasoning_output_index,
                    "summary_index": 0,
                    "part": {
                        "type": "summary_text",
                        "text": ""
                    }
                })
                .to_string(),
            ));
        }
        self.accumulated_reasoning.push_str(text);
        let mut event = serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": self.reasoning_item_id,
            "output_index": self.reasoning_output_index,
            "summary_index": 0,
            "delta": text
        });
        if let Some(obfuscation) = obfuscation {
            event["obfuscation"] = serde_json::Value::String(obfuscation.to_owned());
        }
        events.push(SseEvent::new(
            Some("response.reasoning_summary_text.delta"),
            event.to_string(),
        ));
    }

    fn emit_text_delta(
        &mut self,
        events: &mut Vec<SseEvent>,
        text: &str,
        metadata: Option<(&[serde_json::Value], Option<&str>)>,
    ) {
        self.ensure_text_started(events);
        self.accumulated_text.push_str(text);
        let mut event = serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": self.msg_id,
            "output_index": self.message_output_index.expect("started message"),
            "content_index": self.text_content_index.expect("started text"),
            "delta": text
        });
        if let Some((logprobs, obfuscation)) = metadata {
            event["logprobs"] = serde_json::Value::Array(logprobs.to_vec());
            if let Some(obfuscation) = obfuscation {
                event["obfuscation"] = serde_json::Value::String(obfuscation.to_owned());
            }
        }
        events.push(SseEvent::new(
            Some("response.output_text.delta"),
            event.to_string(),
        ));
    }

    fn terminal_content_part(
        &self,
        output_index: usize,
        content_index: usize,
        fallback: serde_json::Value,
    ) -> serde_json::Value {
        let Some(original) = self
            .completed_message_content
            .get(&output_index)
            .and_then(|content| content.get(content_index))
            .filter(|original| original.get("type") == fallback.get("type"))
        else {
            return fallback;
        };
        let mut preserved = original.clone();
        match fallback.get("type").and_then(serde_json::Value::as_str) {
            Some("output_text") => {
                if preserved.get("text") != fallback.get("text") {
                    preserved["annotations"] = serde_json::Value::Array(Vec::new());
                    preserved["logprobs"] = serde_json::Value::Array(Vec::new());
                }
                preserved["text"] = fallback["text"].clone();
            }
            Some("refusal") => preserved["refusal"] = fallback["refusal"].clone(),
            _ => return fallback,
        }
        preserved
    }

    fn flush_pending_annotations(&mut self, events: &mut Vec<SseEvent>, output_index: usize) {
        let mut remaining = Vec::new();
        for event in std::mem::take(&mut self.pending_annotations) {
            let event_output_index = event
                .get("output_index")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok());
            if event_output_index != Some(output_index) {
                remaining.push(event);
                continue;
            }
            let content_index = event
                .get("content_index")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok());
            let original_text = content_index.and_then(|content_index| {
                self.completed_message_content
                    .get(&output_index)
                    .and_then(|content| content.get(content_index))
                    .and_then(|part| part.get("text"))
                    .and_then(serde_json::Value::as_str)
            });
            let current_text = content_index.and_then(|content_index| {
                self.indexed_messages
                    .get(&output_index)
                    .and_then(|message| message.content.get(&content_index))
                    .map(String::as_str)
                    .or_else(|| {
                        (self.message_output_index == Some(output_index)
                            && self.text_content_index == Some(content_index))
                        .then_some(self.accumulated_text.as_str())
                    })
            });
            if original_text.is_some() && original_text == current_text {
                events.push(SseEvent::new(
                    Some("response.output_text.annotation.added"),
                    event.to_string(),
                ));
            }
        }
        self.pending_annotations = remaining;
    }

    fn ensure_indexed_reasoning(
        &mut self,
        events: &mut Vec<SseEvent>,
        output_index: usize,
        preferred_item_id: Option<&str>,
    ) {
        self.ensure_started(events);
        self.next_output_index = self.next_output_index.max(output_index + 1);
        if self.indexed_reasoning.contains_key(&output_index) {
            return;
        }
        let item_id = preferred_item_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| gateway_item_id("rs", &self.resp_id, output_index));
        events.push(SseEvent::new(
            Some("response.output_item.added"),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [],
                    "content": []
                }
            })
            .to_string(),
        ));
        self.indexed_reasoning.insert(
            output_index,
            PendingIndexedReasoning {
                item_id,
                summary: BTreeMap::new(),
                content: BTreeMap::new(),
                encrypted_content: None,
            },
        );
    }

    fn emit_indexed_reasoning_delta(
        &mut self,
        events: &mut Vec<SseEvent>,
        output_index: usize,
        content_index: usize,
        text: &str,
        obfuscation: Option<&str>,
    ) {
        self.ensure_indexed_reasoning(events, output_index, None);
        let reasoning = self
            .indexed_reasoning
            .get_mut(&output_index)
            .expect("indexed reasoning was inserted");
        reasoning
            .content
            .entry(content_index)
            .or_default()
            .push_str(text);
        let mut event = serde_json::json!({
            "type": "response.reasoning.delta",
            "item_id": reasoning.item_id,
            "output_index": output_index,
            "content_index": content_index,
            "delta": text
        });
        if let Some(obfuscation) = obfuscation {
            event["obfuscation"] = serde_json::Value::String(obfuscation.to_owned());
        }
        events.push(SseEvent::new(
            Some("response.reasoning.delta"),
            event.to_string(),
        ));
    }

    fn emit_indexed_reasoning_summary_delta(
        &mut self,
        events: &mut Vec<SseEvent>,
        output_index: usize,
        content_index: usize,
        text: &str,
        obfuscation: Option<&str>,
    ) {
        self.ensure_indexed_reasoning(events, output_index, None);
        let reasoning = self
            .indexed_reasoning
            .get_mut(&output_index)
            .expect("indexed reasoning was inserted");
        if !reasoning.summary.contains_key(&content_index) {
            events.push(SseEvent::new(
                Some("response.reasoning_summary_part.added"),
                serde_json::json!({
                    "type": "response.reasoning_summary_part.added",
                    "item_id": reasoning.item_id,
                    "output_index": output_index,
                    "summary_index": content_index,
                    "part": {"type": "summary_text", "text": ""}
                })
                .to_string(),
            ));
        }
        reasoning
            .summary
            .entry(content_index)
            .or_default()
            .push_str(text);
        let mut event = serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": reasoning.item_id,
            "output_index": output_index,
            "summary_index": content_index,
            "delta": text
        });
        if let Some(obfuscation) = obfuscation {
            event["obfuscation"] = serde_json::Value::String(obfuscation.to_owned());
        }
        events.push(SseEvent::new(
            Some("response.reasoning_summary_text.delta"),
            event.to_string(),
        ));
    }

    fn ensure_indexed_message(
        &mut self,
        events: &mut Vec<SseEvent>,
        output_index: usize,
        preferred_item_id: Option<&str>,
    ) {
        self.ensure_started(events);
        self.next_output_index = self.next_output_index.max(output_index + 1);
        if self.indexed_messages.contains_key(&output_index) {
            return;
        }
        let item_id = preferred_item_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| gateway_item_id("msg", &self.resp_id, output_index));
        events.push(SseEvent::new(
            Some("response.output_item.added"),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "message",
                    "id": item_id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            })
            .to_string(),
        ));
        self.indexed_messages.insert(
            output_index,
            PendingIndexedMessage {
                item_id,
                content: BTreeMap::new(),
                refusals: BTreeMap::new(),
                status: None,
            },
        );
    }

    fn ensure_indexed_content_part(
        &mut self,
        events: &mut Vec<SseEvent>,
        output_index: usize,
        content_index: usize,
        refusal: bool,
    ) {
        self.ensure_indexed_message(events, output_index, None);
        let message = self
            .indexed_messages
            .get(&output_index)
            .expect("indexed message was inserted");
        if message.content.contains_key(&content_index)
            || message.refusals.contains_key(&content_index)
        {
            return;
        }
        let part = if refusal {
            serde_json::json!({"type": "refusal", "refusal": ""})
        } else {
            serde_json::json!({
                "type": "output_text",
                "text": "",
                "annotations": []
            })
        };
        events.push(SseEvent::new(
            Some("response.content_part.added"),
            serde_json::json!({
                "type": "response.content_part.added",
                "item_id": message.item_id,
                "output_index": output_index,
                "content_index": content_index,
                "part": part
            })
            .to_string(),
        ));
        let message = self
            .indexed_messages
            .get_mut(&output_index)
            .expect("indexed message was inserted");
        if refusal {
            message.refusals.insert(content_index, String::new());
        } else {
            message.content.insert(content_index, String::new());
        }
    }

    fn emit_indexed_text_delta(
        &mut self,
        events: &mut Vec<SseEvent>,
        output_index: usize,
        content_index: usize,
        text: &str,
        metadata: Option<(&[serde_json::Value], Option<&str>)>,
    ) {
        self.ensure_indexed_content_part(events, output_index, content_index, false);
        let message = self
            .indexed_messages
            .get_mut(&output_index)
            .expect("indexed message was inserted");
        message
            .content
            .entry(content_index)
            .or_default()
            .push_str(text);
        let (logprobs, obfuscation) = metadata.unwrap_or((&[], None));
        let mut delta = serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": message.item_id,
            "output_index": output_index,
            "content_index": content_index,
            "delta": text,
            "logprobs": logprobs
        });
        if let Some(obfuscation) = obfuscation {
            delta["obfuscation"] = serde_json::Value::String(obfuscation.to_owned());
        }
        events.push(SseEvent::new(
            Some("response.output_text.delta"),
            delta.to_string(),
        ));
    }

    fn emit_indexed_refusal_delta(
        &mut self,
        events: &mut Vec<SseEvent>,
        output_index: usize,
        content_index: usize,
        text: &str,
    ) {
        self.ensure_indexed_content_part(events, output_index, content_index, true);
        let message = self
            .indexed_messages
            .get_mut(&output_index)
            .expect("indexed message was inserted");
        message
            .refusals
            .entry(content_index)
            .or_default()
            .push_str(text);
        events.push(SseEvent::new(
            Some("response.refusal.delta"),
            serde_json::json!({
                "type": "response.refusal.delta",
                "item_id": message.item_id,
                "output_index": output_index,
                "content_index": content_index,
                "delta": text
            })
            .to_string(),
        ));
    }

    fn emit_terminal(
        &mut self,
        status: &str,
        incomplete_details: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        if let (Some(item_id), Some(output_index)) =
            (&self.reasoning_item_id, self.reasoning_output_index)
        {
            if !self.accumulated_reasoning.is_empty() {
                events.push(SseEvent::new(
                    Some("response.reasoning_summary_text.done"),
                    serde_json::json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "summary_index": 0,
                        "text": self.accumulated_reasoning
                    })
                    .to_string(),
                ));
                events.push(SseEvent::new(
                    Some("response.reasoning_summary_part.done"),
                    serde_json::json!({
                        "type": "response.reasoning_summary_part.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "summary_index": 0,
                        "part": {
                            "type": "summary_text",
                            "text": self.accumulated_reasoning
                        }
                    })
                    .to_string(),
                ));
            }
            if !self.accumulated_reasoning_content.is_empty() {
                events.push(SseEvent::new(
                    Some("response.reasoning.done"),
                    serde_json::json!({
                        "type": "response.reasoning.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "text": self.accumulated_reasoning_content
                    })
                    .to_string(),
                ));
            }
            let reasoning_done = serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": self.reasoning_item()
            });
            events.push(SseEvent::new(
                Some("response.output_item.done"),
                reasoning_done.to_string(),
            ));
        }

        let mut indexed_reasoning_output = Vec::new();
        for (output_index, reasoning) in &self.indexed_reasoning {
            let mut summary = Vec::new();
            for (summary_index, text) in &reasoning.summary {
                events.push(SseEvent::new(
                    Some("response.reasoning_summary_text.done"),
                    serde_json::json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": reasoning.item_id,
                        "output_index": output_index,
                        "summary_index": summary_index,
                        "text": text
                    })
                    .to_string(),
                ));
                let part = serde_json::json!({"type": "summary_text", "text": text});
                events.push(SseEvent::new(
                    Some("response.reasoning_summary_part.done"),
                    serde_json::json!({
                        "type": "response.reasoning_summary_part.done",
                        "item_id": reasoning.item_id,
                        "output_index": output_index,
                        "summary_index": summary_index,
                        "part": part
                    })
                    .to_string(),
                ));
                summary.push(part);
            }
            let mut content = Vec::new();
            for (content_index, text) in &reasoning.content {
                events.push(SseEvent::new(
                    Some("response.reasoning.done"),
                    serde_json::json!({
                        "type": "response.reasoning.done",
                        "item_id": reasoning.item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "text": text
                    })
                    .to_string(),
                ));
                content.push(serde_json::json!({
                    "type": "reasoning_text",
                    "text": text
                }));
            }
            let mut item = serde_json::json!({
                "type": "reasoning",
                "id": reasoning.item_id,
                "summary": summary,
                "content": content
            });
            if let Some(encrypted_content) = &reasoning.encrypted_content {
                item["encrypted_content"] = serde_json::Value::String(encrypted_content.clone());
            }
            events.push(SseEvent::new(
                Some("response.output_item.done"),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item
                })
                .to_string(),
            ));
            indexed_reasoning_output.push((*output_index, item));
        }

        for call in &self.tool_calls {
            let arguments_done = serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": call.item_id,
                "output_index": call.output_index,
                "arguments": call.arguments
            });
            events.push(SseEvent::new(
                Some("response.function_call_arguments.done"),
                arguments_done.to_string(),
            ));
        }
        for call in &self.tool_calls {
            let tool_done = serde_json::json!({
                "type": "response.output_item.done",
                "output_index": call.output_index,
                "item": {
                    "type": "function_call",
                    "id": call.item_id,
                    "call_id": call.call_id,
                    "name": call.name,
                    "arguments": call.arguments,
                    "status": call.status.map(AiItemStatus::as_str).unwrap_or("completed")
                }
            });
            events.push(SseEvent::new(
                Some("response.output_item.done"),
                tool_done.to_string(),
            ));
        }
        for (output_index, item) in &self.indexed_function_outputs {
            events.push(SseEvent::new(
                Some("response.output_item.done"),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item
                })
                .to_string(),
            ));
        }
        for unknown in &self.unknown_items {
            if unknown.done {
                continue;
            }
            let unknown_done = serde_json::json!({
                "type": "response.output_item.done",
                "output_index": unknown.output_index,
                "item": unknown.item.clone()
            });
            events.push(SseEvent::new(
                Some("response.output_item.done"),
                unknown_done.to_string(),
            ));
        }

        let mut indexed_output: Vec<(usize, serde_json::Value)> = indexed_reasoning_output;
        for (output_index, message) in &self.indexed_messages {
            let mut content_by_index = BTreeMap::new();
            for (content_index, text) in &message.content {
                events.push(SseEvent::new(
                    Some("response.output_text.done"),
                    serde_json::json!({
                        "type": "response.output_text.done",
                        "item_id": message.item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "text": text
                    })
                    .to_string(),
                ));
                let part = self.terminal_content_part(
                    *output_index,
                    *content_index,
                    serde_json::json!({
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                        "logprobs": []
                    }),
                );
                events.push(SseEvent::new(
                    Some("response.content_part.done"),
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "item_id": message.item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "part": part
                    })
                    .to_string(),
                ));
                content_by_index.insert(*content_index, part);
            }
            for (content_index, refusal) in &message.refusals {
                events.push(SseEvent::new(
                    Some("response.refusal.done"),
                    serde_json::json!({
                        "type": "response.refusal.done",
                        "item_id": message.item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "refusal": refusal
                    })
                    .to_string(),
                ));
                let part = self.terminal_content_part(
                    *output_index,
                    *content_index,
                    serde_json::json!({
                        "type": "refusal",
                        "refusal": refusal
                    }),
                );
                events.push(SseEvent::new(
                    Some("response.content_part.done"),
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "item_id": message.item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "part": part
                    })
                    .to_string(),
                ));
                content_by_index.insert(*content_index, part);
            }
            let content = content_by_index.into_values().collect::<Vec<_>>();
            let item_status = message.status.map(AiItemStatus::as_str).unwrap_or(status);
            let item = serde_json::json!({
                "type": "message",
                "id": message.item_id,
                "status": item_status,
                "role": "assistant",
                "content": content
            });
            events.push(SseEvent::new(
                Some("response.output_item.done"),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item
                })
                .to_string(),
            ));
            indexed_output.push((*output_index, item));
        }

        let mut indexed_message_content = Vec::new();
        if let Some(output_index) = self.message_output_index {
            if let Some(content_index) = self.text_content_index {
                let text_done = serde_json::json!({
                    "type": "response.output_text.done",
                    "item_id": self.msg_id,
                    "output_index": output_index,
                    "content_index": content_index,
                    "text": self.accumulated_text
                });
                events.push(SseEvent::new(
                    Some("response.output_text.done"),
                    text_done.to_string(),
                ));
                let part = self.terminal_content_part(
                    output_index,
                    content_index,
                    serde_json::json!({
                        "type": "output_text",
                        "text": self.accumulated_text,
                        "annotations": [],
                        "logprobs": []
                    }),
                );
                events.push(SseEvent::new(
                    Some("response.content_part.done"),
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "item_id": self.msg_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "part": part.clone()
                    })
                    .to_string(),
                ));
                indexed_message_content.push((content_index, part));
            }
            if let Some(content_index) = self.refusal_content_index {
                let refusal_done = serde_json::json!({
                    "type": "response.refusal.done",
                    "item_id": self.msg_id,
                    "output_index": output_index,
                    "content_index": content_index,
                    "refusal": self.accumulated_refusal
                });
                events.push(SseEvent::new(
                    Some("response.refusal.done"),
                    refusal_done.to_string(),
                ));
                let part = self.terminal_content_part(
                    output_index,
                    content_index,
                    serde_json::json!({
                        "type": "refusal",
                        "refusal": self.accumulated_refusal
                    }),
                );
                events.push(SseEvent::new(
                    Some("response.content_part.done"),
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "item_id": self.msg_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "part": part.clone()
                    })
                    .to_string(),
                ));
                indexed_message_content.push((content_index, part));
            }
            indexed_message_content.sort_by_key(|(content_index, _)| *content_index);
            let content = indexed_message_content
                .iter()
                .map(|(_, part)| part.clone())
                .collect::<Vec<_>>();
            events.push(SseEvent::new(
                Some("response.output_item.done"),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": {
                        "type": "message",
                        "id": self.msg_id,
                        "status": "completed",
                        "role": "assistant",
                        "content": content
                    }
                })
                .to_string(),
            ));
        }
        if let Some(output_index) = self.message_output_index {
            indexed_output.push((
                output_index,
                serde_json::json!({
                    "type": "message",
                    "id": self.msg_id,
                    "status": "completed",
                    "role": "assistant",
                    "content": indexed_message_content
                        .iter()
                        .map(|(_, part)| part.clone())
                        .collect::<Vec<_>>()
                }),
            ));
        }
        if self.reasoning_item_id.is_some()
            && let Some(output_index) = self.reasoning_output_index
        {
            indexed_output.push((output_index, self.reasoning_item()));
        }
        for call in &self.tool_calls {
            indexed_output.push((
                call.output_index,
                serde_json::json!({
                    "type": "function_call",
                    "id": call.item_id,
                    "call_id": call.call_id,
                    "name": call.name,
                    "arguments": call.arguments,
                    "status": call.status.map(AiItemStatus::as_str).unwrap_or("completed")
                }),
            ));
        }
        for (output_index, item) in &self.indexed_function_outputs {
            indexed_output.push((*output_index, item.clone()));
        }
        for unknown in &self.unknown_items {
            indexed_output.push((unknown.output_index, unknown.item.clone()));
        }
        indexed_output.sort_by_key(|(output_index, _)| *output_index);

        let output = indexed_output
            .into_iter()
            .map(|(_, item)| item)
            .collect::<Vec<_>>();

        let usage = if !self.usage.required_components_known {
            serde_json::Value::Null
        } else {
            let mut input_tokens_details = serde_json::json!({
                "cached_tokens": self.usage.cache_read_tokens.unwrap_or(0)
            });
            if let Some(cache_write_tokens) = self.usage.cache_creation_tokens {
                input_tokens_details["cache_write_tokens"] = cache_write_tokens.into();
            }
            serde_json::json!({
                "input_tokens": self.usage.prompt_tokens,
                "output_tokens": self.usage.completion_tokens,
                "total_tokens": self.usage.total_tokens,
                "input_tokens_details": input_tokens_details,
                "output_tokens_details": {
                    "reasoning_tokens": self.usage.reasoning_tokens.unwrap_or(0)
                }
            })
        };
        let mut response = response_resource_snapshot(
            &self.resp_id,
            &self.model,
            status,
            output,
            incomplete_details,
            serde_json::Value::Null,
            usage,
        );
        self.apply_response_profile(&mut response);
        let event_type = format!("response.{status}");
        events.push(SseEvent::new(
            Some(&event_type),
            serde_json::json!({
                "type": event_type,
                "response": response,
            })
            .to_string(),
        ));

        events
    }
}

impl ResponsesStreamFormatter {
    pub(crate) fn format_deltas(&mut self, deltas: &[AiStreamDelta]) -> Vec<SseEvent> {
        let mut events = Vec::new();

        for delta in deltas {
            match delta {
                AiStreamDelta::MessageStart { id, model } => {
                    if !id.is_empty() {
                        self.resp_id = id.clone();
                    }
                    self.message_output_index = None;
                    self.model = model.clone();
                    self.ensure_started(&mut events);
                }
                AiStreamDelta::ResponseMetadata { metadata } => {
                    if let Some(metadata) = metadata.as_object() {
                        self.response_profile.extend(metadata.clone());
                    }
                }
                AiStreamDelta::ThinkingDelta(text) => {
                    self.emit_reasoning_delta(&mut events, text, None);
                }
                AiStreamDelta::ThinkingDeltaWithMetadata {
                    text,
                    obfuscation,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                } => self.emit_indexed_reasoning_delta(
                    &mut events,
                    *output_index,
                    *content_index,
                    text,
                    obfuscation.as_deref(),
                ),
                AiStreamDelta::ThinkingDeltaWithMetadata {
                    text, obfuscation, ..
                } => {
                    self.emit_reasoning_delta(&mut events, text, obfuscation.as_deref());
                }
                AiStreamDelta::ReasoningSummaryDelta {
                    text,
                    obfuscation,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                } => self.emit_indexed_reasoning_summary_delta(
                    &mut events,
                    *output_index,
                    *content_index,
                    text,
                    obfuscation.as_deref(),
                ),
                AiStreamDelta::ReasoningSummaryDelta {
                    text, obfuscation, ..
                } => {
                    self.emit_reasoning_summary_delta(&mut events, text, obfuscation.as_deref());
                }
                AiStreamDelta::ThinkingSignature(signature) => {
                    self.reasoning_encrypted_content = Some(signature.clone());
                }
                AiStreamDelta::TextDelta(text) => {
                    self.emit_text_delta(&mut events, text, None);
                }
                AiStreamDelta::TextDeltaWithMetadata {
                    text,
                    logprobs,
                    obfuscation,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                } => self.emit_indexed_text_delta(
                    &mut events,
                    *output_index,
                    *content_index,
                    text,
                    Some((logprobs, obfuscation.as_deref())),
                ),
                AiStreamDelta::TextDeltaWithMetadata {
                    text,
                    logprobs,
                    obfuscation,
                    ..
                } => {
                    self.emit_text_delta(
                        &mut events,
                        text,
                        Some((logprobs, obfuscation.as_deref())),
                    );
                }

                AiStreamDelta::RefusalDelta(text) => {
                    self.ensure_refusal_started(&mut events);
                    self.accumulated_refusal.push_str(text);
                    let ev = serde_json::json!({
                        "type": "response.refusal.delta",
                        "item_id": self.msg_id,
                        "output_index": self.message_output_index.expect("started message"),
                        "content_index": self.refusal_content_index.expect("started refusal"),
                        "delta": text
                    });
                    events.push(SseEvent::new(
                        Some("response.refusal.delta"),
                        ev.to_string(),
                    ));
                }
                AiStreamDelta::RefusalDeltaWithIndex {
                    text,
                    output_index,
                    content_index,
                } => self.emit_indexed_refusal_delta(
                    &mut events,
                    *output_index,
                    *content_index,
                    text,
                ),
                AiStreamDelta::ToolCallStart { index, id, name } => {
                    self.ensure_started(&mut events);
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    let item_id = gateway_item_id("fc", &self.resp_id, output_index);
                    let call_id = if id.is_empty() {
                        format!("call_{}", Uuid::new_v4().simple())
                    } else {
                        id.clone()
                    };

                    self.tool_index_map.insert(*index, self.tool_calls.len());
                    self.tool_calls.push(PendingFunctionCall {
                        output_index,
                        item_id: item_id.clone(),
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        status: None,
                    });

                    let added = serde_json::json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": {
                            "type": "function_call",
                            "id": item_id,
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                            "status": "in_progress"
                        }
                    });
                    events.push(SseEvent::new(
                        Some("response.output_item.added"),
                        added.to_string(),
                    ));
                }
                AiStreamDelta::ToolCallDelta { index, arguments } => {
                    if let Some(pos) = self.tool_index_map.get(index).copied()
                        && let Some(call) = self.tool_calls.get_mut(pos)
                    {
                        call.arguments.push_str(arguments);
                        let ev = serde_json::json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": call.item_id,
                            "output_index": call.output_index,
                            "delta": arguments
                        });
                        events.push(SseEvent::new(
                            Some("response.function_call_arguments.delta"),
                            ev.to_string(),
                        ));
                    }
                }
                AiStreamDelta::Unknown { raw } => {
                    let Ok(mut item) = serde_json::from_str::<serde_json::Value>(raw) else {
                        continue;
                    };
                    if let Some(object) = item.as_object_mut() {
                        object.remove("stravia_artifact_id");
                        object.remove("stravia_partial_images");
                    }
                    if let Some(mut event) = item
                        .as_object_mut()
                        .and_then(|object| object.remove("__open_responses_event"))
                    {
                        if event.get("type").and_then(serde_json::Value::as_str)
                            == Some("response.output_text.annotation.added")
                        {
                            let output_index = event
                                .get("output_index")
                                .and_then(serde_json::Value::as_u64)
                                .and_then(|value| usize::try_from(value).ok());
                            let content_index = event
                                .get("content_index")
                                .and_then(serde_json::Value::as_u64)
                                .and_then(|value| usize::try_from(value).ok());
                            if let (Some(output_index), Some(content_index)) =
                                (output_index, content_index)
                            {
                                self.ensure_indexed_content_part(
                                    &mut events,
                                    output_index,
                                    content_index,
                                    false,
                                );
                                event["item_id"] = serde_json::Value::String(
                                    self.indexed_messages[&output_index].item_id.clone(),
                                );
                                event["output_index"] = serde_json::Value::from(output_index);
                                event["content_index"] = serde_json::Value::from(content_index);
                            } else {
                                self.ensure_text_started(&mut events);
                                event["item_id"] = serde_json::Value::String(self.msg_id.clone());
                                event["output_index"] = serde_json::Value::from(
                                    self.message_output_index.expect("started message"),
                                );
                                event["content_index"] = serde_json::Value::from(
                                    self.text_content_index.expect("started text"),
                                );
                            }
                        } else {
                            self.ensure_started(&mut events);
                        }
                        let event_type = event
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("error")
                            .to_owned();
                        if event_type == "response.output_text.annotation.added" {
                            self.pending_annotations.push(event);
                        } else {
                            events.push(SseEvent::new(Some(&event_type), event.to_string()));
                        }
                        continue;
                    }
                    if let Some("stravia:agent_result" | "stravia:media_result") =
                        item.get("type").and_then(|value| value.as_str())
                    {
                        self.ensure_started(&mut events);
                        let output_index = self.next_output_index;
                        self.next_output_index += 1;
                        if let Some(object) = item.as_object_mut() {
                            object.insert(
                                "id".into(),
                                serde_json::Value::String(gateway_item_id(
                                    "item",
                                    &self.resp_id,
                                    output_index,
                                )),
                            );
                        }
                        self.unknown_items.push(PendingUnknownItem {
                            output_index,
                            item: item.clone(),
                            done: false,
                        });
                        let mut pending = item;
                        pending["status"] = serde_json::Value::String("in_progress".into());
                        events.push(SseEvent::new(
                            Some("response.output_item.added"),
                            serde_json::json!({
                                "type": "response.output_item.added",
                                "output_index": output_index,
                                "item": pending
                            })
                            .to_string(),
                        ));
                    }
                }
                AiStreamDelta::ItemDone { index, item } => {
                    if let Some(content) = item
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("__open_responses_content"))
                        .and_then(serde_json::Value::as_array)
                    {
                        self.completed_message_content
                            .insert(*index, content.clone());
                    }
                    self.flush_pending_annotations(&mut events, *index);
                    if item.function_call_ref().is_some()
                        && let Some(call) = self
                            .tool_calls
                            .iter_mut()
                            .find(|call| call.output_index == *index)
                    {
                        call.status = item.status();
                    }
                    if let Some((call_id, content)) = item.function_call_output_ref() {
                        self.ensure_started(&mut events);
                        self.next_output_index = self.next_output_index.max(*index + 1);
                        let item_id = gateway_item_id("fco", &self.resp_id, *index);
                        let output = super::formatter::function_output_value(content);
                        events.push(SseEvent::new(
                            Some("response.output_item.added"),
                            serde_json::json!({
                                "type": "response.output_item.added",
                                "output_index": index,
                                "item": {
                                    "type": "function_call_output",
                                    "id": item_id.clone(),
                                    "call_id": call_id,
                                    "output": output.clone(),
                                    "status": "in_progress"
                                }
                            })
                            .to_string(),
                        ));
                        self.indexed_function_outputs.insert(
                            *index,
                            serde_json::json!({
                                "type": "function_call_output",
                                "id": item_id,
                                "call_id": call_id,
                                "output": output,
                                "status": item
                                    .status()
                                    .map(AiItemStatus::as_str)
                                    .unwrap_or("completed")
                            }),
                        );
                    }
                    let is_message = item.role == crate::protocol::ir::Role::Assistant
                        && item.tool_calls.is_none()
                        && item.reasoning_ref().is_none()
                        && item.thinking_ref().is_none()
                        && item.unknown_ref().is_none();
                    if is_message && self.message_output_index != Some(*index) {
                        self.ensure_indexed_message(&mut events, *index, None);
                        self.indexed_messages
                            .get_mut(index)
                            .expect("indexed message was inserted")
                            .status = item.status();
                    }
                    if let Some((_, _, encrypted_content)) = item.reasoning_ref() {
                        self.ensure_indexed_reasoning(&mut events, *index, None);
                        self.indexed_reasoning
                            .get_mut(index)
                            .expect("indexed reasoning was inserted")
                            .encrypted_content = encrypted_content.map(str::to_owned);
                    }
                }
                AiStreamDelta::Usage(u) => {
                    if u.prompt_tokens > 0 {
                        self.usage.prompt_tokens = u.prompt_tokens;
                    }
                    if u.total_tokens > 0 {
                        self.usage.total_tokens = u.total_tokens;
                    }
                    if u.completion_tokens > 0 {
                        self.usage.completion_tokens = u.completion_tokens;
                    }
                    if u.cache_read_tokens.is_some() {
                        self.usage.cache_read_tokens = u.cache_read_tokens;
                    }
                    if u.cache_creation_tokens.is_some() {
                        self.usage.cache_creation_tokens = u.cache_creation_tokens;
                    }
                    if u.reasoning_tokens.is_some() {
                        self.usage.reasoning_tokens = u.reasoning_tokens;
                    }
                    if u.server_tool_use.is_some() {
                        self.usage.server_tool_use = u.server_tool_use.clone();
                    }
                    self.usage.required_components_known = u.required_components_known;
                }
                AiStreamDelta::StreamError { error: _ } => {
                    self.failed = true;
                    self.completed = true;
                    let public_error = serde_json::json!({
                        "type": "server_error",
                        "code": "response_stream_failed",
                        "message": "The response stream failed.",
                        "param": null,
                    });
                    events.push(SseEvent::new(
                        Some("error"),
                        serde_json::json!({
                            "type": "error",
                            "error": public_error.clone(),
                        })
                        .to_string(),
                    ));
                    let mut response = response_resource_snapshot(
                        &self.resp_id,
                        &self.model,
                        "failed",
                        Vec::new(),
                        serde_json::Value::Null,
                        public_error,
                        serde_json::Value::Null,
                    );
                    self.apply_response_profile(&mut response);
                    events.push(SseEvent::new(
                        Some("response.failed"),
                        serde_json::json!({
                            "type": "response.failed",
                            "response": response,
                        })
                        .to_string(),
                    ));
                    break;
                }
                AiStreamDelta::ResponseTerminal {
                    status,
                    incomplete_details,
                } if !self.completed => {
                    self.completed = true;
                    events.extend(
                        self.emit_terminal(
                            status,
                            incomplete_details
                                .clone()
                                .unwrap_or(serde_json::Value::Null),
                        ),
                    );
                }
                AiStreamDelta::Done { stop_reason } if !self.completed => {
                    self.completed = true;
                    let incomplete = matches!(
                        stop_reason.as_str(),
                        "length" | "max_tokens" | "content_filter"
                    );
                    let status = if incomplete {
                        "incomplete"
                    } else {
                        "completed"
                    };
                    let details = incomplete.then(|| {
                        serde_json::json!({
                            "reason": if stop_reason == "content_filter" {
                                "content_filter"
                            } else {
                                "max_output_tokens"
                            }
                        })
                    });
                    events.extend(
                        self.emit_terminal(status, details.unwrap_or(serde_json::Value::Null)),
                    );
                }
                AiStreamDelta::Done { .. } => {}
                _ => {}
            }
        }
        self.finalize_events(&mut events);

        events
    }

    pub(crate) fn format_done(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.completed {
            self.completed = true;
            events.extend(self.emit_terminal("completed", serde_json::Value::Null));
        }
        self.finalize_events(&mut events);
        events.push(SseEvent::new(None, "[DONE]"));
        events
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{ContentBlock, MessageContent};

    #[test]
    fn stream_formatter_closes_failures_with_standard_terminal_sequence() {
        let mut formatter = ResponsesStreamFormatter::new();
        let mut events = formatter.format_deltas(&[AiStreamDelta::StreamError {
            error: crate::protocol::ir::AiError::new(
                crate::protocol::ir::AiErrorKind::StreamMidError,
                "stream aborted",
            ),
        }]);
        events.extend(formatter.format_done());

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.as_deref(), Some("error"));
        assert_eq!(events[1].event.as_deref(), Some("response.failed"));
        assert_eq!(events[2].event, None);
        assert_eq!(events[2].data, "[DONE]");
        let error: serde_json::Value = serde_json::from_str(&events[0].data).expect("error JSON");
        let failed: serde_json::Value = serde_json::from_str(&events[1].data).expect("failed JSON");
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["code"], "response_stream_failed");
        assert_eq!(error["error"]["message"], "The response stream failed.");
        assert!(!error.to_string().contains("stream aborted"));
        assert_eq!(error["sequence_number"], 0);
        assert_eq!(failed["type"], "response.failed");
        assert_eq!(failed["sequence_number"], 1);
    }

    #[test]
    fn response_profile_uses_effective_request_and_provider_confirmed_values() {
        let mut request = crate::protocol::ir::AiRequest::new("logical-model", Vec::new());
        request.instructions = Some("Be concise.".into());
        request.generation.temperature = Some(0.2);
        request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
            crate::protocol::ir::OpenResponsesExt {
                store: Some(false),
                metadata: Some(serde_json::json!({"tenant": "acme"})),
                safety_identifier: Some("safe-user".into()),
                ..Default::default()
            },
        ));
        let mut formatter = ResponsesStreamFormatter::new();
        formatter.set_response_profile_from_request(&request, Some("resp_parent"));
        let events = formatter.format_deltas(&[
            AiStreamDelta::ResponseMetadata {
                metadata: serde_json::json!({"temperature": 0.7}),
            },
            AiStreamDelta::MessageStart {
                id: "resp_gateway".into(),
                model: "logical-model".into(),
            },
        ]);
        let created: serde_json::Value =
            serde_json::from_str(&events[0].data).expect("response.created JSON");
        let response = &created["response"];

        assert_eq!(response["previous_response_id"], "resp_parent");
        assert_eq!(response["instructions"], "Be concise.");
        assert_eq!(response["temperature"], 0.7);
        assert_eq!(response["store"], false);
        assert_eq!(response["metadata"]["tenant"], "acme");
        assert_eq!(response["safety_identifier"], "safe-user");
    }

    #[test]
    fn stream_events_have_matching_names_and_strict_sequence_numbers() {
        let mut formatter = ResponsesStreamFormatter::new();
        let mut events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp_gateway".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::TextDelta("hello".into()),
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        events.extend(formatter.format_done());

        let json_events = &events[..events.len() - 1];
        for (sequence, event) in json_events.iter().enumerate() {
            let body: serde_json::Value =
                serde_json::from_str(&event.data).expect("stream event JSON");
            assert_eq!(event.event.as_deref(), body["type"].as_str());
            assert_eq!(body["sequence_number"], sequence as u64);
        }
        assert_eq!(events.last().expect("DONE").data, "[DONE]");
    }
    #[test]
    fn terminal_usage_distinguishes_known_zero_counts_from_missing_usage() {
        let mut known = ResponsesStreamFormatter::new();
        let events = known.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-known".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::Usage(Usage {
                reasoning_tokens: Some(7),
                cache_read_tokens: Some(3),
                cache_creation_tokens: Some(5),
                required_components_known: true,
                ..Usage::default()
            }),
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let completed = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|body| body["type"] == "response.completed")
            .expect("known usage terminal");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 0);
        assert_eq!(completed["response"]["usage"]["output_tokens"], 0);
        assert_eq!(completed["response"]["usage"]["total_tokens"], 0);
        assert_eq!(
            completed["response"]["usage"]["input_tokens_details"],
            serde_json::json!({"cached_tokens": 3, "cache_write_tokens": 5})
        );
        assert_eq!(
            completed["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            7
        );

        let mut missing = ResponsesStreamFormatter::new();
        let events = missing.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-missing".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let completed = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|body| body["type"] == "response.completed")
            .expect("missing usage terminal");
        assert!(completed["response"]["usage"].is_null());
    }
    #[test]
    fn refusal_stream_emits_refusal_events_and_content() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-refusal".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::RefusalDelta("cannot comply".into()),
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let bodies = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .collect::<Vec<_>>();

        assert!(bodies.iter().any(|body| {
            body["type"] == "response.refusal.delta" && body["delta"] == "cannot comply"
        }));
        let completed = bodies
            .iter()
            .find(|body| body["type"] == "response.completed")
            .expect("completed response");
        assert_eq!(
            completed["response"]["output"][0]["content"][0],
            serde_json::json!({"type": "refusal", "refusal": "cannot comply"})
        );
        assert!(
            !bodies
                .iter()
                .any(|body| body["type"] == "response.output_text.delta")
        );
    }

    #[test]
    fn tool_only_stream_does_not_emit_an_empty_message_item() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-tool".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "lookup".into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{}".into(),
            },
            AiStreamDelta::Done {
                stop_reason: "tool_calls".into(),
            },
        ]);
        let bodies = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .collect::<Vec<_>>();
        assert!(!bodies.iter().any(|body| {
            body["item"]["type"] == "message"
                && body["item"]["content"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
        }));
        let added = bodies
            .iter()
            .find(|body| {
                body["type"] == "response.output_item.added"
                    && body["item"]["type"] == "function_call"
            })
            .expect("function call added");
        assert_eq!(added["output_index"], 0);
    }

    #[test]
    fn function_call_item_done_preserves_incomplete_status() {
        let mut formatter = ResponsesStreamFormatter::new();
        let completed = crate::protocol::ir::AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        })
        .with_graph_metadata(
            Some("fc_provider".into()),
            Some(AiItemStatus::Incomplete),
            crate::protocol::ir::AiItemProvenance::Provider,
            crate::protocol::ir::AiItemAudience::Client,
        );
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-tool-status".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "lookup".into(),
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: completed,
            },
            AiStreamDelta::ResponseTerminal {
                status: "incomplete".into(),
                incomplete_details: Some(serde_json::json!({"reason": "max_output_tokens"})),
            },
        ]);

        let done = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "function_call"
            })
            .expect("function call done");
        assert_eq!(done["item"]["status"], "incomplete");
        let terminal = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.incomplete")
            .expect("terminal response");
        assert_eq!(terminal["response"]["output"][0]["status"], "incomplete");
    }

    #[test]
    fn streams_platform_owned_result_as_indexed_output_item() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-1".into(),
                model: "model-1".into(),
            },
            AiStreamDelta::Unknown {
                raw: r#"{"type":"stravia:agent_result","turn_id":"aturn_1"}"#.into(),
            },
            AiStreamDelta::Unknown {
                raw: r#"{"type":"stravia:media_result","turn_id":"aturn_media","completion":"complete"}"#.into(),
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);

        let item_events = events
            .iter()
            .filter_map(|event| {
                let body = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
                let is_item_event = matches!(
                    body["type"].as_str(),
                    Some("response.output_item.added" | "response.output_item.done")
                );
                is_item_event.then_some(body)
            })
            .filter(|body| body["item"]["type"] == "stravia:agent_result")
            .collect::<Vec<_>>();
        assert_eq!(item_events.len(), 2);
        assert_eq!(item_events[0]["output_index"], 0);
        assert_eq!(item_events[1]["output_index"], 0);
        assert_eq!(item_events[0]["item"]["turn_id"], "aturn_1");
        assert_eq!(item_events[1]["item"]["turn_id"], "aturn_1");
        let media_events = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .filter(|body| {
                matches!(
                    body["type"].as_str(),
                    Some("response.output_item.added" | "response.output_item.done")
                ) && body["item"]["type"] == "stravia:media_result"
            })
            .collect::<Vec<_>>();
        assert_eq!(media_events.len(), 2);
        assert_eq!(media_events[0]["output_index"], 1);
        assert_eq!(media_events[1]["output_index"], 1);
        assert_eq!(media_events[0]["item"]["turn_id"], "aturn_media");
        assert_eq!(media_events[0]["item"]["completion"], "complete");

        let completed = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|body| body["type"] == "response.completed")
            .expect("response.completed event");
        assert!(
            completed["response"]["output"]
                .as_array()
                .expect("response output")
                .iter()
                .any(|item| item["type"] == "stravia:agent_result" && item["turn_id"] == "aturn_1")
        );
        assert!(
            completed["response"]["output"]
                .as_array()
                .expect("response output")
                .iter()
                .any(|item| item["type"] == "stravia:media_result"
                    && item["turn_id"] == "aturn_media"
                    && item["completion"] == "complete")
        );
    }

    #[test]
    fn terminal_message_clears_metadata_after_text_rewrite() {
        let mut formatter = ResponsesStreamFormatter::new();
        let mut completed = crate::protocol::ir::AiItem::output_text("before");
        completed.meta = Some(serde_json::json!({
            "__open_responses_content": [{
                "type": "output_text",
                "text": "before",
                "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                "logprobs": [{"token": "before", "logprob": -0.1}]
            }]
        }));
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-1".into(),
                model: "model-1".into(),
            },
            AiStreamDelta::TextDelta("after".into()),
            AiStreamDelta::ItemDone {
                index: 0,
                item: completed,
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);

        let terminal = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.completed")
            .expect("response.completed");
        let content = &terminal["response"]["output"][0]["content"][0];
        assert_eq!(content["text"], "after");
        assert_eq!(content["annotations"], serde_json::json!([]));
        assert_eq!(content["logprobs"], serde_json::json!([]));
    }

    #[test]
    fn private_extension_progress_is_not_exposed() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[AiStreamDelta::Unknown {
            raw: serde_json::json!({
                "type": "stravia_web_search_activity",
                "call_id": "call_1",
                "phase": "searching",
                "ordinal": 2
            })
            .to_string(),
        }]);

        assert!(events.is_empty());
    }

    #[test]
    fn text_delta_preserves_logprobs_and_obfuscation() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-1".into(),
                model: "model-1".into(),
            },
            AiStreamDelta::TextDeltaWithMetadata {
                text: "hello".into(),
                logprobs: vec![serde_json::json!({
                    "token": "hello",
                    "logprob": -0.1,
                    "bytes": [104, 101, 108, 108, 111],
                    "top_logprobs": []
                })],
                obfuscation: Some("pad".into()),
                output_index: None,
                content_index: None,
            },
        ]);

        let delta = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.output_text.delta")
            .expect("output text delta");
        assert_eq!(delta["logprobs"][0]["token"], "hello");
        assert_eq!(delta["obfuscation"], "pad");
    }

    #[test]
    fn reasoning_summary_and_content_stream_as_distinct_events() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-1".into(),
                model: "model-1".into(),
            },
            AiStreamDelta::ReasoningSummaryDelta {
                text: "summary".into(),
                obfuscation: Some("summary-pad".into()),
                output_index: None,
                content_index: None,
            },
            AiStreamDelta::ThinkingDeltaWithMetadata {
                text: "full reasoning".into(),
                obfuscation: Some("content-pad".into()),
                output_index: None,
                content_index: None,
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let bodies = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .collect::<Vec<_>>();

        let summary_delta = bodies
            .iter()
            .find(|event| event["type"] == "response.reasoning_summary_text.delta")
            .expect("summary delta");
        assert_eq!(summary_delta["obfuscation"], "summary-pad");
        let content_delta = bodies
            .iter()
            .find(|event| event["type"] == "response.reasoning.delta")
            .expect("reasoning content delta");
        assert_eq!(content_delta["obfuscation"], "content-pad");
        let terminal = bodies
            .iter()
            .find(|event| event["type"] == "response.completed")
            .expect("response completed");
        let reasoning = &terminal["response"]["output"][0];
        assert_eq!(reasoning["summary"][0]["text"], "summary");
        assert_eq!(reasoning["content"][0]["text"], "full reasoning");
    }

    #[test]
    fn completed_item_forwards_encrypted_only_reasoning() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-1".into(),
                model: "model-1".into(),
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: crate::protocol::ir::AiItem::reasoning(
                    Vec::new(),
                    Vec::new(),
                    Some("opaque".into()),
                ),
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let terminal = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.completed")
            .expect("response completed");

        assert_eq!(
            terminal["response"]["output"][0]["encrypted_content"],
            "opaque"
        );
    }
    #[test]
    fn preserves_multiple_message_output_indices() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp-1".into(),
                model: "model-1".into(),
            },
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
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let bodies = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .collect::<Vec<_>>();
        let semantic_deltas = bodies
            .iter()
            .filter(|event| {
                matches!(
                    event["type"].as_str(),
                    Some("response.output_text.delta" | "response.refusal.delta")
                )
            })
            .map(|event| {
                (
                    event["output_index"].as_u64().expect("output index"),
                    event["delta"].as_str().expect("text"),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(semantic_deltas, [(0, "first"), (1, "second")]);
        let terminal = bodies
            .iter()
            .find(|event| event["type"] == "response.completed")
            .expect("response completed");
        assert_eq!(
            terminal["response"]["output"][0]["content"][0]["text"],
            "first"
        );
        assert_eq!(
            terminal["response"]["output"][1]["content"][0]["refusal"],
            "second"
        );
    }
    #[test]
    fn preserves_multiple_reasoning_output_indices() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp_reasoning".into(),
                model: "model".into(),
            },
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
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let terminal = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.completed")
            .expect("response completed");
        assert_eq!(
            terminal["response"]["output"][0]["summary"][0]["text"],
            "first"
        );
        assert_eq!(
            terminal["response"]["output"][1]["summary"][0]["text"],
            "second"
        );
    }

    #[test]
    fn item_done_creates_empty_messages_and_preserves_item_status() {
        let mut formatter = ResponsesStreamFormatter::new();
        let completed = crate::protocol::ir::AiItem::output_text("").with_graph_metadata(
            Some("msg_completed".into()),
            Some(crate::protocol::ir::AiItemStatus::Completed),
            crate::protocol::ir::AiItemProvenance::Provider,
            crate::protocol::ir::AiItemAudience::Client,
        );
        let incomplete = crate::protocol::ir::AiItem::output_text("partial").with_graph_metadata(
            Some("msg_incomplete".into()),
            Some(crate::protocol::ir::AiItemStatus::Incomplete),
            crate::protocol::ir::AiItemProvenance::Provider,
            crate::protocol::ir::AiItemAudience::Client,
        );
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp_messages".into(),
                model: "model".into(),
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: completed,
            },
            AiStreamDelta::TextDeltaWithMetadata {
                text: "partial".into(),
                logprobs: Vec::new(),
                obfuscation: None,
                output_index: Some(1),
                content_index: Some(0),
            },
            AiStreamDelta::ItemDone {
                index: 1,
                item: incomplete,
            },
            AiStreamDelta::ResponseTerminal {
                status: "incomplete".into(),
                incomplete_details: Some(serde_json::json!({"reason": "max_output_tokens"})),
            },
        ]);
        let terminal = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.incomplete")
            .expect("response incomplete");
        assert_eq!(terminal["response"]["output"][0]["status"], "completed");
        assert_eq!(
            terminal["response"]["output"][0]["content"],
            serde_json::json!([])
        );
        assert_eq!(terminal["response"]["output"][1]["status"], "incomplete");
        assert_eq!(
            terminal["response"]["output"][1]["content"][0]["text"],
            "partial"
        );
    }

    #[test]
    fn annotation_stays_on_its_unchanged_indexed_message() {
        let mut formatter = ResponsesStreamFormatter::new();
        let mut completed = crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
                ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
                ContentBlock::Text {
                    text: "answer".into(),
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        completed.meta = Some(serde_json::json!({
            "__open_responses_content": [
                {"type": "output_text", "text": "", "annotations": [], "logprobs": []},
                {"type": "output_text", "text": "", "annotations": [], "logprobs": []},
                {
                    "type": "output_text",
                    "text": "answer",
                    "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                    "logprobs": []
                }
            ]
        }));
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp_annotation".into(),
                model: "model".into(),
            },
            AiStreamDelta::TextDeltaWithMetadata {
                text: "answer".into(),
                logprobs: Vec::new(),
                obfuscation: None,
                output_index: Some(3),
                content_index: Some(2),
            },
            AiStreamDelta::Unknown {
                raw: serde_json::json!({
                    "__open_responses_event": {
                        "type": "response.output_text.annotation.added",
                        "sequence_number": 4,
                        "item_id": "provider-message",
                        "output_index": 3,
                        "content_index": 2,
                        "annotation_index": 0,
                        "annotation": {
                            "type": "url_citation",
                            "url": "https://example.test",
                            "title": "source",
                            "start_index": 0,
                            "end_index": 6
                        }
                    }
                })
                .to_string(),
            },
            AiStreamDelta::ItemDone {
                index: 3,
                item: completed,
            },
        ]);
        let annotation = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.output_text.annotation.added")
            .expect("annotation event");
        assert_eq!(annotation["output_index"], 3);
        assert_eq!(annotation["content_index"], 2);
        let part_added = events
            .iter()
            .position(|event| event.event.as_deref() == Some("response.content_part.added"))
            .expect("content part added");
        let annotation_added = events
            .iter()
            .position(|event| {
                event.event.as_deref() == Some("response.output_text.annotation.added")
            })
            .expect("annotation added");
        assert!(part_added < annotation_added);
        assert_eq!(formatter.indexed_messages.len(), 1);
        assert_eq!(formatter.message_output_index, None);
    }

    #[test]
    fn rewritten_text_drops_stale_annotation_events() {
        let mut formatter = ResponsesStreamFormatter::new();
        let mut completed = crate::protocol::ir::AiItem::output_text("before");
        completed.meta = Some(serde_json::json!({
            "__open_responses_content": [{
                "type": "output_text",
                "text": "before",
                "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                "logprobs": []
            }]
        }));
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp_rewritten".into(),
                model: "model".into(),
            },
            AiStreamDelta::TextDeltaWithMetadata {
                text: "after".into(),
                logprobs: Vec::new(),
                obfuscation: None,
                output_index: Some(0),
                content_index: Some(0),
            },
            AiStreamDelta::Unknown {
                raw: serde_json::json!({
                    "__open_responses_event": {
                        "type": "response.output_text.annotation.added",
                        "item_id": "provider-message",
                        "output_index": 0,
                        "content_index": 0,
                        "annotation_index": 0,
                        "annotation": {
                            "type": "url_citation",
                            "url": "https://example.test"
                        }
                    }
                })
                .to_string(),
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: completed,
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);

        assert!(events.iter().all(|event| {
            event.event.as_deref() != Some("response.output_text.annotation.added")
        }));
        let terminal = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.completed")
            .expect("terminal response");
        assert_eq!(
            terminal["response"]["output"][0]["content"][0]["annotations"],
            serde_json::json!([])
        );
    }
    #[test]
    fn item_done_does_not_restore_semantic_text_removed_by_a_hook() {
        let mut formatter = ResponsesStreamFormatter::new();
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp_redacted".into(),
                model: "model".into(),
            },
            AiStreamDelta::ItemDone {
                index: 0,
                item: crate::protocol::ir::AiItem::output_text("provider secret"),
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let terminal = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|event| event["type"] == "response.completed")
            .expect("terminal response");

        assert_eq!(
            terminal["response"]["output"][0]["content"],
            serde_json::json!([])
        );
    }

    #[test]
    fn function_output_item_done_emits_lifecycle_and_terminal_item() {
        let mut formatter = ResponsesStreamFormatter::new();
        let function_output = crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::Text {
                text: "tool output".into(),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        }
        .with_graph_metadata(
            Some("fco_provider".into()),
            Some(AiItemStatus::Completed),
            crate::protocol::ir::AiItemProvenance::Provider,
            crate::protocol::ir::AiItemAudience::Client,
        );
        let events = formatter.format_deltas(&[
            AiStreamDelta::MessageStart {
                id: "resp_function_output".into(),
                model: "model".into(),
            },
            AiStreamDelta::ItemDone {
                index: 2,
                item: function_output,
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);
        let bodies = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .collect::<Vec<_>>();

        assert!(bodies.iter().any(|event| {
            event["type"] == "response.output_item.added"
                && event["output_index"] == 2
                && event["item"]["type"] == "function_call_output"
                && event["item"]["id"] == "fco_function_output_2"
        }));
        assert!(bodies.iter().any(|event| {
            event["type"] == "response.output_item.done"
                && event["output_index"] == 2
                && event["item"]["output"][0]["type"] == "input_text"
        }));
        let terminal = bodies
            .iter()
            .find(|event| event["type"] == "response.completed")
            .expect("terminal response");
        assert_eq!(
            terminal["response"]["output"][0]["type"],
            "function_call_output"
        );
        assert_eq!(terminal["response"]["output"][0]["call_id"], "call_1");
        assert_eq!(
            terminal["response"]["output"][0]["id"],
            "fco_function_output_2"
        );
    }
}

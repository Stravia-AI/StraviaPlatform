use anyhow::Result;
use serde_json::Value;
use uuid::Uuid;

use crate::protocol::ir::request::ToolCall;
use crate::protocol::ir::usage::{ServerToolUsage, Usage};
use crate::protocol::ir::{AiItem, AiResponse, AiStreamDelta, ContentBlock, MessageContent};
use crate::protocol::*;

// ── Non-streaming response parser ──

pub struct AnthropicResponseParser;

impl AnthropicResponseParser {
    pub(crate) fn parse_response(&self, resp: Value) -> Result<AiResponse> {
        let id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = resp
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut items = Vec::new();
        if let Some(blocks) = resp.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            items.push(AiItem::output_text(text));
                        }
                    }
                    Some("thinking") => {
                        let thinking = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let signature = block
                            .get("signature")
                            .and_then(Value::as_str)
                            .filter(|signature| !signature.is_empty())
                            .map(str::to_owned);
                        if !thinking.is_empty() || signature.is_some() {
                            items.push(AiItem::thinking(thinking, signature));
                        }
                    }
                    Some("tool_use") => {
                        if let (Some(tc_id), Some(name)) = (
                            block.get("id").and_then(|v| v.as_str()),
                            block.get("name").and_then(|v| v.as_str()),
                        ) {
                            let input = block
                                .get("input")
                                .cloned()
                                .unwrap_or(Value::Object(Default::default()));
                            items.push(AiItem::function_call(ToolCall {
                                id: tc_id.to_string(),
                                name: name.to_string(),
                                arguments: input.to_string(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }

        let stop_reason = resp
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(|r| match r {
                "end_turn" => "stop".to_string(),
                "tool_use" => "tool_calls".to_string(),
                other => other.to_string(),
            });

        let usage = extract_anthropic_usage(&resp);

        let mut ai_resp = AiResponse::new(id, model);
        ai_resp.items = items;
        ai_resp.stop_reason = stop_reason;
        ai_resp.usage = usage;
        Ok(ai_resp)
    }
}

// ── Non-streaming response formatter ──

pub struct AnthropicResponseFormatter;

pub(crate) fn normalize_client_history_item(item: &mut AiItem) {
    let MessageContent::Blocks(blocks) = &mut item.content else {
        return;
    };
    for block in blocks {
        match block {
            ContentBlock::Reasoning {
                summary,
                content,
                encrypted_content,
            } => {
                let thinking = summary.iter().chain(content.iter()).cloned().collect();
                let signature = encrypted_content.take();
                *block = ContentBlock::Thinking {
                    thinking,
                    signature,
                };
            }
            ContentBlock::Refusal { refusal } => {
                *block = ContentBlock::Text {
                    text: std::mem::take(refusal),
                    cache_control: None,
                };
            }
            _ => {}
        }
    }
}

impl AnthropicResponseFormatter {
    pub(crate) fn format_response(&self, resp: &AiResponse) -> Value {
        let mut content = Vec::new();

        for item in &resp.items {
            if let Some((reasoning, signature)) = item.thinking_ref() {
                if reasoning.trim().is_empty() {
                    continue;
                }
                let mut block = serde_json::json!({
                    "type": "thinking",
                    "thinking": reasoning,
                });
                if let Some(signature) = signature.filter(|value| !value.trim().is_empty()) {
                    block
                        .as_object_mut()
                        .expect("thinking block is an object")
                        .insert("signature".into(), serde_json::json!(signature));
                }
                content.push(block);
            } else if let Some(text) = item.output_text_ref() {
                if !text.is_empty() {
                    content.push(serde_json::json!({"type": "text", "text": text}));
                }
            } else if let Some(call) = item.function_call_ref() {
                let input: Value = serde_json::from_str(&call.arguments)
                    .unwrap_or(Value::Object(Default::default()));
                content.push(serde_json::json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": input,
                }));
            }
        }

        let stop_reason = resp.stop_reason.as_deref().map(|reason| match reason {
            "stop" => "end_turn",
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            other => other,
        });

        let mut usage = serde_json::json!({
            "input_tokens": anthropic_input_tokens(&resp.usage),
            "output_tokens": resp.usage.completion_tokens,
        });
        extend_usage_json(&mut usage, &resp.usage);

        serde_json::json!({
            "id": resp.id,
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": resp.model,
            "stop_reason": stop_reason,
            "usage": usage
        })
    }
}

// ── Stream parser (upstream Anthropic SSE → deltas) ──

pub struct AnthropicStreamParser {
    buffer: String,
}

impl Default for AnthropicStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }
}

impl AnthropicStreamParser {
    pub(crate) fn parse_chunk(&mut self, raw: &str) -> Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();

        while let Some(pos) = self.buffer.find("\n\n") {
            let block = self.buffer[..pos].to_string();
            self.buffer = self.buffer[pos + 2..].to_string();

            let mut event_type = None;
            let mut data_str = None;

            for line in block.lines() {
                if let Some(ev) = line.strip_prefix("event: ") {
                    event_type = Some(ev.trim().to_string());
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data_str = Some(d.trim().to_string());
                }
            }

            if let Some(data) = data_str
                && let Ok(json) = serde_json::from_str::<Value>(&data)
            {
                parse_anthropic_event(event_type.as_deref(), &json, &mut deltas);
            }
        }

        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<AiStreamDelta>> {
        if self.buffer.trim().is_empty() {
            return Ok(vec![]);
        }
        let remaining = std::mem::take(&mut self.buffer);
        self.parse_chunk(&format!("{remaining}\n\n"))
    }
}

fn parse_anthropic_event(event_type: Option<&str>, data: &Value, deltas: &mut Vec<AiStreamDelta>) {
    match event_type {
        Some("message_start") => {
            if let Some(msg) = data.get("message") {
                let id = msg
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let model = msg
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // Usage BEFORE MessageStart so the formatter has the correct
                // input_tokens available when it emits the message_start SSE event.
                let u = extract_anthropic_usage(msg);
                if u.prompt_tokens > 0
                    || u.cache_read_tokens.is_some()
                    || u.cache_creation_tokens.is_some()
                    || u.server_tool_use.is_some()
                {
                    deltas.push(AiStreamDelta::Usage(u));
                }
                deltas.push(AiStreamDelta::MessageStart { id, model });
            }
        }
        Some("content_block_start") => {
            let idx = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(block) = data.get("content_block") {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        deltas.push(AiStreamDelta::ToolCallStart {
                            index: idx,
                            id,
                            name,
                        });
                    }
                    // Empty text/thinking block starts carry no canonical
                    // content. Their subsequent deltas contain the semantic
                    // payload, so emitting them as Unknown would make an
                    // otherwise lossless cross-protocol stream fail audit.
                    Some("text") | Some("thinking") => {}
                    // Anthropic server-side tool blocks (web_search, code_execution,
                    // mcp_tool_use, etc.) and any future block types not yet known:
                    // forward verbatim so downstream clients receive the full event.
                    _ => {
                        deltas.push(AiStreamDelta::Unknown {
                            raw: format!("event: content_block_start\ndata: {data}"),
                        });
                    }
                }
            }
        }
        Some("content_block_delta") => {
            if let Some(delta) = data.get("delta") {
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            deltas.push(AiStreamDelta::TextDelta(text.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        // Produced by Ollama and native Anthropic thinking models.
                        if let Some(text) = delta
                            .get("thinking")
                            .and_then(|t| t.as_str())
                            .filter(|text| !text.is_empty())
                        {
                            deltas.push(AiStreamDelta::ThinkingDelta(text.to_string()));
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(signature) = delta
                            .get("signature")
                            .and_then(|t| t.as_str())
                            .filter(|signature| !signature.is_empty())
                        {
                            deltas.push(AiStreamDelta::ThinkingSignature(signature.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(json) = delta.get("partial_json").and_then(|t| t.as_str()) {
                            let idx =
                                data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            deltas.push(AiStreamDelta::ToolCallDelta {
                                index: idx,
                                arguments: json.to_string(),
                            });
                        }
                    }
                    // Unknown delta types (e.g. web_search_tool_result_delta,
                    // citations_delta, code_execution_tool_result_delta):
                    // forward verbatim.
                    _ => {
                        deltas.push(AiStreamDelta::Unknown {
                            raw: format!("event: content_block_delta\ndata: {data}"),
                        });
                    }
                }
            }
        }
        Some("message_delta") => {
            // Usage BEFORE Done: the formatter emits message_delta SSE on Done,
            // so self.usage must already reflect the final counts.
            // Also read input_tokens here — ZhipuAI and others publish the real
            // value in message_delta.usage rather than message_start.usage.
            let u = extract_anthropic_usage(data);
            if u.prompt_tokens > 0
                || u.completion_tokens > 0
                || u.cache_read_tokens.is_some()
                || u.cache_creation_tokens.is_some()
                || u.server_tool_use.is_some()
            {
                deltas.push(AiStreamDelta::Usage(u));
            }
            if let Some(delta) = data.get("delta")
                && let Some(reason) = delta.get("stop_reason").and_then(|v| v.as_str())
            {
                let normalized = match reason {
                    "end_turn" => "stop",
                    "tool_use" => "tool_calls",
                    other => other,
                };
                deltas.push(AiStreamDelta::Done {
                    stop_reason: normalized.to_string(),
                });
            }
        }
        Some("ping") | Some("content_block_stop") | Some("message_stop") => {}
        // Unknown top-level event types: forward verbatim so no data is dropped.
        Some(ev) => {
            deltas.push(AiStreamDelta::Unknown {
                raw: format!("event: {ev}\ndata: {data}"),
            });
        }
        None => {}
    }
}

// ── Stream formatter (deltas → Anthropic SSE) ──

pub struct AnthropicStreamFormatter {
    usage: Usage,
    id: String,
    model: String,
    block_index: usize,
    in_thinking_block: bool,
    in_text_block: bool,
    in_tool_block: bool,
    message_started: bool,
}

impl Default for AnthropicStreamFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicStreamFormatter {
    pub fn new() -> Self {
        Self {
            usage: Usage::default(),
            id: format!("msg_{}", Uuid::new_v4().simple()),
            model: String::new(),
            block_index: 0,
            in_thinking_block: false,
            in_text_block: false,
            in_tool_block: false,
            message_started: false,
        }
    }

    fn ensure_message_start(&mut self, events: &mut Vec<SseEvent>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        let mut usage = serde_json::json!({
            "input_tokens": anthropic_input_tokens(&self.usage),
            "output_tokens": 0
        });
        extend_usage_json(&mut usage, &self.usage);
        let msg_start = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": self.id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "usage": usage
            }
        });
        events.push(SseEvent::new(Some("message_start"), msg_start.to_string()));
        events.push(SseEvent::new(Some("ping"), r#"{"type":"ping"}"#));
    }
}

impl AnthropicStreamFormatter {
    pub(crate) fn format_deltas(&mut self, deltas: &[AiStreamDelta]) -> Vec<SseEvent> {
        let mut events = Vec::new();

        for delta in deltas {
            match delta {
                AiStreamDelta::MessageStart { id, model } => {
                    self.id = id.clone();
                    self.model = model.clone();
                    self.ensure_message_start(&mut events);
                }
                AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
                | AiStreamDelta::ReasoningSummaryDelta { text, .. } => {
                    self.ensure_message_start(&mut events);
                    self.close_text_block_if_open(&mut events);
                    if !self.in_thinking_block {
                        self.in_thinking_block = true;
                        let block_start = serde_json::json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {"type": "thinking", "thinking": ""}
                        });
                        events.push(SseEvent::new(
                            Some("content_block_start"),
                            block_start.to_string(),
                        ));
                    }
                    let delta_ev = serde_json::json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "thinking_delta", "thinking": text}
                    });
                    events.push(SseEvent::new(
                        Some("content_block_delta"),
                        delta_ev.to_string(),
                    ));
                }
                AiStreamDelta::ThinkingSignature(signature) => {
                    self.emit_thinking_signature(&mut events, signature);
                }
                AiStreamDelta::ItemDone { item, .. } => {
                    if let Some((_, _, Some(signature))) = item.reasoning_ref()
                        && !signature.is_empty()
                    {
                        self.emit_thinking_signature(&mut events, signature);
                    }
                }
                AiStreamDelta::TextDelta(text)
                | AiStreamDelta::TextDeltaWithMetadata { text, .. }
                | AiStreamDelta::RefusalDelta(text)
                | AiStreamDelta::RefusalDeltaWithIndex { text, .. } => {
                    if !self.in_text_block && text.trim().is_empty() {
                        continue;
                    }
                    self.ensure_message_start(&mut events);
                    self.close_thinking_block_if_open(&mut events);
                    self.close_tool_block_if_open(&mut events);
                    if !self.in_text_block {
                        self.in_text_block = true;
                        let block_start = serde_json::json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {"type": "text", "text": ""}
                        });
                        events.push(SseEvent::new(
                            Some("content_block_start"),
                            block_start.to_string(),
                        ));
                    }
                    let delta_ev = serde_json::json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "text_delta", "text": text}
                    });
                    events.push(SseEvent::new(
                        Some("content_block_delta"),
                        delta_ev.to_string(),
                    ));
                }
                AiStreamDelta::ToolCallStart { index: _, id, name } => {
                    self.ensure_message_start(&mut events);
                    self.close_thinking_block_if_open(&mut events);
                    self.close_text_block_if_open(&mut events);
                    self.close_tool_block_if_open(&mut events);
                    let tool_use_id = if id.trim().is_empty() {
                        format!("toolu_{}", Uuid::new_v4().simple())
                    } else {
                        id.clone()
                    };
                    let block_start = serde_json::json!({
                        "type": "content_block_start",
                        "index": self.block_index,
                        "content_block": {"type": "tool_use", "id": tool_use_id, "name": name, "input": {}}
                    });
                    events.push(SseEvent::new(
                        Some("content_block_start"),
                        block_start.to_string(),
                    ));
                    self.in_tool_block = true;
                }
                AiStreamDelta::ToolCallDelta {
                    index: _,
                    arguments,
                } => {
                    let delta_ev = serde_json::json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "input_json_delta", "partial_json": arguments}
                    });
                    events.push(SseEvent::new(
                        Some("content_block_delta"),
                        delta_ev.to_string(),
                    ));
                }
                AiStreamDelta::Usage(u) => {
                    if u.prompt_tokens > 0 {
                        self.usage.prompt_tokens = u.prompt_tokens;
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
                    if u.server_tool_use.is_some() {
                        self.usage.server_tool_use = u.server_tool_use.clone();
                    }
                }
                AiStreamDelta::StreamError { error } => {
                    let error = serde_json::json!({
                        "type": "error",
                        "error": {
                            "type": error.kind.to_string(),
                            "message": error.message,
                        }
                    });
                    events.push(SseEvent::new(Some("error"), error.to_string()));
                }
                AiStreamDelta::Done { stop_reason } => {
                    self.ensure_message_start(&mut events);
                    self.close_thinking_block_if_open(&mut events);
                    self.close_text_block_if_open(&mut events);
                    self.close_tool_block_if_open(&mut events);
                    let anthropic_reason = match stop_reason.as_str() {
                        "stop" => "end_turn",
                        "length" => "max_tokens",
                        "tool_calls" => "tool_use",
                        other => other,
                    };
                    let mut usage = serde_json::json!({
                        "input_tokens": anthropic_input_tokens(&self.usage),
                        "output_tokens": self.usage.completion_tokens
                    });
                    extend_usage_json(&mut usage, &self.usage);
                    let msg_delta = serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": anthropic_reason},
                        "usage": usage
                    });
                    events.push(SseEvent::new(Some("message_delta"), msg_delta.to_string()));
                    events.push(SseEvent::new(
                        Some("message_stop"),
                        r#"{"type":"message_stop"}"#,
                    ));
                }
                // Pass-through: raw is in SSE format "event: TYPE\ndata: DATA".
                AiStreamDelta::Unknown { raw } => {
                    let mut ev_type = None;
                    let mut ev_data = String::new();
                    for line in raw.lines() {
                        if let Some(t) = line.strip_prefix("event: ") {
                            ev_type = Some(t.to_string());
                        } else if let Some(d) = line.strip_prefix("data: ") {
                            ev_data = d.to_string();
                        }
                    }
                    if let Some(et) = ev_type {
                        events.push(SseEvent::new(Some(et.as_str()), ev_data));
                    }
                }
                _ => {}
            }
        }

        events
    }

    pub(crate) fn format_done(&mut self) -> Vec<SseEvent> {
        vec![]
    }

    #[cfg(test)]
    pub(crate) fn usage(&self) -> Usage {
        self.usage.clone()
    }
}

impl AnthropicStreamFormatter {
    fn emit_thinking_signature(&mut self, events: &mut Vec<SseEvent>, signature: &str) {
        self.ensure_message_start(events);
        self.close_text_block_if_open(events);
        if !self.in_thinking_block {
            self.in_thinking_block = true;
            let block_start = serde_json::json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": {"type": "thinking", "thinking": ""}
            });
            events.push(SseEvent::new(
                Some("content_block_start"),
                block_start.to_string(),
            ));
        }
        let delta_ev = serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "signature_delta", "signature": signature}
        });
        events.push(SseEvent::new(
            Some("content_block_delta"),
            delta_ev.to_string(),
        ));
    }

    fn close_text_block_if_open(&mut self, events: &mut Vec<SseEvent>) {
        if !self.in_text_block {
            return;
        }
        let stop = serde_json::json!({
            "type": "content_block_stop",
            "index": self.block_index,
        });
        events.push(SseEvent::new(Some("content_block_stop"), stop.to_string()));
        self.block_index += 1;
        self.in_text_block = false;
    }

    fn close_thinking_block_if_open(&mut self, events: &mut Vec<SseEvent>) {
        if !self.in_thinking_block {
            return;
        }
        let stop = serde_json::json!({
            "type": "content_block_stop",
            "index": self.block_index,
        });
        events.push(SseEvent::new(Some("content_block_stop"), stop.to_string()));
        self.block_index += 1;
        self.in_thinking_block = false;
    }

    fn close_tool_block_if_open(&mut self, events: &mut Vec<SseEvent>) {
        if !self.in_tool_block {
            return;
        }
        let stop = serde_json::json!({
            "type": "content_block_stop",
            "index": self.block_index,
        });
        events.push(SseEvent::new(Some("content_block_stop"), stop.to_string()));
        self.block_index += 1;
        self.in_tool_block = false;
    }
}

fn extract_anthropic_usage(v: &Value) -> Usage {
    let Some(u) = v.get("usage") else {
        return Usage::default();
    };
    let get_opt_u32 = |key: &str| u.get(key).and_then(|v| v.as_u64()).map(|n| n as u32);
    let input_tokens = get_opt_u32("input_tokens");
    let output_tokens = get_opt_u32("output_tokens");
    let cache_read_tokens = get_opt_u32("cache_read_input_tokens");
    let cache_creation_tokens = get_opt_u32("cache_creation_input_tokens");
    let prompt_tokens = input_tokens
        .unwrap_or(0)
        .saturating_add(cache_read_tokens.unwrap_or(0))
        .saturating_add(cache_creation_tokens.unwrap_or(0));
    let server_tool_use = u.get("server_tool_use").map(|stu| ServerToolUsage {
        web_search_requests: stu
            .get("web_search_requests")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        web_fetch_requests: stu
            .get("web_fetch_requests")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
    });
    Usage {
        prompt_tokens,
        completion_tokens: output_tokens.unwrap_or(0),
        total_tokens: input_tokens
            .zip(output_tokens)
            .map(|(_, output)| prompt_tokens.saturating_add(output))
            .unwrap_or(0),
        required_components_known: input_tokens.is_some() && output_tokens.is_some(),
        cache_read_tokens,
        cache_creation_tokens,
        server_tool_use,
        ..Usage::default()
    }
}

fn anthropic_input_tokens(usage: &Usage) -> u32 {
    usage
        .prompt_tokens
        .saturating_sub(usage.cache_read_tokens.unwrap_or(0))
        .saturating_sub(usage.cache_creation_tokens.unwrap_or(0))
}

/// Append optional Anthropic-specific usage fields to an existing JSON usage object.
/// Omits keys whose values are `None`.
fn extend_usage_json(obj: &mut Value, u: &Usage) {
    if let Some(v) = u.cache_read_tokens {
        obj["cache_read_input_tokens"] = v.into();
    }
    if let Some(v) = u.cache_creation_tokens {
        obj["cache_creation_input_tokens"] = v.into();
    }
    if let Some(ref stu) = u.server_tool_use {
        obj["server_tool_use"] = serde_json::json!({
            "web_search_requests": stu.web_search_requests,
            "web_fetch_requests": stu.web_fetch_requests,
        });
    }
}

#[cfg(test)]
mod tests;

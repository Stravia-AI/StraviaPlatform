use super::types::GoogleFunctionCall;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::protocol::ir::request::ToolCall;
use crate::protocol::ir::usage::Usage;
use crate::protocol::ir::{AiItem, AiResponse, AiStreamDelta};
use crate::protocol::*;

// ── Non-streaming response parser ──

pub struct GoogleResponseParser;

impl GoogleResponseParser {
    pub(crate) fn parse_response(&self, resp: Value) -> Result<AiResponse> {
        let candidate = resp
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        let content_obj = candidate.and_then(|c| c.get("content"));

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut items = Vec::new();

        if let Some(parts) = content_obj
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                    if is_plain_text_part(part) {
                        items.push(AiItem::output_text(t));
                    } else {
                        items.push(AiItem::unknown(part.clone()));
                    }
                    continue;
                }

                if let Some(fc) = part.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = fc
                        .get("args")
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    let call_id = format!("call_{}", uuid::Uuid::new_v4().simple());
                    let arguments = args.to_string();
                    tool_calls.push(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    if is_plain_function_call_part(part) {
                        items.push(AiItem::function_call(ToolCall {
                            id: call_id,
                            name,
                            arguments,
                        }));
                    } else {
                        items.push(AiItem::unknown(part.clone()));
                    }
                    continue;
                }

                items.push(AiItem::unknown(part.clone()));
            }
        }

        let stop_reason = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(|v| v.as_str())
            .map(|r| match r {
                "STOP" => "stop".to_string(),
                "MAX_TOKENS" => "length".to_string(),
                other => other.to_lowercase(),
            });

        let usage = extract_gemini_usage(&resp);

        let model = resp
            .get("modelVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let response_id = resp
            .get("responseId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("gen-{}", uuid::Uuid::new_v4().simple()));

        let mut ai_resp = AiResponse::new(response_id, model);
        ai_resp.items = items;
        ai_resp.stop_reason = stop_reason;
        ai_resp.usage = usage;
        preserve_google_response_metadata(&mut ai_resp, &resp);
        Ok(ai_resp)
    }
}

// ── Non-streaming response formatter ──

pub struct GoogleResponseFormatter;

impl GoogleResponseFormatter {
    pub(crate) fn format_response(&self, resp: &AiResponse) -> Value {
        let parts = google_parts_from_response(resp);

        let finish_reason = resp.stop_reason.as_deref().map(|r| match r {
            "stop" => "STOP",
            "length" => "MAX_TOKENS",
            "tool_calls" => "STOP",
            other => other,
        });

        let mut out = serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": parts},
                "finishReason": finish_reason,
            }],
            "usageMetadata": google_usage_metadata(resp)
        });
        add_preserved_google_response_metadata(&mut out, resp);
        out
    }
}

// ── Stream parser (upstream Gemini SSE → deltas) ──

#[derive(Debug, Deserialize)]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiStreamCandidate>,
    #[serde(rename = "modelVersion")]
    model_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamCandidate {
    content: Option<GeminiStreamContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamContent {
    #[serde(default)]
    parts: Vec<GeminiStreamPart>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GeminiStreamPart {
    Text {
        text: String,
        #[serde(default)]
        thought: Option<bool>,
        #[serde(rename = "thoughtSignature", default)]
        thought_signature: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GoogleFunctionCall,
        #[serde(rename = "thoughtSignature", default)]
        thought_signature: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Other(Value),
}

pub struct GoogleStreamParser {
    buffer: String,
    first: bool,
}

impl Default for GoogleStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            first: true,
        }
    }
}

impl GoogleStreamParser {
    pub(crate) fn parse_chunk(&mut self, raw: &str) -> Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();

        while let Some(pos) = self.buffer.find("\n\n") {
            let block = self.buffer[..pos].to_string();
            self.buffer = self.buffer[pos + 2..].to_string();

            let mut saw_sse_data = false;
            for line in block.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    saw_sse_data = true;
                    let chunk = serde_json::from_str::<Value>(data.trim())?;
                    parse_gemini_chunk(&chunk, &mut deltas, &mut self.first)?;
                }
            }

            let bare = block.trim();
            if !saw_sse_data && (bare.starts_with('{') || bare.starts_with('[')) {
                let chunk = serde_json::from_str::<Value>(bare)?;
                parse_gemini_chunk(&chunk, &mut deltas, &mut self.first)?;
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

fn parse_gemini_chunk(
    raw_chunk: &Value,
    deltas: &mut Vec<AiStreamDelta>,
    first: &mut bool,
) -> Result<()> {
    let chunk: GeminiStreamChunk = serde_json::from_value(raw_chunk.clone())?;
    if *first {
        *first = false;
        deltas.push(AiStreamDelta::MessageStart {
            id: format!("gen-{}", uuid::Uuid::new_v4().simple()),
            model: chunk.model_version.unwrap_or_default(),
        });
    }

    let candidate = chunk.candidates.first();
    if let Some(parts) = candidate
        .and_then(|candidate| candidate.content.as_ref())
        .map(|content| content.parts.as_slice())
    {
        for part in parts {
            match part {
                GeminiStreamPart::Text {
                    text,
                    thought,
                    thought_signature,
                    extra,
                } if extra.is_empty() => {
                    if thought.unwrap_or(false) || thought_signature.is_some() {
                        if !text.is_empty() {
                            deltas.push(AiStreamDelta::ThinkingDelta(text.clone()));
                        }
                        if let Some(signature) = thought_signature {
                            deltas.push(AiStreamDelta::ThinkingSignature(signature.clone()));
                        }
                    } else if !text.is_empty() {
                        deltas.push(AiStreamDelta::TextDelta(text.clone()));
                    }
                }
                GeminiStreamPart::FunctionCall {
                    function_call,
                    thought_signature,
                    extra,
                } if extra.is_empty() => {
                    if let Some(signature) = thought_signature {
                        deltas.push(AiStreamDelta::ThinkingSignature(signature.clone()));
                    }
                    let id = function_call
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
                    deltas.push(AiStreamDelta::ToolCallStart {
                        index: 0,
                        id,
                        name: function_call.name.clone(),
                    });
                    let args = function_call.args.to_string();
                    if args != "{}" {
                        deltas.push(AiStreamDelta::ToolCallDelta {
                            index: 0,
                            arguments: args,
                        });
                    }
                }
                GeminiStreamPart::Text {
                    text,
                    thought,
                    thought_signature,
                    extra,
                } => {
                    let mut raw = extra.clone();
                    raw.insert("text".into(), Value::String(text.clone()));
                    if let Some(thought) = thought {
                        raw.insert("thought".into(), Value::Bool(*thought));
                    }
                    if let Some(signature) = thought_signature {
                        raw.insert("thoughtSignature".into(), Value::String(signature.clone()));
                    }
                    deltas.push(AiStreamDelta::Unknown {
                        raw: Value::Object(raw).to_string(),
                    });
                }
                GeminiStreamPart::FunctionCall {
                    function_call,
                    thought_signature,
                    extra,
                } => {
                    let mut raw = extra.clone();
                    raw.insert("functionCall".into(), serde_json::to_value(function_call)?);
                    if let Some(signature) = thought_signature {
                        raw.insert("thoughtSignature".into(), Value::String(signature.clone()));
                    }
                    deltas.push(AiStreamDelta::Unknown {
                        raw: Value::Object(raw).to_string(),
                    });
                }
                GeminiStreamPart::Other(raw) => {
                    if raw.as_object().is_some_and(|fields| {
                        fields.contains_key("text") || fields.contains_key("functionCall")
                    }) {
                        anyhow::bail!("invalid typed Gemini stream part: {raw}");
                    }
                    deltas.push(AiStreamDelta::Unknown {
                        raw: raw.to_string(),
                    });
                }
            }
        }
    }

    let usage = extract_gemini_usage(raw_chunk);
    if usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
        deltas.push(AiStreamDelta::Usage(usage));
    }
    if let Some(metadata) = google_stream_metadata(raw_chunk) {
        deltas.push(AiStreamDelta::Unknown {
            raw: serde_json::json!({"__google_response_metadata": metadata}).to_string(),
        });
    }

    if let Some(reason) = candidate.and_then(|candidate| candidate.finish_reason.as_deref()) {
        let normalized = match reason {
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            other => other,
        };
        deltas.push(AiStreamDelta::Done {
            stop_reason: normalized.to_string(),
        });
    }
    Ok(())
}

// ── Stream formatter (deltas → Gemini SSE) ──

pub struct GoogleStreamFormatter {
    usage: Usage,
    model: String,
    tool_names: HashMap<usize, String>,
    tool_ids: HashMap<usize, String>,
    tool_arg_buffers: HashMap<usize, String>,
    response_metadata: serde_json::Map<String, Value>,
}

impl Default for GoogleStreamFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleStreamFormatter {
    pub fn new() -> Self {
        Self {
            usage: Usage::default(),
            model: String::new(),
            tool_names: HashMap::new(),
            tool_ids: HashMap::new(),
            tool_arg_buffers: HashMap::new(),
            response_metadata: serde_json::Map::new(),
        }
    }
}

impl GoogleStreamFormatter {
    fn emit_thinking_signature(&self, events: &mut Vec<SseEvent>, signature: &str) {
        let chunk = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "text": "",
                        "thought": true,
                        "thoughtSignature": signature
                    }]
                },
            }],
            "modelVersion": self.model,
        });
        events.push(SseEvent::new(None, chunk.to_string()));
    }

    pub(crate) fn format_deltas(&mut self, deltas: &[AiStreamDelta]) -> Vec<SseEvent> {
        let mut events = Vec::new();

        for delta in deltas {
            match delta {
                AiStreamDelta::MessageStart { model, .. } => {
                    self.model = model.clone();
                }
                AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
                | AiStreamDelta::ReasoningSummaryDelta { text, .. } => {
                    let chunk = serde_json::json!({
                        "candidates": [{
                            "content": {
                                "role": "model",
                                "parts": [{"text": text, "thought": true}]
                            },
                        }],
                        "modelVersion": self.model,
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
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
                    let chunk = serde_json::json!({
                        "candidates": [{
                            "content": {"role": "model", "parts": [{"text": text}]},
                        }],
                        "modelVersion": self.model,
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::ToolCallStart { index, id, name } => {
                    self.tool_names.insert(*index, name.clone());
                    self.tool_ids.insert(*index, id.clone());
                    self.tool_arg_buffers.insert(*index, String::new());
                }
                AiStreamDelta::ToolCallDelta { index, arguments } => {
                    let Some(name) = self.tool_names.get(index).cloned() else {
                        continue;
                    };
                    let Some(id) = self.tool_ids.get(index).cloned() else {
                        continue;
                    };
                    let buf = self.tool_arg_buffers.entry(*index).or_default();
                    buf.push_str(arguments);
                    let Ok(args) = serde_json::from_str::<Value>(buf) else {
                        continue;
                    };
                    let normalized_args = normalize_tool_args(&name, args);
                    let chunk = serde_json::json!({
                        "candidates": [{
                            "content": {"role": "model", "parts": [{
                                "functionCall": {
                                    "id": id,
                                    "name": name,
                                    "args": normalized_args
                                }
                            }]},
                        }],
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::Usage(u) => {
                    if u.prompt_tokens > 0 {
                        self.usage.prompt_tokens = u.prompt_tokens;
                    }
                    if u.completion_tokens > 0 {
                        self.usage.completion_tokens = u.completion_tokens;
                    }
                    if u.total_tokens > 0 {
                        self.usage.total_tokens = u.total_tokens;
                    }
                    self.usage.required_components_known |= u.required_components_known;
                    if u.cache_read_tokens.is_some() {
                        self.usage.cache_read_tokens = u.cache_read_tokens;
                    }
                    if u.reasoning_tokens.is_some() {
                        self.usage.reasoning_tokens = u.reasoning_tokens;
                    }
                    if u.cache_creation_tokens.is_some() {
                        self.usage.cache_creation_tokens = u.cache_creation_tokens;
                    }
                    if u.server_tool_use.is_some() {
                        self.usage.server_tool_use = u.server_tool_use.clone();
                    }
                }
                AiStreamDelta::Unknown { raw } => {
                    let Ok(value) = serde_json::from_str::<Value>(raw) else {
                        continue;
                    };
                    if let Some(metadata) = value
                        .get("__google_response_metadata")
                        .and_then(Value::as_object)
                    {
                        merge_json_object(&mut self.response_metadata, metadata);
                        continue;
                    }
                    let chunk = serde_json::json!({
                        "candidates": [{
                            "content": {"role": "model", "parts": [value]},
                        }],
                        "modelVersion": self.model,
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::StreamError { error } => {
                    let error = serde_json::json!({
                        "error": {
                            "code": error.status_code.unwrap_or(500),
                            "message": error.message,
                            "status": error.kind.to_string(),
                        }
                    });
                    events.push(SseEvent::new(None, error.to_string()));
                }
                AiStreamDelta::Done { stop_reason } => {
                    let gemini_reason = match stop_reason.as_str() {
                        "stop" => "STOP",
                        "length" => "MAX_TOKENS",
                        other => other,
                    };
                    let mut chunk = serde_json::json!({
                        "candidates": [{
                            "content": {"role": "model", "parts": []},
                            "finishReason": gemini_reason,
                        }],
                        "usageMetadata": merge_usage_counts(
                            self.response_metadata
                                .get("usageMetadata")
                                .cloned()
                                .unwrap_or_else(|| google_usage_from_counts(&self.usage)),
                            &AiResponse {
                                usage: self.usage.clone(),
                                ..AiResponse::new("", self.model.clone())
                            },
                        )
                    });
                    add_stream_response_metadata(&mut chunk, &self.response_metadata);
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                _ => {}
            }
        }

        events
    }

    pub(crate) fn format_done(&mut self) -> Vec<SseEvent> {
        vec![]
    }
}

fn google_parts_from_response(resp: &AiResponse) -> Vec<Value> {
    let mut parts = Vec::new();
    for item in &resp.items {
        if let Some((summary, content, signature)) = item.reasoning_ref() {
            let text = summary.iter().chain(content).cloned().collect::<String>();
            let mut part = serde_json::json!({"text": text, "thought": true});
            if let Some(signature) = signature {
                part["thoughtSignature"] = Value::String(signature.to_owned());
            }
            parts.push(part);
        } else if let Some((thinking, signature)) = item.thinking_ref() {
            let mut part = serde_json::json!({"text": thinking, "thought": true});
            if let Some(signature) = signature {
                part["thoughtSignature"] = Value::String(signature.to_owned());
            }
            parts.push(part);
        } else if let Some(text) = item.output_text_ref()
            && !text.is_empty()
        {
            parts.push(serde_json::json!({"text": text}));
        } else if let Some(call) = item.function_call_ref() {
            let args: Value =
                serde_json::from_str(&call.arguments).unwrap_or(Value::Object(Default::default()));
            parts.push(serde_json::json!({
                "functionCall": {"id": call.id, "name": call.name, "args": args}
            }));
        } else if let Some(raw) = item.unknown_ref() {
            parts.push(raw.clone());
        }
    }
    parts
}

fn is_plain_text_part(part: &Value) -> bool {
    part.as_object()
        .is_some_and(|obj| obj.len() == 1 && obj.get("text").is_some_and(Value::is_string))
}

fn is_plain_function_call_part(part: &Value) -> bool {
    part.as_object()
        .is_some_and(|obj| obj.len() == 1 && obj.contains_key("functionCall"))
}

fn preserve_google_response_metadata(resp: &mut AiResponse, raw: &Value) {
    let mut metadata = serde_json::Map::new();
    if let Some(obj) = raw.as_object() {
        for (key, value) in obj {
            if key != "candidates" {
                metadata.insert(key.clone(), value.clone());
            }
        }
    }

    if let Some(candidate) = raw
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_object)
    {
        let mut candidate_extra = serde_json::Map::new();
        for (key, value) in candidate {
            if key != "content" && key != "finishReason" {
                candidate_extra.insert(key.clone(), value.clone());
            }
        }
        if !candidate_extra.is_empty() {
            metadata.insert(
                "__candidate_extra".to_string(),
                Value::Object(candidate_extra),
            );
        }

        if let Some(content) = candidate.get("content").and_then(Value::as_object) {
            let mut content_extra = serde_json::Map::new();
            for (key, value) in content {
                if key != "role" && key != "parts" {
                    content_extra.insert(key.clone(), value.clone());
                }
            }
            if !content_extra.is_empty() {
                metadata.insert("__content_extra".to_string(), Value::Object(content_extra));
            }
        }
    }

    if !metadata.is_empty() {
        resp.vendor.ingress.insert(
            "__google_response_metadata".to_string(),
            Value::Object(metadata),
        );
    }
}

fn add_preserved_google_response_metadata(out: &mut Value, resp: &AiResponse) {
    let Some(metadata) = resp
        .vendor
        .ingress
        .get("__google_response_metadata")
        .and_then(Value::as_object)
    else {
        if !resp.model.is_empty() {
            out.as_object_mut()
                .expect("Gemini response is an object")
                .insert(
                    "modelVersion".to_string(),
                    Value::String(resp.model.clone()),
                );
        }
        if !resp.id.is_empty() {
            out.as_object_mut()
                .expect("Gemini response is an object")
                .insert("responseId".to_string(), Value::String(resp.id.clone()));
        }
        return;
    };

    let obj = out.as_object_mut().expect("Gemini response is an object");
    for (key, value) in metadata {
        if key.starts_with("__") {
            continue;
        }
        if key == "usageMetadata" {
            obj.insert(
                "usageMetadata".to_string(),
                merge_usage_counts(value.clone(), resp),
            );
        } else {
            obj.insert(key.clone(), value.clone());
        }
    }
    if let Some(candidate_extra) = metadata.get("__candidate_extra").and_then(Value::as_object)
        && let Some(candidate) = out
            .get_mut("candidates")
            .and_then(Value::as_array_mut)
            .and_then(|arr| arr.first_mut())
            .and_then(Value::as_object_mut)
    {
        merge_json_object(candidate, candidate_extra);
    }
    if let Some(content_extra) = metadata.get("__content_extra").and_then(Value::as_object)
        && let Some(content) = out
            .get_mut("candidates")
            .and_then(Value::as_array_mut)
            .and_then(|arr| arr.first_mut())
            .and_then(|candidate| candidate.get_mut("content"))
            .and_then(Value::as_object_mut)
    {
        merge_json_object(content, content_extra);
    }
}

fn google_usage_metadata(resp: &AiResponse) -> Value {
    let preserved = resp
        .vendor
        .ingress
        .get("__google_response_metadata")
        .and_then(|m| m.get("usageMetadata"));
    if let Some(usage) = preserved {
        return merge_usage_counts(usage.clone(), resp);
    }

    google_usage_from_counts(&resp.usage)
}

fn merge_usage_counts(mut usage: Value, resp: &AiResponse) -> Value {
    let Some(obj) = usage.as_object_mut() else {
        return google_usage_metadata_fallback(resp);
    };
    let reasoning_tokens = resp.usage.reasoning_tokens.unwrap_or(0);
    obj.insert(
        "promptTokenCount".to_string(),
        resp.usage.prompt_tokens.into(),
    );
    obj.insert(
        "candidatesTokenCount".to_string(),
        resp.usage
            .completion_tokens
            .saturating_sub(reasoning_tokens)
            .into(),
    );
    obj.insert(
        "totalTokenCount".to_string(),
        if resp.usage.total_tokens > 0 {
            resp.usage.total_tokens
        } else {
            resp.usage.prompt_tokens + resp.usage.completion_tokens
        }
        .into(),
    );
    if reasoning_tokens > 0 {
        obj.insert("thoughtsTokenCount".to_string(), reasoning_tokens.into());
    }
    if let Some(cache_read_tokens) = resp.usage.cache_read_tokens {
        obj.entry("cachedContentTokenCount".to_string())
            .or_insert_with(|| cache_read_tokens.into());
    }
    usage
}

fn google_usage_metadata_fallback(resp: &AiResponse) -> Value {
    google_usage_from_counts(&resp.usage)
}

fn google_usage_from_counts(usage: &Usage) -> Value {
    let reasoning_tokens = usage.reasoning_tokens.unwrap_or(0);
    let candidate_tokens = usage.completion_tokens.saturating_sub(reasoning_tokens);
    let mut metadata = serde_json::json!({
        "promptTokenCount": usage.prompt_tokens,
        "candidatesTokenCount": candidate_tokens,
        "totalTokenCount": usage.prompt_tokens + usage.completion_tokens,
    });
    if reasoning_tokens > 0 {
        metadata["thoughtsTokenCount"] = reasoning_tokens.into();
    }
    if let Some(cache_read_tokens) = usage.cache_read_tokens {
        metadata["cachedContentTokenCount"] = cache_read_tokens.into();
    }
    metadata
}

fn merge_json_object(
    target: &mut serde_json::Map<String, Value>,
    source: &serde_json::Map<String, Value>,
) {
    for (key, value) in source {
        target.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn google_stream_metadata(chunk: &Value) -> Option<Value> {
    let mut metadata = serde_json::Map::new();
    if let Some(obj) = chunk.as_object() {
        for (key, value) in obj {
            if key != "candidates" {
                metadata.insert(key.clone(), value.clone());
            }
        }
    }
    if metadata.is_empty() {
        None
    } else {
        Some(Value::Object(metadata))
    }
}

fn add_stream_response_metadata(out: &mut Value, metadata: &serde_json::Map<String, Value>) {
    let obj = out
        .as_object_mut()
        .expect("Gemini stream chunk is an object");
    for (key, value) in metadata {
        if key == "usageMetadata" || key == "candidates" {
            continue;
        }
        obj.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn extract_gemini_usage(v: &Value) -> Usage {
    let usage = v
        .get("usageMetadata")
        .or_else(|| v.get("usage_metadata"))
        .or_else(|| v.get("usage"));
    let Some(u) = usage else {
        return Usage::default();
    };

    let input = first_u64(
        u,
        &[
            "promptTokenCount",
            "prompt_tokens",
            "inputTokenCount",
            "input_tokens",
        ],
    );
    let total = first_u64(u, &["totalTokenCount", "total_tokens"]);
    let candidate_output = first_u64(
        u,
        &[
            "candidatesTokenCount",
            "completion_tokens",
            "outputTokenCount",
            "output_tokens",
        ],
    );
    let thoughts = first_u64(
        u,
        &["thoughtsTokenCount", "reasoning_tokens", "thought_tokens"],
    )
    .unwrap_or(0);
    let cache_read = first_u64(
        u,
        &["cachedContentTokenCount", "cached_content_token_count"],
    );
    let output = input.and_then(|input| {
        total
            .and_then(|total| total.checked_sub(input))
            .or_else(|| candidate_output.map(|output| output.saturating_add(thoughts)))
    });

    Usage {
        prompt_tokens: input.unwrap_or(0) as u32,
        completion_tokens: output.unwrap_or(0) as u32,
        total_tokens: input
            .zip(output)
            .map(|(input, output)| total.unwrap_or(input.saturating_add(output)))
            .unwrap_or(0) as u32,
        required_components_known: input.is_some() && output.is_some(),
        cache_read_tokens: cache_read.map(|tokens| tokens as u32),
        ..Usage::default()
    }
}

fn first_u64(obj: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_u64()))
}

fn normalize_tool_args(tool_name: &str, mut args: Value) -> Value {
    let Some(obj) = args.as_object_mut() else {
        return args;
    };

    if let Some(v) = obj.get("exclude_patterns").cloned() {
        obj.insert(
            "exclude_patterns".to_string(),
            normalize_stringified_string_array(v),
        );
    }
    if let Some(v) = obj.remove("exclude_pattern") {
        let normalized = match v {
            Value::String(s) => Value::Array(vec![Value::String(s)]),
            other => normalize_stringified_string_array(other),
        };
        obj.entry("exclude_patterns".to_string())
            .or_insert(normalized);
    }

    match tool_name {
        "glob" => {
            if let Some(v) = obj.remove("include_pattern") {
                obj.entry("pattern".to_string()).or_insert(v);
            }
            if let Some(v) = obj.remove("path") {
                obj.entry("root_dir".to_string()).or_insert(v);
            }
            if let Some(v) = obj.remove("search_root") {
                obj.entry("root_dir".to_string()).or_insert(v);
            }
        }
        "list_directory" => {
            if let Some(v) = obj.remove("path") {
                obj.entry("dir_path".to_string()).or_insert(v);
            }
        }
        _ => {}
    }

    args
}

fn normalize_stringified_string_array(v: Value) -> Value {
    match v {
        Value::String(s) => {
            let parsed = serde_json::from_str::<Value>(&s).ok();
            if let Some(Value::Array(arr)) = parsed {
                let only_strings = arr.iter().all(|item| item.is_string());
                if only_strings {
                    return Value::Array(arr);
                }
            }
            Value::String(s)
        }
        Value::Array(arr) => Value::Array(arr),
        other => other,
    }
}

#[cfg(test)]
mod tests;

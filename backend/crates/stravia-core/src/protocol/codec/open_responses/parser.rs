use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::Value;

use crate::protocol::ir::request::ToolCall;
use crate::protocol::ir::usage::Usage;
use crate::protocol::ir::{
    AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, AiResponse, AiStreamDelta,
    ContentBlock, MessageContent, Role,
};

pub(super) const DATED_EVENT_TYPES: &[&str] = &[
    "error",
    "response.completed",
    "response.content_part.added",
    "response.content_part.done",
    "response.created",
    "response.failed",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.in_progress",
    "response.incomplete",
    "response.output_item.added",
    "response.output_item.done",
    "response.output_text.annotation.added",
    "response.output_text.delta",
    "response.output_text.done",
    "response.queued",
    "response.reasoning.delta",
    "response.reasoning.done",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.reasoning_summary_text.delta",
    "response.reasoning_summary_text.done",
    "response.refusal.delta",
    "response.refusal.done",
];

// OpenAI's rolling Responses transport emits these equivalents while Stravia
// continues to expose the fixed 2026-04-24 protocol to its own clients.
const ROLLING_RESPONSES_EVENT_TYPES: &[&str] = &[
    "response.reasoning_text.delta",
    "response.reasoning_text.done",
];

#[derive(serde::Deserialize)]
struct DatedResponseResource {
    id: String,
    object: String,
    #[serde(rename = "created_at")]
    _created_at: i64,
    #[serde(rename = "completed_at")]
    _completed_at: Option<i64>,
    status: String,
    #[serde(rename = "incomplete_details")]
    _incomplete_details: Option<Value>,
    model: String,
    #[serde(rename = "previous_response_id")]
    _previous_response_id: Option<String>,
    #[serde(rename = "instructions")]
    _instructions: Option<Value>,
    output: Vec<Value>,
    #[serde(rename = "error")]
    _error: Option<Value>,
    #[serde(rename = "tools")]
    _tools: Vec<Value>,
    #[serde(rename = "tool_choice")]
    _tool_choice: Value,
    #[serde(rename = "truncation")]
    _truncation: String,
    #[serde(rename = "parallel_tool_calls")]
    _parallel_tool_calls: bool,
    #[serde(rename = "text")]
    _text: Value,
    #[serde(rename = "top_p")]
    _top_p: Option<f64>,
    #[serde(rename = "presence_penalty")]
    _presence_penalty: Option<f64>,
    #[serde(rename = "frequency_penalty")]
    _frequency_penalty: Option<f64>,
    #[serde(rename = "top_logprobs")]
    _top_logprobs: Option<u32>,
    #[serde(rename = "temperature")]
    _temperature: Option<f64>,
    #[serde(rename = "reasoning")]
    _reasoning: Option<Value>,
    #[serde(rename = "usage")]
    _usage: Option<Value>,
    #[serde(rename = "max_output_tokens")]
    _max_output_tokens: Option<u32>,
    #[serde(rename = "max_tool_calls")]
    _max_tool_calls: Option<u32>,
    #[serde(rename = "store")]
    _store: bool,
    #[serde(rename = "background")]
    _background: bool,
    #[serde(rename = "service_tier")]
    _service_tier: String,
    #[serde(rename = "metadata")]
    _metadata: Value,
    #[serde(rename = "safety_identifier")]
    _safety_identifier: Option<String>,
    #[serde(rename = "prompt_cache_key")]
    _prompt_cache_key: Option<String>,
    #[serde(default)]
    #[serde(rename = "moderation")]
    _moderation: Option<Value>,
    #[serde(default)]
    #[serde(rename = "prompt_cache_retention")]
    _prompt_cache_retention: Option<String>,
    #[serde(default)]
    #[serde(rename = "prompt_cache_options")]
    _prompt_cache_options: Option<Value>,
    #[serde(default)]
    #[serde(rename = "tool_usage")]
    _tool_usage: Option<Value>,
    #[serde(default)]
    #[serde(rename = "user")]
    _user: Option<String>,
}

fn validate_core_output_item(item: &Value) -> Result<()> {
    let object = item
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("output item must be an object"))?;
    let item_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("output item type is missing"))?;
    if !matches!(item_type, "message" | "function_call" | "reasoning") {
        return Ok(());
    }
    required_non_empty_output_string(object, "id", item_type)?;

    match item_type {
        "message" => {
            require_output_status(object, item_type)?;
            if object.get("role").and_then(Value::as_str) != Some("assistant") {
                anyhow::bail!("output item message must have assistant role");
            }
            let content = object
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("output item message content must be an array"))?;
            for block in content {
                let block = block.as_object().ok_or_else(|| {
                    anyhow::anyhow!("output item message content block must be an object")
                })?;
                match block.get("type").and_then(Value::as_str) {
                    Some("output_text") => {
                        if block.get("text").and_then(Value::as_str).is_none()
                            || block
                                .get("annotations")
                                .is_some_and(|value| !value.is_array())
                            || block.get("logprobs").is_some_and(|value| !value.is_array())
                        {
                            anyhow::bail!("output item message output_text content is invalid");
                        }
                    }
                    Some("refusal") => {
                        if block.get("refusal").and_then(Value::as_str).is_none() {
                            anyhow::bail!("output item message refusal content is invalid");
                        }
                    }
                    _ => anyhow::bail!("output item message content type is unsupported"),
                }
            }
        }
        "function_call" => {
            require_output_status(object, item_type)?;
            required_non_empty_output_string(object, "call_id", item_type)?;
            required_non_empty_output_string(object, "name", item_type)?;
            let arguments = object
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("output item function_call missing arguments"))?;
            serde_json::from_str::<Value>(arguments).map_err(|error| {
                anyhow::anyhow!("output item function_call arguments are invalid JSON: {error}")
            })?;
        }
        "reasoning" => {
            let summary = object
                .get("summary")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("output item reasoning summary must be an array"))?;
            validate_reasoning_parts(summary, "summary_text", "summary")?;
            if let Some(content) = object.get("content").filter(|value| !value.is_null()) {
                validate_reasoning_parts(
                    content.as_array().ok_or_else(|| {
                        anyhow::anyhow!("output item reasoning content must be an array or null")
                    })?,
                    "reasoning_text",
                    "content",
                )?;
            }
            if object
                .get("encrypted_content")
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                anyhow::bail!("output item reasoning encrypted_content must be a string or null");
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn require_output_status(object: &serde_json::Map<String, Value>, item_type: &str) -> Result<()> {
    match object.get("status").and_then(Value::as_str) {
        Some("in_progress" | "completed" | "incomplete") => Ok(()),
        _ => anyhow::bail!("output item {item_type} has missing or invalid status"),
    }
}

fn validate_reasoning_parts(parts: &[Value], expected_type: &str, field: &str) -> Result<()> {
    for part in parts {
        let part = part.as_object().ok_or_else(|| {
            anyhow::anyhow!("output item reasoning {field} part must be an object")
        })?;
        if part.get("type").and_then(Value::as_str) != Some(expected_type)
            || part.get("text").and_then(Value::as_str).is_none()
        {
            anyhow::bail!("output item reasoning {field} part is invalid");
        }
    }
    Ok(())
}

fn required_non_empty_output_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
    item_type: &str,
) -> Result<()> {
    if object
        .get(field)
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        anyhow::bail!("output item {item_type} missing {field}");
    }
    Ok(())
}
fn parse_dated_response_resource(resp: &Value) -> Result<DatedResponseResource> {
    let object = resp
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Open Responses response must be an object"))?;
    for field in [
        "id",
        "object",
        "created_at",
        "completed_at",
        "status",
        "incomplete_details",
        "model",
        "previous_response_id",
        "instructions",
        "output",
        "error",
        "tools",
        "tool_choice",
        "truncation",
        "parallel_tool_calls",
        "text",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "top_logprobs",
        "temperature",
        "reasoning",
        "usage",
        "max_output_tokens",
        "max_tool_calls",
        "store",
        "background",
        "service_tier",
        "metadata",
        "safety_identifier",
        "prompt_cache_key",
    ] {
        if !object.contains_key(field) {
            anyhow::bail!(
                "invalid Open Responses 2026-04-24 response: missing required field '{field}'"
            );
        }
    }
    let wire = serde_json::from_value::<DatedResponseResource>(resp.clone())
        .map_err(|error| anyhow::anyhow!("invalid Open Responses 2026-04-24 response: {error}"))?;
    for item in &wire.output {
        validate_core_output_item(item)?;
    }
    if wire.object != "response" {
        anyhow::bail!("Open Responses response object must be 'response'");
    }
    if wire.id.is_empty() || wire.model.is_empty() {
        anyhow::bail!("Open Responses response id and model must be non-empty");
    }
    if !matches!(
        wire.status.as_str(),
        "queued" | "in_progress" | "completed" | "incomplete" | "failed"
    ) {
        anyhow::bail!(
            "invalid Open Responses 2026-04-24 response status '{}'",
            wire.status
        );
    }
    let effective = [
        "tools",
        "tool_choice",
        "truncation",
        "parallel_tool_calls",
        "text",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "top_logprobs",
        "temperature",
        "reasoning",
        "max_output_tokens",
        "max_tool_calls",
        "service_tier",
    ]
    .into_iter()
    .filter_map(|field| {
        let mut value = object.get(field)?.clone();
        if field == "tools" {
            strip_provider_function_output_schemas(&mut value);
        }
        if field == "reasoning"
            && let Some(reasoning) = value.as_object_mut()
        {
            reasoning.remove("context");
            reasoning.remove("mode");
        }
        Some((field.to_owned(), value))
    })
    .collect();
    super::decoder::decode_effective_response_profile(&wire.model, &effective)?;
    Ok(wire)
}

fn strip_provider_function_output_schemas(tools: &mut Value) {
    let Some(tools) = tools.as_array_mut() else {
        return;
    };
    for tool in tools {
        if tool.get("type").and_then(Value::as_str) == Some("function")
            && let Some(tool) = tool.as_object_mut()
        {
            tool.remove("output_schema");
        }
    }
}

pub struct ResponsesResponseParser;

impl ResponsesResponseParser {
    pub(crate) fn parse_response(&self, resp: Value) -> Result<AiResponse> {
        let wire = parse_dated_response_resource(&resp)?;
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
        let mut stop_reason = resp.get("status").and_then(|v| v.as_str()).map(|status| {
            match status {
                "completed" => "stop",
                "incomplete" => "length",
                other => other,
            }
            .to_string()
        });

        let mut saw_tool_call = false;
        let mut items = Vec::new();
        let with_wire_metadata = |canonical: AiItem, wire: &Value| {
            let status = match wire.get("status").and_then(Value::as_str) {
                Some("in_progress") => Some(AiItemStatus::InProgress),
                Some("completed") => Some(AiItemStatus::Completed),
                Some("incomplete") => Some(AiItemStatus::Incomplete),
                Some("failed") => Some(AiItemStatus::Failed),
                _ => None,
            };
            canonical.with_graph_metadata(
                wire.get("id").and_then(Value::as_str).map(str::to_owned),
                status,
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            )
        };

        if let Some(output_items) = resp.get("output").and_then(|v| v.as_array()) {
            for item in output_items {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "message" => {
                        let mut blocks = Vec::new();
                        if let Some(content_blocks) = item.get("content").and_then(Value::as_array)
                        {
                            for block in content_blocks {
                                match block.get("type").and_then(Value::as_str) {
                                    Some("output_text" | "text") => {
                                        let text = block
                                            .get("text")
                                            .and_then(Value::as_str)
                                            .ok_or_else(|| {
                                                anyhow::anyhow!("output_text content missing text")
                                            })?;
                                        blocks.push(ContentBlock::Text {
                                            text: text.to_owned(),
                                            cache_control: None,
                                        });
                                    }
                                    Some("refusal") => {
                                        let refusal = block
                                            .get("refusal")
                                            .and_then(Value::as_str)
                                            .ok_or_else(|| {
                                                anyhow::anyhow!("refusal content missing refusal")
                                            })?;
                                        blocks.push(ContentBlock::Refusal {
                                            refusal: refusal.to_owned(),
                                        });
                                    }
                                    Some(other) => anyhow::bail!(
                                        "unsupported Open Responses output content type: {other}"
                                    ),
                                    None => anyhow::bail!(
                                        "Open Responses output content type is missing"
                                    ),
                                }
                            }
                        }
                        let mut canonical = AiItem {
                            role: Role::Assistant,
                            content: MessageContent::Blocks(blocks),
                            tool_calls: None,
                            tool_call_id: None,
                            meta: Some(serde_json::json!({
                                "__open_responses_content": item.get("content").cloned().unwrap_or_default(),
                            })),
                        };
                        if let Some(phase) = item.get("phase")
                            && let Some(meta) =
                                canonical.meta.as_mut().and_then(Value::as_object_mut)
                        {
                            meta.insert("phase".into(), phase.clone());
                        }
                        items.push(with_wire_metadata(canonical, item));
                    }
                    "function_call" => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        serde_json::from_str::<Value>(&arguments).map_err(|error| {
                            anyhow::anyhow!("function_call arguments are invalid JSON: {error}")
                        })?;
                        if call_id.is_empty() || name.is_empty() {
                            anyhow::bail!("function_call requires non-empty call_id and name");
                        }
                        saw_tool_call = true;
                        items.push(with_wire_metadata(
                            AiItem::function_call(ToolCall {
                                id: call_id,
                                name,
                                arguments,
                            }),
                            item,
                        ));
                    }
                    "function_call_output" => {
                        let canonical =
                            crate::protocol::codec::open_responses::decoder::decode_input_item(
                                item,
                            )?
                            .ok_or_else(|| anyhow::anyhow!("function_call_output item is empty"))?;
                        items.push(with_wire_metadata(canonical, item));
                    }
                    "reasoning" => {
                        let summary = item
                            .get("summary")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .map(str::to_owned)
                            .collect();
                        let content = item
                            .get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .map(str::to_owned)
                            .collect();
                        let signature = item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        items.push(with_wire_metadata(
                            AiItem::reasoning(summary, content, signature),
                            item,
                        ));
                    }
                    extension_type if super::is_registered_extension_item(extension_type) => {
                        super::validate_extension_item(item, true)?;
                        items.push(with_wire_metadata(AiItem::unknown(item.clone()), item));
                    }
                    extension_type if super::is_namespaced_extension(extension_type) => {
                        anyhow::bail!(
                            "unregistered Open Responses output extension: {extension_type}"
                        );
                    }
                    other => anyhow::bail!("unsupported Open Responses output item type: {other}"),
                }
            }
        }
        if saw_tool_call {
            stop_reason = Some("tool_calls".to_string());
        }

        let usage = parse_dated_usage(&resp)?;

        let mut ai_resp = AiResponse::new(id, model);
        ai_resp.items = items;
        ai_resp.stop_reason = stop_reason;
        ai_resp.usage = usage;
        let mut effective = serde_json::Map::new();
        for field in [
            "tools",
            "tool_choice",
            "truncation",
            "parallel_tool_calls",
            "text",
            "top_p",
            "presence_penalty",
            "frequency_penalty",
            "top_logprobs",
            "temperature",
            "reasoning",
            "max_output_tokens",
            "max_tool_calls",
            "service_tier",
        ] {
            if let Some(value) = resp.get(field) {
                effective.insert(field.into(), value.clone());
            }
        }
        ai_resp.vendor.egress.insert(
            "__open_responses_provider_effective".into(),
            Value::Object(effective),
        );
        ai_resp.vendor.egress.insert(
            "__open_responses_terminal".into(),
            serde_json::json!({
                "status": wire.status,
                "error": wire._error,
                "incomplete_details": wire._incomplete_details
            }),
        );
        let item_index = resp
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                item.get("id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_owned(), item.clone()))
            })
            .collect::<serde_json::Map<_, _>>();
        ai_resp.vendor.ingress.insert(
            "__open_responses_item_index".into(),
            Value::Object(item_index),
        );
        Ok(ai_resp)
    }
}

pub struct ResponsesStreamParser {
    buffer: String,
    started: bool,
    started_tool_call_indexes: HashSet<usize>,
    streamed_tool_call_argument_indexes: HashSet<usize>,
    streamed_unknown_item_indexes: HashSet<usize>,
    open_items: HashMap<usize, (String, String)>,
    open_content_parts: HashMap<(usize, usize), String>,
    streamed_text: HashMap<(usize, usize), String>,
    streamed_refusal: HashMap<(usize, usize), String>,
    streamed_reasoning: HashMap<(usize, usize), String>,
    streamed_reasoning_summary: HashMap<(usize, usize), String>,
    next_sequence_number: u64,
    saw_error: bool,
    terminated: bool,
    saw_done: bool,
}
impl Default for ResponsesStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            started: false,
            started_tool_call_indexes: HashSet::new(),
            streamed_tool_call_argument_indexes: HashSet::new(),
            streamed_unknown_item_indexes: HashSet::new(),
            open_items: HashMap::new(),
            open_content_parts: HashMap::new(),
            streamed_text: HashMap::new(),
            streamed_refusal: HashMap::new(),
            streamed_reasoning: HashMap::new(),
            streamed_reasoning_summary: HashMap::new(),
            next_sequence_number: 0,
            saw_error: false,
            terminated: false,
            saw_done: false,
        }
    }
}

fn sse_frame_boundary(buffer: &str) -> Option<(usize, usize)> {
    let bytes = buffer.as_bytes();
    let mut previous_eol = None;
    let mut index = 0;
    while index < bytes.len() {
        let eol_len = match bytes[index] {
            b'\n' => 1,
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => {
                index += 1;
                continue;
            }
        };
        if let Some((previous_start, previous_end)) = previous_eol
            && previous_end == index
        {
            return Some((previous_start, index + eol_len - previous_start));
        }
        previous_eol = Some((index, index + eol_len));
        index += eol_len;
    }
    None
}

impl ResponsesStreamParser {
    pub(crate) fn parse_chunk(&mut self, raw: &str) -> Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();

        while let Some((pos, delimiter_len)) = sse_frame_boundary(&self.buffer) {
            let block = self.buffer[..pos].to_string();
            self.buffer = self.buffer[pos + delimiter_len..].to_string();

            let mut event_name: Option<String> = None;
            for line in block.lines() {
                if let Some(event) = line.strip_prefix("event: ") {
                    event_name = Some(event.trim().to_string());
                    continue;
                }
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    if !self.terminated {
                        anyhow::bail!("Open Responses SSE ended before a terminal response event");
                    }
                    self.saw_done = true;
                    continue;
                }
                let payload = serde_json::from_str::<Value>(data)
                    .map_err(|error| anyhow::anyhow!("invalid Open Responses SSE JSON: {error}"))?;
                self.parse_event(event_name.as_deref(), &payload, &mut deltas)?;
            }
        }

        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<AiStreamDelta>> {
        let deltas = if self.buffer.trim().is_empty() {
            Vec::new()
        } else {
            let remaining = std::mem::take(&mut self.buffer);
            self.parse_chunk(&format!("{remaining}\n\n"))?
        };
        // OpenAI's rolling Responses SSE ends after the terminal event and
        // omits the legacy [DONE] sentinel.
        if !self.terminated {
            anyhow::bail!("Open Responses SSE ended without a terminal event");
        }
        Ok(deltas)
    }
}

fn take_missing_suffix(
    streamed: &mut HashMap<(usize, usize), String>,
    output_index: usize,
    content_index: usize,
    completed: &str,
) -> Result<Option<String>> {
    let prior = streamed
        .remove(&(output_index, content_index))
        .unwrap_or_default();
    let Some(missing) = completed.strip_prefix(&prior) else {
        anyhow::bail!("completed stream content does not match its deltas");
    };
    Ok((!missing.is_empty()).then(|| missing.to_owned()))
}

impl ResponsesStreamParser {
    fn open_item_for_event(&self, payload: &Value) -> Result<(usize, &str)> {
        let output_index = required_stream_index(payload, "output_index", "stream item event")?;
        let item_id = payload
            .get("item_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("stream item event missing item_id"))?;
        let Some((open_id, item_type)) = self.open_items.get(&output_index) else {
            anyhow::bail!("stream event references no open output item");
        };
        if open_id != item_id {
            anyhow::bail!("stream event item_id does not match the open output item");
        }
        Ok((output_index, item_type))
    }

    fn open_content_part_for_event(&self, payload: &Value) -> Result<(usize, usize)> {
        let (output_index, _) = self.open_item_for_event(payload)?;
        let content_index =
            required_stream_index(payload, "content_index", "stream content event")?;
        if !self
            .open_content_parts
            .contains_key(&(output_index, content_index))
        {
            anyhow::bail!("stream content event references no open content part");
        }
        Ok((output_index, content_index))
    }
    fn require_open_content_part_type(
        &self,
        payload: &Value,
        expected_type: &str,
        event: &str,
    ) -> Result<(usize, usize)> {
        let key = self.open_content_part_for_event(payload)?;
        if self.open_content_parts.get(&key).map(String::as_str) != Some(expected_type) {
            anyhow::bail!("{event} does not match the open content part type");
        }
        Ok(key)
    }

    fn append_missing_completed_semantics(
        &mut self,
        output_index: usize,
        item: &AiItem,
        deltas: &mut Vec<AiStreamDelta>,
    ) -> Result<()> {
        if item.role == Role::Assistant {
            match &item.content {
                crate::protocol::ir::MessageContent::Text(text) => {
                    if let Some(text) =
                        take_missing_suffix(&mut self.streamed_text, output_index, 0, text)?
                    {
                        deltas.push(AiStreamDelta::TextDeltaWithMetadata {
                            text,
                            logprobs: Vec::new(),
                            obfuscation: None,
                            output_index: Some(output_index),
                            content_index: Some(0),
                        });
                    }
                }
                crate::protocol::ir::MessageContent::Blocks(blocks) => {
                    for (content_index, block) in blocks.iter().enumerate() {
                        match block {
                            ContentBlock::Text { text, .. } => {
                                if let Some(text) = take_missing_suffix(
                                    &mut self.streamed_text,
                                    output_index,
                                    content_index,
                                    text,
                                )? {
                                    deltas.push(AiStreamDelta::TextDeltaWithMetadata {
                                        text,
                                        logprobs: Vec::new(),
                                        obfuscation: None,
                                        output_index: Some(output_index),
                                        content_index: Some(content_index),
                                    });
                                }
                            }
                            ContentBlock::Refusal { refusal } => {
                                if let Some(text) = take_missing_suffix(
                                    &mut self.streamed_refusal,
                                    output_index,
                                    content_index,
                                    refusal,
                                )? {
                                    deltas.push(AiStreamDelta::RefusalDeltaWithIndex {
                                        text,
                                        output_index,
                                        content_index,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if let Some((summary, content, _)) = item.reasoning_ref() {
            for (content_index, text) in summary.iter().enumerate() {
                if let Some(text) = take_missing_suffix(
                    &mut self.streamed_reasoning_summary,
                    output_index,
                    content_index,
                    text,
                )? {
                    deltas.push(AiStreamDelta::ReasoningSummaryDelta {
                        text,
                        obfuscation: None,
                        output_index: Some(output_index),
                        content_index: Some(content_index),
                    });
                }
            }
            for (content_index, text) in content.iter().enumerate() {
                if let Some(text) = take_missing_suffix(
                    &mut self.streamed_reasoning,
                    output_index,
                    content_index,
                    text,
                )? {
                    deltas.push(AiStreamDelta::ThinkingDeltaWithMetadata {
                        text,
                        obfuscation: None,
                        output_index: Some(output_index),
                        content_index: Some(content_index),
                    });
                }
            }
        }
        for (label, map) in [
            ("output text", &self.streamed_text),
            ("refusal", &self.streamed_refusal),
            ("reasoning", &self.streamed_reasoning),
            ("reasoning summary", &self.streamed_reasoning_summary),
        ] {
            if map.keys().any(|(index, _)| *index == output_index) {
                anyhow::bail!("{label} delta has no matching completed content");
            }
        }
        Ok(())
    }

    fn parse_event(
        &mut self,
        event: Option<&str>,
        payload: &Value,
        deltas: &mut Vec<AiStreamDelta>,
    ) -> Result<()> {
        let event =
            event.ok_or_else(|| anyhow::anyhow!("Open Responses SSE event name missing"))?;
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Open Responses SSE body type missing"))?;
        if payload_type != event {
            anyhow::bail!(
                "Open Responses SSE event name '{event}' does not match body type '{payload_type}'"
            );
        }
        if !DATED_EVENT_TYPES.contains(&event) && !ROLLING_RESPONSES_EVENT_TYPES.contains(&event) {
            anyhow::bail!("unsupported Open Responses 2026-04-24 event '{event}'");
        }
        if self.terminated {
            anyhow::bail!("Open Responses SSE emitted '{event}' after a terminal event");
        }
        // OpenAI's rolling Responses SSE does not include the dated protocol's
        // sequence_number; preserve strict ordering when it is present.
        let sequence_number = payload
            .get("sequence_number")
            .and_then(Value::as_u64)
            .unwrap_or(self.next_sequence_number);
        if sequence_number != self.next_sequence_number {
            anyhow::bail!(
                "Open Responses SSE sequence_number {sequence_number} does not match {}",
                self.next_sequence_number
            );
        }
        self.next_sequence_number = self.next_sequence_number.saturating_add(1);
        if !self.started && !matches!(event, "response.queued" | "response.created" | "error") {
            anyhow::bail!("Open Responses SSE emitted '{event}' before response.created");
        }
        if matches!(
            event,
            "response.queued"
                | "response.in_progress"
                | "response.completed"
                | "response.incomplete"
                | "response.failed"
        ) {
            let response = payload
                .get("response")
                .ok_or_else(|| anyhow::anyhow!("{event} missing response"))?;
            let wire = parse_dated_response_resource(response)?;
            let expected_status = event
                .strip_prefix("response.")
                .expect("lifecycle event has response prefix");
            if wire.status != expected_status {
                anyhow::bail!("{event} response status does not match the event");
            }
        }
        match event {
            "response.created" => {
                if self.started {
                    anyhow::bail!("Open Responses SSE emitted response.created more than once");
                }
                let response = payload
                    .get("response")
                    .ok_or_else(|| anyhow::anyhow!("response.created missing response"))?;
                let wire = parse_dated_response_resource(response)?;
                if wire.status != "in_progress" {
                    anyhow::bail!("response.created response status does not match the event");
                }
                let id = wire.id;
                let model = wire.model;
                let metadata = [
                    "tools",
                    "tool_choice",
                    "truncation",
                    "parallel_tool_calls",
                    "text",
                    "top_p",
                    "presence_penalty",
                    "frequency_penalty",
                    "top_logprobs",
                    "temperature",
                    "reasoning",
                    "max_output_tokens",
                    "max_tool_calls",
                    "service_tier",
                ]
                .into_iter()
                .filter_map(|field| {
                    let mut value = response.get(field)?.clone();
                    if field == "tools" {
                        strip_provider_function_output_schemas(&mut value);
                    }
                    Some((field.to_owned(), value))
                })
                .collect();
                deltas.push(AiStreamDelta::ResponseMetadata {
                    metadata: Value::Object(metadata),
                });
                self.started = true;
                deltas.push(AiStreamDelta::MessageStart { id, model });
            }
            "response.in_progress" => {}
            "response.output_text.delta" => {
                let (output_index, content_index) =
                    self.require_open_content_part_type(payload, "output_text", event)?;
                let text = required_stream_text(payload, "delta", event)?;
                let logprobs = match payload.get("logprobs") {
                    None => Vec::new(),
                    Some(Value::Array(values)) => values.clone(),
                    Some(_) => anyhow::bail!("{event} has invalid logprobs"),
                };
                let obfuscation = match payload.get("obfuscation") {
                    None => None,
                    Some(Value::String(value)) => Some(value.clone()),
                    Some(_) => anyhow::bail!("{event} has invalid obfuscation"),
                };
                if !text.is_empty() {
                    self.streamed_text
                        .entry((output_index, content_index))
                        .or_default()
                        .push_str(text);
                    deltas.push(AiStreamDelta::TextDeltaWithMetadata {
                        text: text.to_owned(),
                        logprobs,
                        obfuscation,
                        output_index: Some(output_index),
                        content_index: Some(content_index),
                    });
                }
            }
            "response.reasoning_summary_text.delta" => {
                let (output_index, item_type) = self.open_item_for_event(payload)?;
                if item_type != "reasoning" {
                    anyhow::bail!("reasoning summary delta references no open reasoning item");
                }
                let content_index = required_stream_index(payload, "summary_index", event)?;
                let text = required_stream_text(payload, "delta", event)?;
                let obfuscation = match payload.get("obfuscation") {
                    None => None,
                    Some(Value::String(value)) => Some(value.clone()),
                    Some(_) => anyhow::bail!("{event} has invalid obfuscation"),
                };
                if !text.is_empty() {
                    self.streamed_reasoning_summary
                        .entry((output_index, content_index))
                        .or_default()
                        .push_str(text);
                    deltas.push(AiStreamDelta::ReasoningSummaryDelta {
                        text: text.to_owned(),
                        obfuscation,
                        output_index: Some(output_index),
                        content_index: Some(content_index),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let (index, item_type) = self.open_item_for_event(payload)?;
                if item_type != "function_call" {
                    anyhow::bail!(
                        "function call argument delta references no open function_call item"
                    );
                }
                let arguments = required_stream_text(payload, "delta", event)?;
                if !arguments.is_empty() {
                    self.streamed_tool_call_argument_indexes.insert(index);
                    deltas.push(AiStreamDelta::ToolCallDelta {
                        index,
                        arguments: arguments.to_owned(),
                    });
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let index = payload
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("output item event missing output_index"))?
                    as usize;
                let item = payload
                    .get("item")
                    .ok_or_else(|| anyhow::anyhow!("output item event missing item"))?;
                let item_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("output item type is missing"))?;
                if super::is_registered_extension_item(item_type) {
                    super::validate_extension_item(item, event == "response.output_item.done")?;
                }
                if event == "response.output_item.done" && item_type == "function_call" {
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("function_call arguments are missing"))?;
                    serde_json::from_str::<Value>(arguments).map_err(|error| {
                        anyhow::anyhow!("function_call arguments are invalid JSON: {error}")
                    })?;
                }
                if !matches!(
                    item_type,
                    "message" | "function_call" | "function_call_output" | "reasoning"
                ) && !super::is_registered_extension_item(item_type)
                {
                    anyhow::bail!("unsupported Open Responses output item type: {item_type}");
                }
                let item_id = item
                    .get("id")
                    .or_else(|| item.get("call_id"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("output item id is missing"))?
                    .to_owned();

                if event == "response.output_item.added" {
                    if self
                        .open_items
                        .insert(index, (item_id.clone(), item_type.to_owned()))
                        .is_some()
                    {
                        anyhow::bail!("output item index {index} was added more than once");
                    }
                    if item_type == "reasoning"
                        && item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.is_empty())
                    {
                        deltas.push(AiStreamDelta::ProtectedThinkingStart { index });
                    }
                } else {
                    if self
                        .open_content_parts
                        .iter()
                        .any(|((open_index, _), _)| *open_index == index)
                    {
                        anyhow::bail!(
                            "output item index {index} completed before its content parts"
                        );
                    }
                    let Some((open_id, open_type)) = self.open_items.remove(&index) else {
                        anyhow::bail!("output item index {index} completed before it was added");
                    };
                    if open_id != item_id || open_type != item_type {
                        anyhow::bail!("output item {index} completion does not match its addition");
                    }
                }
                if event == "response.output_item.done" {
                    let resource = super::formatter::response_resource_snapshot(
                        "stream_item",
                        "stream_model",
                        "completed",
                        vec![item.clone()],
                        Value::Null,
                        Value::Null,
                        Value::Null,
                    );
                    let mut completed = ResponsesResponseParser.parse_response(resource)?;
                    let canonical = completed.items.pop().ok_or_else(|| {
                        anyhow::anyhow!("completed output item has no canonical representation")
                    })?;
                    self.append_missing_completed_semantics(index, &canonical, deltas)?;
                    deltas.push(AiStreamDelta::ItemDone {
                        index,
                        item: canonical,
                    });
                }

                if super::is_registered_extension_item(item_type)
                    && event == "response.output_item.done"
                    && self.streamed_unknown_item_indexes.insert(index)
                {
                    deltas.push(AiStreamDelta::Unknown {
                        raw: item.to_string(),
                    });
                }
                if item_type == "function_call" {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("function_call call_id is missing"))?
                        .to_owned();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("function_call name is missing"))?
                        .to_owned();
                    if self.started_tool_call_indexes.insert(index) {
                        deltas.push(AiStreamDelta::ToolCallStart {
                            index,
                            id: call_id,
                            name,
                        });
                    }
                    if event == "response.output_item.done"
                        && !self.streamed_tool_call_argument_indexes.contains(&index)
                        && let Some(arguments) = item.get("arguments").and_then(Value::as_str)
                        && !arguments.is_empty()
                    {
                        self.streamed_tool_call_argument_indexes.insert(index);
                        deltas.push(AiStreamDelta::ToolCallDelta {
                            index,
                            arguments: arguments.to_owned(),
                        });
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                if !self.open_items.is_empty() || !self.open_content_parts.is_empty() {
                    anyhow::bail!(
                        "Open Responses terminal event arrived with incomplete output items"
                    );
                }
                let response = payload
                    .get("response")
                    .ok_or_else(|| anyhow::anyhow!("{event} missing response"))?;
                let expected_status = if event == "response.completed" {
                    "completed"
                } else {
                    "incomplete"
                };
                if response.get("status").and_then(Value::as_str) != Some(expected_status) {
                    anyhow::bail!("{event} response status does not match the event");
                }
                let response = payload.get("response").expect("validated response");
                let usage = parse_dated_usage(response)?;
                if usage.required_components_known {
                    deltas.push(AiStreamDelta::Usage(usage));
                }
                deltas.push(AiStreamDelta::ResponseTerminal {
                    status: expected_status.to_owned(),
                    incomplete_details: response.get("incomplete_details").cloned(),
                });
                self.terminated = true;
                deltas.push(AiStreamDelta::Done {
                    stop_reason: if event == "response.completed" {
                        "stop".to_string()
                    } else {
                        "length".to_string()
                    },
                });
            }
            "error" | "response.failed" => {
                let error = payload
                    .pointer("/response/error")
                    .or_else(|| payload.get("error"))
                    .unwrap_or(payload);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream Open Responses stream failed");
                let status = payload
                    .get("status")
                    .or_else(|| error.get("status"))
                    .or_else(|| error.get("status_code"))
                    .and_then(Value::as_u64)
                    .and_then(|value| u16::try_from(value).ok());
                if !self.saw_error {
                    self.saw_error = true;
                    let mut error = crate::protocol::ir::AiError::new(
                        crate::protocol::ir::AiErrorKind::StreamMidError,
                        message,
                    );
                    if let Some(status) = status {
                        error = error.with_status(status);
                    }
                    deltas.push(AiStreamDelta::StreamError { error });
                }
                if event == "response.failed" {
                    if payload.pointer("/response/status").and_then(Value::as_str) != Some("failed")
                    {
                        anyhow::bail!("response.failed response status does not match the event");
                    }
                    self.terminated = true;
                }
            }
            "response.refusal.delta" => {
                let (output_index, content_index) =
                    self.require_open_content_part_type(payload, "refusal", event)?;
                let text = payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("response.refusal.delta missing delta"))?;
                if !text.is_empty() {
                    self.streamed_refusal
                        .entry((output_index, content_index))
                        .or_default()
                        .push_str(text);
                    deltas.push(AiStreamDelta::RefusalDeltaWithIndex {
                        text: text.to_owned(),
                        output_index,
                        content_index,
                    });
                }
            }
            "response.reasoning.delta" | "response.reasoning_text.delta" => {
                let (output_index, item_type) = self.open_item_for_event(payload)?;
                if item_type != "reasoning" {
                    anyhow::bail!("reasoning delta references no open reasoning item");
                }
                let content_index = required_stream_index(payload, "content_index", event)?;
                let text = required_stream_text(payload, "delta", event)?;
                let obfuscation = match payload.get("obfuscation") {
                    None => None,
                    Some(Value::String(value)) => Some(value.clone()),
                    Some(_) => anyhow::bail!("{event} has invalid obfuscation"),
                };
                if !text.is_empty() {
                    self.streamed_reasoning
                        .entry((output_index, content_index))
                        .or_default()
                        .push_str(text);
                    deltas.push(AiStreamDelta::ThinkingDeltaWithMetadata {
                        text: text.to_owned(),
                        obfuscation,
                        output_index: Some(output_index),
                        content_index: Some(content_index),
                    });
                }
            }
            "response.output_text.annotation.added" => {
                self.require_open_content_part_type(payload, "output_text", event)?;
                required_stream_index(payload, "annotation_index", event)?;
                match payload.get("annotation") {
                    Some(Value::Null) => {}
                    Some(Value::Object(annotation))
                        if annotation.get("type").and_then(Value::as_str)
                            == Some("url_citation")
                            && annotation.get("url").is_some_and(Value::is_string)
                            && annotation.get("title").is_some_and(Value::is_string)
                            && annotation.get("start_index").is_some_and(Value::is_u64)
                            && annotation.get("end_index").is_some_and(Value::is_u64) => {}
                    _ => anyhow::bail!(
                        "{event} annotation must be null or a dated url_citation object"
                    ),
                }
                deltas.push(AiStreamDelta::Unknown {
                    raw: serde_json::json!({
                        "__open_responses_event": payload
                    })
                    .to_string(),
                });
            }
            "response.content_part.added" | "response.content_part.done" => {
                let (output_index, _) = self.open_item_for_event(payload)?;
                let content_index = required_stream_index(payload, "content_index", event)?;
                let part = payload
                    .get("part")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow::anyhow!("{event} missing content part"))?;
                let part_type = part
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|part_type| matches!(*part_type, "output_text" | "refusal"))
                    .ok_or_else(|| anyhow::anyhow!("{event} content part type is invalid"))?;
                let key = (output_index, content_index);
                if event == "response.content_part.added" {
                    if self
                        .open_content_parts
                        .insert(key, part_type.to_owned())
                        .is_some()
                    {
                        anyhow::bail!("content part was added more than once");
                    }
                } else {
                    let Some(open_type) = self.open_content_parts.remove(&key) else {
                        anyhow::bail!("content part completed before it was added");
                    };
                    if open_type != part_type {
                        anyhow::bail!("completed content part type does not match its addition");
                    }
                }
            }
            "response.output_text.done" => {
                self.require_open_content_part_type(payload, "output_text", event)?;
                required_stream_text(payload, "text", event)?;
                if payload
                    .get("logprobs")
                    .is_some_and(|value| !value.is_array())
                {
                    anyhow::bail!("{event} logprobs must be an array");
                }
            }
            "response.refusal.done" => {
                self.require_open_content_part_type(payload, "refusal", event)?;
                required_stream_text(payload, "refusal", event)?;
            }
            "response.reasoning.done" | "response.reasoning_text.done" => {
                let (_, item_type) = self.open_item_for_event(payload)?;
                if item_type != "reasoning" {
                    anyhow::bail!("{event} references no open reasoning item");
                }
                required_stream_index(payload, "content_index", event)?;
                required_stream_text(payload, "text", event)?;
            }
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
                let (_, item_type) = self.open_item_for_event(payload)?;
                if item_type != "reasoning" {
                    anyhow::bail!("{event} references no open reasoning item");
                }
                required_stream_index(payload, "summary_index", event)?;
                let part = payload
                    .get("part")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow::anyhow!("{event} missing summary part"))?;
                if part.get("type").and_then(Value::as_str).is_none() {
                    anyhow::bail!("{event} summary part type is missing");
                }
            }
            "response.reasoning_summary_text.done" => {
                let (_, item_type) = self.open_item_for_event(payload)?;
                if item_type != "reasoning" {
                    anyhow::bail!("{event} references no open reasoning item");
                }
                required_stream_index(payload, "summary_index", event)?;
                required_stream_text(payload, "text", event)?;
            }
            "response.function_call_arguments.done" => {
                let (_, item_type) = self.open_item_for_event(payload)?;
                if item_type != "function_call" {
                    anyhow::bail!("{event} references no open function_call item");
                }
                let arguments = required_stream_text(payload, "arguments", event)?;
                serde_json::from_str::<Value>(arguments).map_err(|error| {
                    anyhow::anyhow!("{event} arguments are invalid JSON: {error}")
                })?;
            }
            "response.queued" => {}
            _ => unreachable!("dated event allowlist and match must stay aligned"),
        }
        Ok(())
    }
}

fn required_stream_index(payload: &Value, field: &str, event: &str) -> Result<usize> {
    payload
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| anyhow::anyhow!("{event} missing or invalid {field}"))
}

fn required_stream_text<'a>(payload: &'a Value, field: &str, event: &str) -> Result<&'a str> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{event} missing or invalid {field}"))
}

fn parse_dated_usage(response: &Value) -> Result<Usage> {
    let Some(raw) = response.get("usage").filter(|value| !value.is_null()) else {
        return Ok(Usage::default());
    };
    let usage = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("response usage must be an object or null"))?;
    let required = |path: &str, value: Option<&Value>| -> Result<u32> {
        value
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| anyhow::anyhow!("response usage missing required '{path}'"))
    };
    Ok(Usage {
        prompt_tokens: required("input_tokens", usage.get("input_tokens"))?,
        completion_tokens: required("output_tokens", usage.get("output_tokens"))?,
        total_tokens: required("total_tokens", usage.get("total_tokens"))?,
        cache_read_tokens: Some(required(
            "input_tokens_details.cached_tokens",
            usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens")),
        )?),
        cache_creation_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        reasoning_tokens: Some(required(
            "output_tokens_details.reasoning_tokens",
            usage
                .get("output_tokens_details")
                .and_then(|details| details.get("reasoning_tokens")),
        )?),
        required_components_known: true,
        ..Usage::default()
    })
}

#[cfg(test)]
mod tests;

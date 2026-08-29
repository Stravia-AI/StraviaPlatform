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
mod tests {
    use super::*;
    use crate::protocol::ir::AiStreamDelta;

    fn sse_event(event: &str, data: &str) -> String {
        let mut payload: Value = serde_json::from_str(data).expect("SSE fixture JSON");
        if let Some(partial) = payload.get("response").cloned() {
            let id = partial
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("response");
            let model = partial
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("model");
            let status = partial
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress");
            let mut response = super::super::formatter::response_resource_snapshot(
                id,
                model,
                status,
                Vec::new(),
                Value::Null,
                Value::Null,
                Value::Null,
            );

            response
                .as_object_mut()
                .expect("response object")
                .extend(partial.as_object().expect("partial response").clone());
            payload["response"] = response;
        }
        format!("event: {event}\ndata: {payload}\n\n")
    }

    fn dated_response(partial: Value) -> Value {
        let mut response = super::super::formatter::response_resource_snapshot(
            "response",
            "model",
            "completed",
            Vec::new(),
            Value::Null,
            Value::Null,
            Value::Null,
        );
        response
            .as_object_mut()
            .expect("response object")
            .extend(partial.as_object().expect("partial object").clone());
        response
    }

    fn sse_data(data: &str) -> String {
        format!("data: {data}\n\n")
    }
    #[test]
    fn stream_rejects_partial_response_resource_snapshots() {
        let error = ResponsesStreamParser::new()
            .parse_chunk(
                "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"model\":\"model\",\"status\":\"in_progress\"}}\n\n",
            )
            .expect_err("partial response resource");

        assert!(
            error.to_string().contains("missing required field"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn stream_error_preserves_upstream_status() {
        let deltas = ResponsesStreamParser::new()
            .parse_chunk(
                "event: error\ndata: {\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"System messages are not allowed\"}}\n\n",
            )
            .expect("error event");

        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::StreamError { error }
                if error.status_code == Some(400)
                    && error.message == "System messages are not allowed"
        )));
    }
    #[test]
    fn stream_accepts_known_codex_response_resource_extensions() {
        let response = dated_response(serde_json::json!({
            "id": "resp_codex",
            "model": "gpt-5.4",
            "status": "in_progress",
            "moderation": null,
            "prompt_cache_retention": "24h",
            "tool_usage": {"web_search": {"num_requests": 0}},
            "user": null,
            "reasoning": {
                "effort": "none",
                "summary": null,
                "context": "current_turn",
                "mode": "standard"
            }
        }));
        let event = serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": response
        });

        ResponsesStreamParser::new()
            .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
            .expect("known Codex response extensions");
    }

    #[test]
    fn stream_accepts_gpt56_prompt_cache_options() {
        let response = dated_response(serde_json::json!({
            "id": "resp_gpt56",
            "model": "gpt-5.6-luna",
            "status": "in_progress",
            "prompt_cache_options": {
                "mode": "implicit",
                "ttl": "30m"
            }
        }));
        let event = serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": response
        });

        ResponsesStreamParser::new()
            .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
            .expect("GPT-5.6 prompt cache options");
    }

    #[test]
    fn stream_omits_provider_function_output_schema() {
        let response = dated_response(serde_json::json!({
            "id": "resp_gpt56",
            "model": "gpt-5.6-luna",
            "status": "in_progress",
            "tools": [{
                "type": "function",
                "name": "web_search",
                "description": "Search the public web.",
                "parameters": {"type": "object"},
                "output_schema": {"type": "object"}
            }]
        }));
        let event = serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": response
        });

        let deltas = ResponsesStreamParser::new()
            .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
            .expect("provider function output schema is response metadata");

        let metadata = deltas
            .iter()
            .find_map(|delta| match delta {
                AiStreamDelta::ResponseMetadata { metadata } => Some(metadata),
                _ => None,
            })
            .expect("response metadata");
        assert!(metadata["tools"][0].get("output_schema").is_none());
    }

    #[test]
    fn stream_accepts_openai_responses_events_without_sequence_numbers() {
        let response = dated_response(serde_json::json!({
            "id": "resp_openai",
            "model": "gpt-5.4-mini",
            "status": "in_progress"
        }));
        let event = serde_json::json!({
            "type": "response.created",
            "response": response
        });

        ResponsesStreamParser::new()
            .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
            .expect("OpenAI Responses omits sequence_number");
    }

    #[test]
    fn stream_accepts_rolling_reasoning_text_events() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-luna","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[]}}"#,
            ),
            sse_event(
                "response.reasoning_text.delta",
                r#"{"type":"response.reasoning_text.delta","sequence_number":2,"item_id":"rs_1","output_index":0,"content_index":0,"delta":"considering"}"#,
            ),
            sse_event(
                "response.reasoning_text.done",
                r#"{"type":"response.reasoning_text.done","sequence_number":3,"item_id":"rs_1","output_index":0,"content_index":0,"text":"considering"}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect("rolling reasoning text events");

        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::ThinkingDeltaWithMetadata {
                text,
                output_index: Some(0),
                content_index: Some(0),
                ..
            } if text == "considering"
        )));
    }

    #[test]
    fn stream_accepts_provider_reasoning_metadata() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-sol","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"internal_chat_message_metadata_passthrough":{"source":"codex"}}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"internal_chat_message_metadata_passthrough":{"source":"codex"}}}"#,
            ),
        ]
        .concat();

        ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect("provider reasoning metadata");
    }

    #[test]
    fn stream_accepts_provider_function_call_metadata() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-luna","status":"in_progress","provider_extension":true}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"todowrite","arguments":"","status":"in_progress","internal_chat_message_metadata_passthrough":{"create_time":1787534585.512904,"turn_id":"turn_1"},"metadata":{"turn_id":"turn_1"},"provider_extension":true}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"todowrite","arguments":"{\"todos\":[]}","status":"completed","internal_chat_message_metadata_passthrough":{"create_time":1787534585.512904,"turn_id":"turn_1"},"metadata":{"turn_id":"turn_1"},"provider_extension":true}}"#,
            ),
        ]
        .concat();

        ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect("provider function call metadata");
    }

    #[test]
    fn stream_preserves_encrypted_reasoning_from_completed_item() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-luna","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"opaque-reasoning"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"opaque-reasoning"}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect("encrypted reasoning item");

        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::ThinkingSignature(_)))
        );
        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::ItemDone { index: 0, item }
                if item.reasoning_ref().is_some_and(
                    |(_, _, encrypted)| encrypted == Some("opaque-reasoning")
                )
        )));
    }

    #[test]
    fn stream_accepts_openai_responses_without_done_sentinel() {
        let created = dated_response(serde_json::json!({
            "id": "resp_openai",
            "model": "gpt-5.4-mini",
            "status": "in_progress"
        }));
        let completed = dated_response(serde_json::json!({
            "id": "resp_openai",
            "model": "gpt-5.4-mini",
            "status": "completed"
        }));
        let mut parser = ResponsesStreamParser::new();
        parser
            .parse_chunk(&format!(
                "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{created}}}\n\n"
            ))
            .expect("created event");
        parser
            .parse_chunk(&format!(
                "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n"
            ))
            .expect("completed event");
        parser.finish().expect("terminal event is sufficient");
    }

    // ── ResponsesResponseParser ──

    #[test]
    fn test_parse_response_message_output() {
        let resp = dated_response(serde_json::json!({
            "id": "resp_1",
            "model": "gpt-4o",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello"}],
                    "internal_chat_message_metadata_passthrough": {"source": "codex"},
                    "metadata": {"kind": "assistant"}
                }
            ],
            "usage": {
                "input_tokens": 5,
                "output_tokens": 3,
                "total_tokens": 8,
                "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 2},
                "output_tokens_details": {"reasoning_tokens": 0}
            }
        }));
        let r = ResponsesResponseParser.parse_response(resp).unwrap();
        assert_eq!(r.output_text(), "hello");
        assert_eq!(r.stop_reason.as_deref(), Some("stop"));
        assert_eq!(r.usage.prompt_tokens, 5);
        assert_eq!(r.usage.cache_creation_tokens, Some(2));
    }
    #[test]
    fn test_parse_response_preserves_registered_items() {
        let resp = dated_response(serde_json::json!({
            "id": "resp_agent",
            "model": "gpt-4o",
            "status": "completed",
            "output": [{
                "id": "agent_1",
                "type": "stravia:agent_result",
                "status": "completed",
                "turn_id": "aturn_1"
            }, {
                "id": "media_1",
                "type": "stravia:media_result",
                "status": "completed",
                "turn_id": "aturn_media",
                "completion": "complete"
            }]
        }));

        let response = ResponsesResponseParser.parse_response(resp).unwrap();
        let items = &response.items;
        assert_eq!(items.len(), 2);
        let agent = items[0].unknown_ref().expect("agent result");
        assert_eq!(agent["type"], "stravia:agent_result");
        assert_eq!(agent["turn_id"], "aturn_1");
        let media = items[1].unknown_ref().expect("media result");
        assert_eq!(media["type"], "stravia:media_result");
        assert_eq!(media["turn_id"], "aturn_media");
        assert_eq!(media["completion"], "complete");
    }

    #[test]
    fn test_parse_response_rejects_unregistered_namespaced_items() {
        let resp = dated_response(serde_json::json!({
            "output": [{
                "id": "unknown_1",
                "type": "provider:future_item",
                "status": "completed",
                "payload": "must not cross the protocol boundary"
            }]
        }));

        let error = ResponsesResponseParser
            .parse_response(resp)
            .expect_err("unregistered output extension");
        assert!(
            error
                .to_string()
                .contains("unregistered Open Responses output extension")
        );
    }

    #[test]
    fn test_parse_response_with_encrypted_content_plaintext() {
        // Ollama's Responses API returns reasoning as plaintext in encrypted_content field.
        // The parser must not fail, and should extract text from the content array.
        let resp = dated_response(serde_json::json!({
            "id": "resp_2",
            "model": "qwen3",

            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "thinking..."}],
                    // encrypted_content is plaintext in Ollama — parser must not crash
                    "encrypted_content": "plaintext-not-base64"
                },
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "answer"}]
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "total_tokens": 30,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }
        }));
        let result = ResponsesResponseParser.parse_response(resp);
        assert!(
            result.is_ok(),
            "parser must not fail on plaintext encrypted_content"
        );
        let r = result.unwrap();
        assert_eq!(r.output_text(), "answer");
    }
    #[test]
    fn reasoning_summary_and_content_round_trip_without_merging() {
        let response = dated_response(serde_json::json!({
            "output": [{
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "short summary"}],
                "content": [{"type": "reasoning_text", "text": "full reasoning"}],
                "encrypted_content": "opaque"
            }]
        }));

        let canonical = ResponsesResponseParser
            .parse_response(response)
            .expect("parse reasoning item");
        let (summary, content, encrypted) = canonical.items[0]
            .reasoning_ref()
            .expect("typed reasoning item");
        assert_eq!(summary, ["short summary"]);
        assert_eq!(content, ["full reasoning"]);
        assert_eq!(encrypted, Some("opaque"));
        let formatted =
            super::super::formatter::ResponsesResponseFormatter.format_response(&canonical);

        assert_eq!(
            formatted["output"][0]["summary"],
            serde_json::json!([{"type": "summary_text", "text": "short summary"}])
        );
        assert_eq!(
            formatted["output"][0]["content"],
            serde_json::json!([{"type": "reasoning_text", "text": "full reasoning"}])
        );
    }

    #[test]
    fn rejects_schema_invalid_core_output_items() {
        for output in [
            serde_json::json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}]
            }),
            serde_json::json!({
                "type": "function_call",
                "id": "fc_1",
                "status": "completed",
                "call_id": "",
                "name": "lookup",
                "arguments": "{}"
            }),
            serde_json::json!({
                "type": "reasoning",
                "id": "rs_1",
                "status": "completed"
            }),
        ] {
            let error = ResponsesResponseParser
                .parse_response(dated_response(serde_json::json!({
                    "output": [output]
                })))
                .expect_err("schema-invalid output item");
            assert!(
                error.to_string().contains("output item"),
                "unexpected error: {error}"
            );
        }
    }
    #[test]
    fn test_parse_response_function_call_output() {
        let resp = dated_response(serde_json::json!({
            "id": "resp_3",
            "model": "gpt-4o",
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "status": "completed",
                    "call_id": "call_abc",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Paris\"}"
                }
            ],
            "usage": {
                "input_tokens": 15,
                "output_tokens": 10,
                "total_tokens": 25,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }
        }));
        let r = ResponsesResponseParser.parse_response(resp).unwrap();
        assert_eq!(r.tool_calls().count(), 1);
        let call = r.tool_calls().next().expect("function call");
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.name, "get_weather");
    }

    // ── ResponsesStreamParser ──

    #[test]
    fn test_stream_output_text_delta() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-4o","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"hello","logprobs":[{"token":"hello","logprob":-0.1}],"obfuscation":"pad"}"#,
            ),
            sse_event(
                "response.content_part.done",
                r#"{"type":"response.content_part.done","sequence_number":4,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"hello","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[],"logprobs":[]}]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":6,"response":{"id":"resp_1","model":"gpt-4o","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::TextDeltaWithMetadata {
                text,
                logprobs,
                obfuscation: Some(obfuscation),
                output_index: Some(0),
                content_index: Some(0),
            } if text == "hello"
                && logprobs[0]["token"] == "hello"
                && obfuscation == "pad"
        )));
        assert!(
            deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
        );
    }

    #[test]
    fn test_stream_reasoning_summary_text_delta() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_2","model":"qwen3","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
            ),
            sse_event(
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","sequence_number":2,"item_id":"rs_1","output_index":0,"summary_index":0,"delta":"thinking step","obfuscation":"pad"}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking step"}]}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":4,"output_index":1,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":5,"item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":6,"item_id":"msg_1","output_index":1,"content_index":0,"delta":"answer text"}"#,
            ),
            sse_event(
                "response.content_part.done",
                r#"{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","text":"answer text","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":8,"output_index":1,"item":{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"answer text","annotations":[],"logprobs":[]}]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":9,"response":{"id":"resp_2","model":"qwen3","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::ReasoningSummaryDelta {
                text,
                obfuscation: Some(obfuscation),
                output_index: Some(0),
                content_index: Some(0),
            } if text == "thinking step" && obfuscation == "pad"
        )));
        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::TextDeltaWithMetadata { text, .. } if text == "answer text"
        )));
    }

    #[test]
    fn item_done_only_summary_becomes_hook_visible_semantic_delta() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_summary","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_provider","summary":[],"content":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_provider","summary":[{"type":"summary_text","text":"late summary"}],"content":[]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":3,"response":{"id":"resp_summary","model":"model","status":"completed","output":[],"usage":null}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect("valid item-done summary");
        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::ReasoningSummaryDelta {
                text,
                output_index: Some(0),
                content_index: Some(0),
                ..
            } if text == "late summary"
        )));
        let events = super::super::stream::ResponsesStreamFormatter::new().format_deltas(&deltas);
        let added = events
            .iter()
            .position(|event| {
                event.event.as_deref() == Some("response.reasoning_summary_part.added")
            })
            .expect("summary part added");
        let done = events
            .iter()
            .position(|event| {
                event.event.as_deref() == Some("response.reasoning_summary_part.done")
            })
            .expect("summary part done");
        assert!(added < done);
    }

    #[test]
    fn refusal_delta_remains_distinct_from_output_text() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_refusal","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_refusal","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_refusal","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":""}}"#,
            ),
            sse_event(
                "response.refusal.delta",
                r#"{"type":"response.refusal.delta","sequence_number":3,"item_id":"msg_refusal","output_index":0,"content_index":0,"delta":"cannot comply"}"#,
            ),
        ]
        .concat();
        let deltas = ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect("dated refusal events");
        assert!(deltas.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::RefusalDeltaWithIndex {
                text,
                output_index: 0,
                content_index: 0,
            } if text == "cannot comply"
        )));
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::TextDelta(_)))
        );
    }

    #[test]
    fn test_done_sentinel_before_terminal_event_is_rejected() {
        let sse = sse_data("[DONE]");
        let mut parser = ResponsesStreamParser::new();
        let error = parser
            .parse_chunk(&sse)
            .expect_err("bare transport terminator must not invent completion");
        assert!(error.to_string().contains("before a terminal"));
    }

    #[test]
    fn rejects_rolling_or_mismatched_provider_events() {
        for sse in [
            sse_event(
                "response.future.delta",
                r#"{"type":"response.future.delta","sequence_number":0}"#,
            ),
            sse_event(
                "response.output_text.delta",
                r#"{"type":"response.refusal.delta","sequence_number":0,"delta":"no"}"#,
            ),
        ] {
            let error = ResponsesStreamParser::new()
                .parse_chunk(&sse)
                .expect_err("provider event must match the dated contract");
            assert!(
                error.to_string().contains("unsupported")
                    || error.to_string().contains("does not match")
            );
        }
    }

    #[test]
    fn rejects_malformed_done_event_references_and_payloads() {
        let message_prefix = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
        ]
        .concat();
        let mut parser = ResponsesStreamParser::new();
        parser.parse_chunk(&message_prefix).expect("message prefix");
        let error = parser
            .parse_chunk(&sse_event(
                "response.output_text.done",
                r#"{"type":"response.output_text.done","sequence_number":3,"item_id":"wrong","output_index":0,"content_index":0,"text":"answer","logprobs":[]}"#,
            ))
            .expect_err("done event with a mismatched item id");
        assert!(error.to_string().contains("item_id"));

        let function_prefix = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_2","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"","status":"in_progress"}}"#,
            ),
        ]
        .concat();
        let mut parser = ResponsesStreamParser::new();
        parser
            .parse_chunk(&function_prefix)
            .expect("function prefix");
        let error = parser
            .parse_chunk(&sse_event(
                "response.function_call_arguments.done",
                r#"{"type":"response.function_call_arguments.done","sequence_number":2,"item_id":"fc_1","output_index":0,"arguments":"{"}"#,
            ))
            .expect_err("done event with invalid final arguments");
        assert!(error.to_string().contains("arguments"));
    }

    #[test]
    fn test_stream_function_call_done_does_not_duplicate_start() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"fc_1","output_index":0,"delta":"{\"city\":\"Par"}"#,
            ),
            sse_event(
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"fc_1","output_index":0,"delta":"is\"}"}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
        assert_eq!(
            deltas
                .iter()
                .filter(|delta| matches!(delta, AiStreamDelta::ToolCallStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            deltas
                .iter()
                .filter_map(|delta| match delta {
                    AiStreamDelta::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            r#"{"city":"Paris"}"#
        );
    }

    #[test]
    fn test_stream_function_call_done_emits_arguments_when_no_deltas() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
        assert!(deltas.iter().any(
            |delta| matches!(delta, AiStreamDelta::ToolCallDelta { arguments, .. } if arguments == r#"{"city":"Paris"}"#)
        ));
    }
    #[test]
    fn test_stream_registered_items_round_trip_once_on_done() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":1,"item":{"id":"agent_1","type":"stravia:agent_result","status":"in_progress","turn_id":"aturn_1"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":1,"item":{"id":"agent_1","type":"stravia:agent_result","status":"completed","turn_id":"aturn_1"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":3,"output_index":2,"item":{"id":"media_1","type":"stravia:media_result","status":"in_progress","turn_id":"aturn_media","completion":"complete"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":4,"output_index":2,"item":{"id":"media_1","type":"stravia:media_result","status":"completed","turn_id":"aturn_media","completion":"complete"}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
        let results = deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::Unknown { raw } => Some(raw),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert!(results[0].contains(r#""type":"stravia:agent_result""#));
        assert!(results[1].contains(r#""type":"stravia:media_result""#));
    }

    #[test]
    fn non_stream_output_text_annotations_survive_same_protocol_round_trip() {
        let response = dated_response(serde_json::json!({
            "id": "resp_annotation",
            "model": "model",
            "output": [{
                "type": "message",
                "id": "msg_annotation",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "Example",
                    "annotations": [{
                        "type": "url_citation",
                        "start_index": 0,
                        "end_index": 7,
                        "url": "https://example.test",
                        "title": "Example"
                    }],
                    "logprobs": []
                }]
            }]
        }));
        let canonical = ResponsesResponseParser
            .parse_response(response)
            .expect("dated response");
        let encoded =
            super::super::formatter::ResponsesResponseFormatter.format_response(&canonical);

        assert_eq!(
            encoded["output"][0]["content"][0]["annotations"][0]["url"],
            "https://example.test"
        );
        assert_eq!(
            encoded["output"][0]["content"][0]["annotations"][0]["start_index"],
            0
        );
        assert_eq!(
            encoded["output"][0]["content"][0]["annotations"][0]["end_index"],
            7
        );
    }

    #[test]
    fn preserves_output_text_annotations_for_same_protocol_reencoding() {
        let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_provider","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_provider","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_provider","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_text.annotation.added",
                r#"{"type":"response.output_text.annotation.added","sequence_number":3,"item_id":"msg_provider","output_index":0,"content_index":0,"annotation_index":0,"annotation":{"type":"url_citation","start_index":0,"end_index":7,"url":"https://example.test","title":"Example"}}"#,
            ),
            sse_event(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_provider","output_index":0,"content_index":0,"delta":"Example","logprobs":[]}"#,
            ),
            sse_event(
                "response.content_part.done",
                r#"{"type":"response.content_part.done","sequence_number":5,"item_id":"msg_provider","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Example","annotations":[{"type":"url_citation","start_index":0,"end_index":7,"url":"https://example.test","title":"Example"}],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"type":"message","id":"msg_provider","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Example","annotations":[{"type":"url_citation","start_index":0,"end_index":7,"url":"https://example.test","title":"Example"}],"logprobs":[]}]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":7,"response":{"id":"resp_provider","model":"model","status":"completed","output":[],"usage":null}}"#,
            ),
        ]
        .concat();

        let deltas = ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect("dated annotation event");
        let events = super::super::stream::ResponsesStreamFormatter::new().format_deltas(&deltas);
        let annotation = events
            .iter()
            .find(|event| event.event.as_deref() == Some("response.output_text.annotation.added"))
            .expect("re-encoded annotation");
        let body: Value = serde_json::from_str(&annotation.data).expect("annotation JSON");

        assert_eq!(body["type"], "response.output_text.annotation.added");
        assert_eq!(body["sequence_number"], 5);
        assert_eq!(body["annotation"]["url"], "https://example.test");
        assert_ne!(body["item_id"], "msg_provider");
    }

    #[test]
    fn rejects_annotation_events_missing_required_dated_fields() {
        let malformed = [
            serde_json::json!({
                "type": "response.output_text.annotation.added",
                "sequence_number": 3,
                "item_id": "msg_provider",
                "output_index": 0,
                "content_index": 0,
                "annotation": null
            }),
            serde_json::json!({
                "type": "response.output_text.annotation.added",
                "sequence_number": 3,
                "item_id": "msg_provider",
                "output_index": 0,
                "content_index": 0,
                "annotation_index": 0
            }),
            serde_json::json!({
                "type": "response.output_text.annotation.added",
                "sequence_number": 3,
                "item_id": "msg_provider",
                "output_index": 0,
                "content_index": 0,
                "annotation_index": 0,
                "annotation": {"type": "url_citation", "url": "https://example.test"}
            }),
        ];

        for event in malformed {
            let sse = [
                sse_event(
                    "response.created",
                    r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_provider","model":"model","status":"in_progress"}}"#,
                ),
                sse_event(
                    "response.output_item.added",
                    r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_provider","status":"in_progress","role":"assistant","content":[]}}"#,
                ),
                sse_event(
                    "response.content_part.added",
                    r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_provider","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
                ),
                sse_event(
                    "response.output_text.annotation.added",
                    &event.to_string(),
                ),
            ]
            .concat();

            let error = ResponsesStreamParser::new()
                .parse_chunk(&sse)
                .expect_err("malformed dated annotation event");
            assert!(error.to_string().contains("annotation"));
        }
    }
    #[test]
    fn rejects_response_resource_missing_a_required_field() {
        let mut response = dated_response(serde_json::json!({
            "id": "resp_provider",
            "model": "logical-model"
        }));
        response
            .as_object_mut()
            .expect("response object")
            .remove("temperature");

        let error = ResponsesResponseParser
            .parse_response(response)
            .expect_err("missing required response field must fail");

        assert!(error.to_string().contains("temperature"));
    }
    #[test]
    fn rejects_unknown_response_status() {
        let error = ResponsesResponseParser
            .parse_response(dated_response(serde_json::json!({
                "status": "future_status"
            })))
            .expect_err("dated status enum is closed");

        assert!(error.to_string().contains("response status"));
    }

    #[test]
    fn sse_parser_handles_crlf_split_at_every_boundary() {
        let created = sse_event(
            "response.created",
            r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
        );
        let completed = sse_event(
            "response.completed",
            r#"{"type":"response.completed","sequence_number":1,"response":{"id":"resp_1","model":"model","status":"completed"}}"#,
        );
        let stream = format!("{created}{completed}data: [DONE]\n\n").replace('\n', "\r\n");

        for split in 0..=stream.len() {
            let mut parser = ResponsesStreamParser::new();
            let mut deltas = parser
                .parse_chunk(&stream[..split])
                .expect("first arbitrary chunk");
            deltas.extend(
                parser
                    .parse_chunk(&stream[split..])
                    .expect("second arbitrary chunk"),
            );
            deltas.extend(parser.finish().expect("complete dated stream"));
            assert!(
                deltas
                    .iter()
                    .any(|delta| matches!(delta, AiStreamDelta::Done { .. })),
                "missing terminal delta at byte split {split}"
            );
        }
    }
}

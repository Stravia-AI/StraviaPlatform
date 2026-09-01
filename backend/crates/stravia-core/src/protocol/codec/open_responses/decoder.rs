//! Open Responses 2026-04-24 ingress decoder — produces `AiRequest` directly.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
use crate::protocol::ir::{
    AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, AiRequest, ContentBlock,
    GenerationConfig, MediaSource, MessageContent, OpenResponsesExt, ProtocolExt, ReasoningConfig,
    ResponseFormat, Role, StreamConfig, ToolCall, ToolChoice, ToolSpec,
};

pub struct ResponsesDecoder;

// Fields decoded into named IR fields (not ingress bag).
const KNOWN_FIELDS: &[&str] = &[
    "model",
    "input",
    "previous_response_id",
    "include",
    "tools",
    "tool_choice",
    "metadata",
    "text",
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "parallel_tool_calls",
    "stream",
    "stream_options",
    "background",
    "max_output_tokens",
    "max_tool_calls",
    "reasoning",
    "safety_identifier",
    "prompt_cache_key",
    "truncation",
    "instructions",
    "store",
    "service_tier",
    "top_logprobs",
];

impl ResponsesDecoder {
    pub(crate) fn decode_request(&self, body: Value) -> Result<AiRequest> {
        let obj = body
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("request body must be a JSON object"))?;
        validate_field_types(obj)?;
        let passthrough_body = obj
            .iter()
            .filter(|(field, _)| !KNOWN_FIELDS.contains(&field.as_str()))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect();

        let previous_response_id = match obj.get("previous_response_id") {
            Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
            Some(Value::Null) | None => None,
            Some(_) => anyhow::bail!("'previous_response_id' must be a non-empty string or null"),
        };
        let model = match obj.get("model") {
            Some(Value::String(model)) if !model.is_empty() => model.clone(),
            Some(Value::Null) | None if previous_response_id.is_some() => String::new(),
            _ => anyhow::bail!("missing 'model' field"),
        };

        let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        let temperature = obj.get("temperature").and_then(|v| v.as_f64());
        let max_tokens = optional_u32(obj, "max_output_tokens")?;
        let top_p = obj.get("top_p").and_then(|v| v.as_f64());
        let presence_penalty = obj.get("presence_penalty").and_then(Value::as_f64);
        let frequency_penalty = obj.get("frequency_penalty").and_then(Value::as_f64);
        let parallel_tool_calls = obj.get("parallel_tool_calls").and_then(|v| v.as_bool());

        let instructions = match obj.get("instructions") {
            Some(Value::String(instructions)) => Some(instructions.clone()),
            Some(Value::Null) | None => None,
            Some(_) => anyhow::bail!("'instructions' must be a string or null"),
        };
        let mut messages: Vec<AiItem> = Vec::new();

        // ── Input items ───────────────────────────────────────────────────────
        let input = obj.get("input").filter(|value| !value.is_null());
        if let Some(input) = input {
            match input {
                Value::String(text) => {
                    messages.push(AiItem {
                        role: Role::User,
                        content: MessageContent::Text(text.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: None,
                    });
                }
                Value::Array(items) => {
                    let current_items = items
                        .iter()
                        .filter(|item| !is_item_reference(item))
                        .filter_map(|item| {
                            let id = item.get("id").and_then(Value::as_str)?;
                            decode_input_item(item).transpose().map(|decoded| {
                                decoded.map(|mut message| {
                                    set_input_graph_metadata(&mut message, item);
                                    (id.to_owned(), message)
                                })
                            })
                        })
                        .collect::<Result<HashMap<_, _>>>()?;
                    for item in items {
                        let is_reference = is_item_reference(item);
                        let decoded = if is_reference {
                            if let Some(message) = item
                                .get("id")
                                .and_then(Value::as_str)
                                .and_then(|id| current_items.get(id))
                            {
                                Some(message.clone())
                            } else {
                                decode_input_item(item)?
                            }
                        } else {
                            decode_input_item(item)?
                        };
                        if let Some(mut message) = decoded {
                            if !is_reference {
                                set_input_graph_metadata(&mut message, item);
                            }
                            messages.push(message);
                        }
                    }
                }
                _ => anyhow::bail!("'input' must be a string or array"),
            }
            if messages.is_empty() {
                anyhow::bail!("no messages found in input");
            }
        } else if previous_response_id.is_none() {
            anyhow::bail!("missing 'input' field");
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        let ParsedTools {
            tools,
            native_web_search,
            passthrough_tools,
        } = parse_tools(obj.get("tools"))?;
        let tool_choice = obj
            .get("tool_choice")
            .cloned()
            .map(parse_tool_choice)
            .transpose()?;

        // ── Reasoning ─────────────────────────────────────────────────────────
        let reasoning = if let Some(reasoning) = obj.get("reasoning") {
            let effort = reasoning
                .get("effort")
                .and_then(Value::as_str)
                .map(crate::protocol::ir::ReasoningEffort::from_openai_str)
                .transpose()?;
            let summary = reasoning
                .get("summary")
                .and_then(Value::as_str)
                .map(String::from);
            ReasoningConfig {
                enabled: true,
                effort,
                display: summary,
                level: reasoning
                    .get("effort")
                    .and_then(Value::as_str)
                    .map(crate::thinking::ThinkingLevel::from_wire)
                    .transpose()?,
                ..Default::default()
            }
        } else {
            ReasoningConfig::default()
        };
        let response_format = parse_response_format(obj.get("text"))?;

        // ── ProtocolExt ───────────────────────────────────────────────────────
        let resp_ext = OpenResponsesExt {
            instructions_present: obj.contains_key("instructions"),
            passthrough_body,
            passthrough_tools,
            background: obj.get("background").and_then(Value::as_bool),
            previous_response_id,
            include: obj.get("include").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            }),
            stream_options: obj.get("stream_options").cloned(),
            max_tool_calls: optional_u32(obj, "max_tool_calls")?,
            safety_identifier: obj
                .get("safety_identifier")
                .and_then(Value::as_str)
                .map(String::from),
            prompt_cache_key: obj
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .map(String::from),
            top_logprobs: optional_u32(obj, "top_logprobs")?,
            truncation: obj
                .get("truncation")
                .and_then(Value::as_str)
                .map(String::from),
            store: obj.get("store").and_then(Value::as_bool),
            metadata: obj.get("metadata").cloned(),
            text: obj.get("text").cloned(),
            service_tier: obj
                .get("service_tier")
                .and_then(Value::as_str)
                .map(String::from),
            native_web_search,
            tool_choice_ext: None,
        };

        // ── Build AiRequest ───────────────────────────────────────────────────
        let mut ai_req = AiRequest::new(model, messages);
        ai_req.instructions = instructions;
        ai_req.generation = GenerationConfig {
            temperature,
            max_tokens,
            top_p,
            presence_penalty,
            frequency_penalty,
            ..Default::default()
        };
        ai_req.stream = StreamConfig {
            enabled: stream,
            include_usage: false,
        };
        ai_req.tools = tools;
        ai_req.tool_choice = tool_choice;
        ai_req.parallel_tool_calls = parallel_tool_calls;
        ai_req.reasoning = reasoning;
        ai_req.response_format = response_format;
        ai_req.ext = Some(ProtocolExt::OpenResponses(resp_ext));
        ai_req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
        ai_req.meta.vendor.ingress = Default::default();

        Ok(ai_req)
    }
}

pub(crate) fn decode_effective_response_profile(
    model: &str,
    profile: &serde_json::Map<String, Value>,
) -> Result<AiRequest> {
    let mut body = profile.clone();
    body.insert("model".into(), Value::String(model.to_owned()));
    body.insert(
        "input".into(),
        Value::String("effective response profile".into()),
    );
    ResponsesDecoder.decode_request(Value::Object(body))
}

fn optional_u32(obj: &serde_json::Map<String, Value>, field: &str) -> Result<Option<u32>> {
    let Some(value) = obj.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("'{field}' must be an integer between 0 and {}", u32::MAX))
}

fn validate_field_types(obj: &serde_json::Map<String, Value>) -> Result<()> {
    for field in ["stream", "background", "store", "parallel_tool_calls"] {
        if let Some(value) = obj.get(field)
            && !value.is_null()
            && !value.is_boolean()
        {
            anyhow::bail!("'{field}' must be a boolean or null");
        }
    }
    for field in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if let Some(value) = obj.get(field)
            && !value.is_null()
            && !value.is_number()
        {
            anyhow::bail!("'{field}' must be a number or null");
        }
    }
    for field in ["max_output_tokens", "max_tool_calls", "top_logprobs"] {
        if let Some(value) = obj.get(field)
            && !value.is_null()
            && value.as_u64().is_none()
        {
            anyhow::bail!("'{field}' must be a non-negative integer or null");
        }
    }
    for field in [
        "safety_identifier",
        "prompt_cache_key",
        "truncation",
        "service_tier",
    ] {
        if let Some(value) = obj.get(field)
            && !value.is_null()
            && !value.is_string()
        {
            anyhow::bail!("'{field}' must be a string or null");
        }
    }
    for field in ["reasoning", "stream_options", "metadata", "text"] {
        if let Some(value) = obj.get(field)
            && !value.is_null()
            && !value.is_object()
        {
            anyhow::bail!("'{field}' must be an object or null");
        }
    }
    if let Some(value) = obj.get("include")
        && !value.is_null()
        && !value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
    {
        anyhow::bail!("'include' must be an array of strings or null");
    }
    if let Some(value) = obj.get("tools")
        && !value.is_null()
        && !value.is_array()
    {
        anyhow::bail!("'tools' must be an array or null");
    }
    if let Some(value) = obj.get("tool_choice")
        && !value.is_null()
        && !value.is_string()
        && !value.is_object()
    {
        anyhow::bail!("'tool_choice' must be a string, object, or null");
    }
    if let Some(reasoning) = obj.get("reasoning").and_then(Value::as_object) {
        if let Some(value) = reasoning.get("effort")
            && !value.is_null()
            && !value.as_str().is_some_and(|value| {
                matches!(
                    value,
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                )
            })
        {
            anyhow::bail!("unsupported 'reasoning.effort' value");
        }
        if let Some(value) = reasoning.get("summary")
            && !value.is_null()
            && !value.is_string()
        {
            anyhow::bail!("'reasoning.summary' must be a string or null");
        }
    }
    if let Some(stream_options) = obj.get("stream_options").and_then(Value::as_object) {
        if stream_options
            .get("include_obfuscation")
            .is_some_and(|value| !value.is_boolean())
        {
            anyhow::bail!("'stream_options.include_obfuscation' must be a boolean");
        }
    }
    if let Some(text) = obj.get("text").and_then(Value::as_object) {
        if let Some(verbosity) = text.get("verbosity")
            && !verbosity.is_null()
            && !verbosity
                .as_str()
                .is_some_and(|value| matches!(value, "low" | "medium" | "high"))
        {
            anyhow::bail!("unsupported 'text.verbosity' value");
        }
        if let Some(format) = text.get("format") {
            validate_text_format(format)?;
        }
    }
    Ok(())
}

fn validate_text_format(format: &Value) -> Result<()> {
    if format.is_null() {
        return Ok(());
    }
    let object = format
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("'text.format' must be an object or null"))?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => Ok(()),
        Some("json_schema") => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| {
                    (1..=64).contains(&name.len())
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("'text.format.name' must match ^[a-zA-Z0-9_-]{{1,64}}$")
                })?;
            let _ = name;
            if !object.get("schema").is_some_and(Value::is_object) {
                anyhow::bail!("'text.format.schema' must be an object");
            }
            if object
                .get("description")
                .is_some_and(|value| !value.is_string())
            {
                anyhow::bail!("'text.format.description' must be a string");
            }
            if object
                .get("strict")
                .is_some_and(|value| !value.is_null() && !value.is_boolean())
            {
                anyhow::bail!("'text.format.strict' must be a boolean or null");
            }
            Ok(())
        }
        _ => anyhow::bail!("unsupported 'text.format.type' value"),
    }
}

fn parse_response_format(text: Option<&Value>) -> Result<Option<ResponseFormat>> {
    let Some(format) = text
        .and_then(Value::as_object)
        .and_then(|text| text.get("format"))
        .filter(|format| !format.is_null())
    else {
        return Ok(None);
    };
    let object = format
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("'text.format' must be an object or null"))?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => Ok(Some(ResponseFormat::Text)),
        Some("json_schema") => Ok(Some(ResponseFormat::JsonSchema {
            name: object
                .get("name")
                .and_then(Value::as_str)
                .expect("validated text.format.name")
                .to_owned(),
            schema: object
                .get("schema")
                .expect("validated text.format.schema")
                .clone(),
            strict: object.get("strict").and_then(Value::as_bool),
        })),
        _ => anyhow::bail!("unsupported 'text.format.type' value"),
    }
}

fn is_item_reference(item: &Value) -> bool {
    match item.get("type") {
        Some(Value::String(item_type)) => item_type == "item_reference",
        None => item
            .as_object()
            .is_some_and(|item| item.len() == 1 && item.contains_key("id")),
        _ => false,
    }
}

fn set_input_graph_metadata(item: &mut AiItem, wire: &Value) {
    let id = wire
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let status = match wire.get("status").and_then(Value::as_str) {
        Some("in_progress") => Some(AiItemStatus::InProgress),
        Some("completed") => Some(AiItemStatus::Completed),
        Some("incomplete") => Some(AiItemStatus::Incomplete),
        Some("failed") => Some(AiItemStatus::Failed),
        _ => None,
    };
    item.set_graph_metadata(
        id,
        status,
        AiItemProvenance::Client,
        AiItemAudience::Provider,
    );
}

// ── Input item decoding ───────────────────────────────────────────────────────
pub(crate) fn decode_input_item(item: &Value) -> Result<Option<AiItem>> {
    let item_type = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            if is_item_reference(item) {
                "item_reference"
            } else {
                "message"
            }
        });
    item.as_object()
        .ok_or_else(|| anyhow::anyhow!("input item must be an object"))?;

    match item_type {
        "item_reference" => {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("item_reference missing 'id'"))?;
            Ok(Some(AiItem {
                role: Role::User,
                content: MessageContent::Text(String::new()),
                tool_calls: None,
                tool_call_id: None,
                meta: Some(serde_json::json!({
                    "__open_responses_item_reference": id
                })),
            }))
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
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            Ok(Some(AiItem::reasoning(summary, content, signature)))
        }
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("function_call_output missing 'call_id'"))?
                .to_string();
            let output = item
                .get("output")
                .ok_or_else(|| anyhow::anyhow!("function_call_output missing 'output'"))?;
            let content = match output {
                Value::String(text) => MessageContent::Text(text.clone()),
                Value::Array(_) => {
                    decode_message_item(
                        &serde_json::json!({
                            "role": "tool",
                            "content": output
                        }),
                        true,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("function_call_output content array is empty"))?
                    .content
                }
                _ => anyhow::bail!("function_call_output 'output' must be a string or array"),
            };
            Ok(Some(AiItem {
                role: Role::Tool,
                content,
                tool_calls: None,
                tool_call_id: Some(call_id),
                meta: None,
            }))
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
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            serde_json::from_str::<Value>(&arguments).map_err(|error| {
                anyhow::anyhow!("function_call arguments are invalid JSON: {error}")
            })?;
            if call_id.trim().is_empty() || name.trim().is_empty() {
                anyhow::bail!("function_call requires non-empty call_id and name");
            }
            Ok(Some(AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                tool_calls: Some(vec![ToolCall {
                    id: call_id,
                    name,
                    arguments,
                }]),
                tool_call_id: None,
                meta: None,
            }))
        }

        "message" => decode_message_item(item, false),
        other if super::is_registered_extension_item(other) => {
            super::validate_extension_item(item, true)?;
            Ok(Some(AiItem::unknown(item.clone())))
        }
        other if super::is_namespaced_extension(other) => {
            anyhow::bail!("unregistered input extension: {other}")
        }
        other => anyhow::bail!("unsupported Open Responses input item type: {other}"),
    }
}

fn decode_message_item(item: &Value, allow_video: bool) -> Result<Option<AiItem>> {
    let role_str = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
    let role = match role_str {
        "system" => Role::System,
        "developer" => Role::Developer,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => anyhow::bail!("unsupported role in responses input: {other}"),
    };

    let content = match item.get("content") {
        Some(Value::String(text)) => MessageContent::Text(text.clone()),
        Some(Value::Array(blocks)) => {
            let mut texts = Vec::new();
            let mut content_blocks = Vec::new();
            let mut has_media = false;
            for block in blocks {
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Responses content block type is required"))?;
                match block_type {
                    "input_text" | "output_text" | "text" => {
                        let object = block
                            .as_object()
                            .expect("content block with a type is an object");
                        let text = object.get("text").and_then(Value::as_str).ok_or_else(|| {
                            anyhow::anyhow!("Responses {block_type} content block text is required")
                        })?;
                        texts.push(text.to_owned());
                        content_blocks.push(ContentBlock::Text {
                            text: text.to_owned(),
                            cache_control: None,
                        });
                    }
                    "input_image" => {
                        let object = block
                            .as_object()
                            .expect("content block with a type is an object");
                        let image_url = object
                            .get("image_url")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Responses input_image requires a non-empty image_url"
                                )
                            })?;
                        let detail = match object.get("detail") {
                            None | Some(Value::Null) => None,
                            Some(Value::String(detail))
                                if matches!(detail.as_str(), "low" | "high" | "auto") =>
                            {
                                Some(detail.clone())
                            }
                            Some(Value::String(_)) => {
                                anyhow::bail!(
                                    "Responses input_image detail must be low, high, or auto"
                                )
                            }
                            Some(_) => {
                                anyhow::bail!(
                                    "Responses input_image detail must be a string or null"
                                )
                            }
                        };
                        let source =
                            crate::protocol::codec::parse_data_url_source(image_url.to_owned());
                        content_blocks.push(ContentBlock::Image {
                            source,
                            detail,
                            cache_control: None,
                        });
                        has_media = true;
                    }
                    "refusal" if role == Role::Assistant => {
                        let refusal = block
                            .get("refusal")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("refusal content requires refusal"))?;
                        content_blocks.push(ContentBlock::Refusal {
                            refusal: refusal.to_owned(),
                        });
                        has_media = true;
                    }
                    "input_file" => {
                        let source = if let Some(file_data) =
                            block.get("file_data").and_then(Value::as_str)
                        {
                            if file_data.starts_with("data:") {
                                crate::protocol::codec::parse_data_url_source(file_data.to_owned())
                            } else {
                                MediaSource::Base64 {
                                    media_type: "application/octet-stream".into(),
                                    data: file_data.to_owned(),
                                }
                            }
                        } else if let Some(file_url) = block.get("file_url").and_then(Value::as_str)
                        {
                            MediaSource::Url(file_url.to_owned())
                        } else {
                            anyhow::bail!("Responses input_file requires file_data or file_url");
                        };
                        content_blocks.push(ContentBlock::File {
                            source,
                            media_type: None,
                        });
                        has_media = true;
                    }
                    "input_video" if allow_video => {
                        let video_url = block
                            .get("video_url")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                anyhow::anyhow!("Responses input_video requires video_url")
                            })?;
                        let source =
                            crate::protocol::codec::parse_data_url_source(video_url.to_owned());
                        let media_type = match &source {
                            MediaSource::Base64 { media_type, .. } => Some(media_type.clone()),
                            MediaSource::Url(_) | MediaSource::FileId { .. } => None,
                        };
                        content_blocks.push(ContentBlock::Video { source, media_type });
                        has_media = true;
                    }
                    other => {
                        anyhow::bail!("unsupported Open Responses message content type: {other}")
                    }
                }
            }
            if has_media {
                MessageContent::Blocks(content_blocks)
            } else {
                let text = texts.join("");
                if text.is_empty() {
                    return Ok(None);
                }
                MessageContent::Text(text)
            }
        }
        Some(_) => anyhow::bail!("unsupported content type in responses input item"),
        None => return Ok(None),
    };

    let mut meta = serde_json::Map::new();
    if let Some(phase) = item.get("phase") {
        meta.insert("phase".into(), phase.clone());
    }
    Ok(Some(AiItem {
        role,
        content,
        tool_calls: None,
        tool_call_id: None,
        meta: (!meta.is_empty()).then_some(Value::Object(meta)),
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────
#[derive(Debug)]
struct ParsedTools {
    tools: Option<Vec<ToolSpec>>,
    native_web_search: Option<Value>,
    passthrough_tools: Vec<Value>,
}

fn parse_tools(raw_tools: Option<&Value>) -> Result<ParsedTools> {
    let Some(Value::Array(items)) = raw_tools else {
        return Ok(ParsedTools {
            tools: None,
            native_web_search: None,
            passthrough_tools: Vec::new(),
        });
    };

    let mut tools = Vec::new();
    let mut native_web_search = None;
    let mut passthrough_tools = Vec::new();
    for item in items {
        let tool_type = item
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("function");

        match tool_type {
            "function" => {
                item.as_object()
                    .ok_or_else(|| anyhow::anyhow!("function tool must be an object"))?;
                let name = item
                    .get("name")
                    .and_then(|value| value.as_str())
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("function tool missing 'name' field"))?
                    .to_string();
                if let Some(description) = item.get("description")
                    && !description.is_null()
                    && !description.is_string()
                {
                    anyhow::bail!("function tool 'description' must be a string or null");
                }
                if let Some(parameters) = item.get("parameters")
                    && !parameters.is_null()
                    && !parameters.is_object()
                {
                    anyhow::bail!("function tool 'parameters' must be an object or null");
                }
                let description = item
                    .get("description")
                    .and_then(|value| value.as_str())
                    .map(String::from);
                let parameters = item
                    .get("parameters")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                let strict = match item.get("strict") {
                    Some(Value::Bool(value)) => Some(*value),
                    Some(Value::Null) | None => None,
                    Some(_) => anyhow::bail!("function tool 'strict' must be a boolean or null"),
                };
                tools.push(ToolSpec {
                    name,
                    description,
                    parameters,
                    strict,
                    cache_control: None,
                    meta: None,
                });
            }
            "stravia:web_search" => {
                if native_web_search.is_some() {
                    anyhow::bail!("only one stravia:web_search tool may be declared");
                }
                native_web_search = Some(item.clone());
            }
            other if super::is_namespaced_extension(other) => {
                anyhow::bail!("unregistered Open Responses tool extension: {other}");
            }
            _ => passthrough_tools.push(item.clone()),
        }
    }

    Ok(ParsedTools {
        tools: Some(tools),
        native_web_search,
        passthrough_tools,
    })
}

fn parse_tool_choice(v: Value) -> Result<ToolChoice> {
    match &v {
        Value::String(s) => match s.as_str() {
            "none" => Ok(ToolChoice::None),
            "auto" => Ok(ToolChoice::Auto),
            "required" => Ok(ToolChoice::Required),
            _ => Ok(ToolChoice::Raw(v)),
        },
        Value::Object(obj) if obj.get("type").and_then(Value::as_str) == Some("function") => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow::anyhow!("function tool_choice missing 'name'"))?;
            Ok(ToolChoice::Named {
                name: name.to_owned(),
            })
        }
        Value::Object(_) => Ok(ToolChoice::Raw(v)),
        _ => anyhow::bail!("unsupported 'tool_choice' value"),
    }
}

#[cfg(test)]
mod tests;

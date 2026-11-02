use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use crate::SseEvent;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::request::ToolCall;
use stravia_runtime_contract::protocol::ir::usage::Usage;
use stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;

// ── Non-streaming response parser ──

pub struct OpenAIResponseParser;

/// Some OpenAI-compatible gateways (observed: ClinePass) wrap non-streaming
/// completions in an envelope `{"success": true, "data": {…completion…}}`.
/// Unwrap `data` only when the top level carries no `choices` itself, so
/// standard completions and error bodies pass through unchanged.
fn unwrap_gateway_envelope(resp: Value) -> Result<Value> {
    if resp.get("choices").is_some() {
        return Ok(resp);
    }
    if resp.get("success").and_then(Value::as_bool) == Some(false) {
        let detail = resp
            .pointer("/error/message")
            .or_else(|| resp.get("error"))
            .or_else(|| resp.get("message"))
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            });
        anyhow::bail!(
            "upstream gateway reported failure: {}",
            detail.unwrap_or_else(|| "success=false".to_string())
        );
    }
    match resp.get("data") {
        Some(data) if data.get("choices").and_then(Value::as_array).is_some() => Ok(data.clone()),
        _ => Ok(resp),
    }
}

impl OpenAIResponseParser {
    pub(crate) fn parse_response(&self, resp: Value) -> Result<AiResponse> {
        self.parse_response_with_reasoning_field(resp, None)
    }

    pub(crate) fn parse_response_with_reasoning_field(
        &self,
        resp: Value,
        reasoning_source: Option<&str>,
    ) -> Result<AiResponse> {
        let resp = unwrap_gateway_envelope(resp)?;
        if resp.get("choices").is_none() {
            tracing::warn!(
                top_level_keys = ?resp
                    .as_object()
                    .map(|o| o.keys().collect::<Vec<_>>())
                    .unwrap_or_default(),
                "chat completion response has no choices; returning empty output"
            );
        }
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

        let choice = resp
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        let message = choice.and_then(|c| c.get("message"));

        let content = message
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let reasoning_content = message
            .map(|message| extract_selected_reasoning(message, reasoning_source))
            .transpose()?
            .flatten();
        let empty_reasoning = message
            .and_then(|message| {
                selected_empty_reasoning_field(
                    message,
                    reasoning_content.as_deref(),
                    reasoning_source,
                )
            })
            .cloned();

        let stop_reason = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let tool_calls: Vec<ToolCall> = message
            .and_then(|m| m.get("tool_calls"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|tc| {
                        let func = tc.get("function")?;
                        Some(ToolCall {
                            id: (tc.get("id")?.as_str()?.to_string()).into(),
                            name: func.get("name")?.as_str()?.to_string(),
                            arguments: func
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        if stop_reason.as_deref() == Some("tool_calls") && tool_calls.is_empty() {
            anyhow::bail!("upstream tool_calls terminal contains no representable tool call");
        }

        let usage = extract_usage(&resp);

        let mut ai_resp = AiResponse::new(id, model);
        if let Some(reasoning) = reasoning_content {
            ai_resp.push_reasoning(reasoning, None);
        }
        ai_resp.push_output_text(content);
        ai_resp
            .items
            .extend(tool_calls.into_iter().map(AiItem::function_call));
        if let Some(value) = empty_reasoning {
            ai_resp
                .vendor
                .ingress
                .insert(CHAT_REASONING_FIELD_META.into(), value);
        }
        ai_resp.stop_reason = stop_reason;
        ai_resp.usage = usage;
        if let Some(index) = choice
            .and_then(|choice| choice.get("index"))
            .and_then(Value::as_u64)
            .filter(|index| *index != 0)
        {
            ai_resp
                .vendor
                .egress
                .insert("__openai_native_choice_index".into(), Value::from(index));
        }
        if let Some(choices) = resp.get("choices").and_then(Value::as_array)
            && choices.len() > 1
        {
            for choice in &choices[1..] {
                self.parse_response_with_reasoning_field(
                    serde_json::json!({"choices": [choice]}),
                    reasoning_source,
                )?;
            }
            ai_resp.vendor.egress.insert(
                "__openai_native_choices".into(),
                Value::Array(choices[1..].to_vec()),
            );
        }
        Ok(ai_resp)
    }
}

// ── Non-streaming response formatter ──

pub struct OpenAIResponseFormatter;

impl OpenAIResponseFormatter {
    pub(crate) fn format_response(&self, resp: &AiResponse) -> Value {
        let tool_calls: Vec<Value> = resp
            .tool_calls()
            .map(|call| {
                serde_json::json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    }
                })
            })
            .collect();
        let finish_reason = if tool_calls.is_empty() {
            resp.stop_reason.as_deref().map(chat_finish_reason)
        } else {
            Some("tool_calls")
        };
        let mut message = serde_json::json!({
            "role": "assistant",
            "content": resp.output_text(),
        });
        let refusal = resp.refusal_text();
        if !refusal.is_empty() {
            message
                .as_object_mut()
                .expect("message is an object")
                .insert("refusal".into(), Value::String(refusal));
        }
        let reasoning = visible_reasoning_text(resp);
        if !reasoning.is_empty() {
            message
                .as_object_mut()
                .expect("message is an object")
                .insert("reasoning_content".into(), Value::String(reasoning));
        } else if let Some(value) = empty_reasoning_metadata(resp) {
            message
                .as_object_mut()
                .expect("message is an object")
                .insert("reasoning_content".into(), value.clone());
        }
        if !tool_calls.is_empty() {
            message
                .as_object_mut()
                .expect("message is an object")
                .insert("tool_calls".into(), Value::Array(tool_calls));
        }

        let mut out = serde_json::json!({
            "id": resp.id,
            "object": "chat.completion",
            "model": resp.model,
            "choices": [{
                "index": resp.vendor.egress
                    .get("__openai_native_choice_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                "message": message,
                "finish_reason": finish_reason,
            }],
            "usage": openai_usage_json(&resp.usage)
        });
        let mut source_text = String::new();
        let mut logprobs = Vec::new();
        for item in &resp.items {
            if let Some(parts) = item
                .meta
                .as_ref()
                .and_then(|meta| meta.get("__open_responses_content"))
                .and_then(Value::as_array)
            {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        source_text
                            .push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                        if let Some(values) = part.get("logprobs").and_then(Value::as_array) {
                            logprobs.extend(values.iter().cloned());
                        }
                    }
                }
            }
        }
        if !logprobs.is_empty() && source_text == resp.output_text() {
            out["choices"][0]["logprobs"] = serde_json::json!({"content": logprobs});
        }
        if let Some(extra) = resp
            .vendor
            .egress
            .get("__openai_native_choices")
            .and_then(Value::as_array)
        {
            out["choices"]
                .as_array_mut()
                .expect("choices array")
                .extend(extra.iter().cloned());
        }
        out
    }
}

fn visible_reasoning_text(resp: &AiResponse) -> String {
    let mut reasoning = String::new();
    for item in &resp.items {
        if let Some((text, _)) = item.thinking_ref() {
            reasoning.push_str(text);
        } else if let Some((summary, content, _)) = item.reasoning_ref() {
            for text in summary.iter().chain(content) {
                reasoning.push_str(text);
            }
        }
    }
    reasoning
}

fn empty_reasoning_metadata(resp: &AiResponse) -> Option<&Value> {
    resp.vendor
        .ingress
        .get(CHAT_REASONING_FIELD_META)
        .filter(|value| value.is_null() || value.as_str() == Some(""))
}

pub fn client_history_output_item(resp: &AiResponse) -> AiItem {
    let reasoning = visible_reasoning_text(resp);
    let output = resp.output_text();
    let mut blocks = Vec::with_capacity(2);
    if !reasoning.is_empty() {
        blocks.push(ContentBlock::Thinking {
            thinking: reasoning.clone(),
            signature: None,
        });
    }
    if !output.is_empty() {
        blocks.push(ContentBlock::Text {
            text: output.into(),
            cache_control: None,
        });
    }

    let tool_calls = resp.tool_calls().cloned().collect::<Vec<_>>();
    let reasoning_field = if reasoning.is_empty() {
        empty_reasoning_metadata(resp).cloned()
    } else {
        Some(Value::String(reasoning))
    };
    AiItem {
        role: Role::Assistant,
        content: if blocks.is_empty() {
            MessageContent::Text(String::new().into())
        } else {
            MessageContent::Blocks(blocks)
        },
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
        meta: reasoning_field.map(|value| {
            stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                "reasoning_content": value,
            }))
        }),
    }
}

fn openai_usage_json(usage: &Usage) -> Value {
    // Incomplete IR usage can still prove positive counters independently;
    // only a complete report distinguishes a zero from a numeric default.
    let input =
        (usage.required_components_known || usage.prompt_tokens > 0).then_some(usage.prompt_tokens);
    let output = (usage.required_components_known || usage.completion_tokens > 0)
        .then_some(usage.completion_tokens);
    let mut value = serde_json::json!({});
    if let Some(input) = input {
        value["prompt_tokens"] = input.into();
    }
    if let Some(output) = output {
        value["completion_tokens"] = output.into();
    }
    if let Some((input, output)) = input.zip(output) {
        value["total_tokens"] = (input + output).into();
    }
    if let Some(cache_read_tokens) = usage.cache_read_tokens {
        value["prompt_tokens_details"] = serde_json::json!({
            "cached_tokens": cache_read_tokens
        });
    }
    if let Some(reasoning_tokens) = usage.reasoning_tokens {
        value["completion_tokens_details"] = serde_json::json!({
            "reasoning_tokens": reasoning_tokens
        });
    }
    value
}

// ── Stream parser (upstream OpenAI SSE → deltas) ──

pub struct OpenAIStreamParser {
    buffer: String,
    started: bool,
    done: bool,
    think_buffer: String,
    in_think_block: bool,
    reasoning_field: Option<Value>,
    reasoning_source: Option<Arc<str>>,
    saw_tool_call: bool,
    pending_tool_terminal_error: bool,
    pending_stop_reason: Option<String>,
    canonical_choice: Option<u64>,
    native_choices: std::collections::HashMap<u64, bool>,
    usage_input: Option<u64>,
    usage_output: Option<u64>,
    usage_cache_read: Option<u32>,
    usage_cache_write: Option<u32>,
    usage_reasoning: Option<u32>,
}

impl Default for OpenAIStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAIStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            started: false,
            done: false,
            think_buffer: String::new(),
            in_think_block: false,
            reasoning_field: None,
            reasoning_source: None,
            saw_tool_call: false,
            pending_tool_terminal_error: false,
            pending_stop_reason: None,
            canonical_choice: None,
            native_choices: std::collections::HashMap::new(),
            usage_input: None,
            usage_output: None,
            usage_cache_read: None,
            usage_cache_write: None,
            usage_reasoning: None,
        }
    }

    pub(crate) fn with_reasoning_field(field: Arc<str>) -> Self {
        Self {
            reasoning_source: Some(field),
            ..Self::new()
        }
    }
}

impl OpenAIStreamParser {
    pub(crate) fn parse_chunk(&mut self, raw: &str) -> Result<Vec<AiStreamDelta>> {
        let mut deltas = Vec::new();

        let mut buffer = std::mem::take(&mut self.buffer);
        let result = crate::consume_sse_frames(&mut buffer, raw, |block| {
            for line in block.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        if self.pending_tool_terminal_error {
                            deltas.extend(self.flush_pending_text());
                            self.emit_pending_terminal_error(&mut deltas);
                        } else if !self.done {
                            deltas.extend(self.flush_pending_text());
                            self.done = true;
                            deltas.push(AiStreamDelta::Done {
                                stop_reason: self
                                    .pending_stop_reason
                                    .take()
                                    .unwrap_or_else(|| "stop".to_string()),
                            });
                        }
                        continue;
                    }
                    let chunk = serde_json::from_str::<Value>(data)?;
                    self.parse_openai_chunk(&chunk, &mut deltas)?;
                }
            }
            Ok(())
        });
        self.buffer = buffer;
        result?;
        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<AiStreamDelta>> {
        let mut ai_deltas: Vec<AiStreamDelta> = Vec::new();
        if !self.buffer.trim().is_empty() {
            self.buffer.push_str("\n\n");
            ai_deltas.extend(self.parse_chunk("")?);
        }
        ai_deltas.extend(self.flush_pending_text());
        self.emit_pending_terminal_error(&mut ai_deltas);
        Ok(ai_deltas)
    }
}

impl OpenAIStreamParser {
    fn emit_pending_terminal_error(&mut self, deltas: &mut Vec<AiStreamDelta>) {
        if !self.pending_tool_terminal_error {
            return;
        }
        self.pending_tool_terminal_error = false;
        self.done = true;
        deltas.push(AiStreamDelta::StreamError {
            error: stravia_runtime_contract::protocol::ir::AiError::new(
                stravia_runtime_contract::protocol::ir::AiErrorKind::StreamMidError,
                "upstream tool_calls terminal contains no representable tool call",
            ),
        });
    }

    fn emit_usage(&mut self, chunk: &Value, deltas: &mut Vec<AiStreamDelta>) {
        let (mut usage, input, output) = extract_usage_report(chunk);
        if input.is_none()
            && output.is_none()
            && usage.cache_read_tokens.is_none()
            && usage.cache_creation_tokens.is_none()
            && usage.reasoning_tokens.is_none()
        {
            return;
        }
        // OpenAI-compatible usage frames revise cumulative counters. Preserve
        // per-field presence here: the common IR completeness flag alone cannot
        // distinguish an omitted required component from a reported zero.
        self.usage_input = input.or(self.usage_input);
        self.usage_output = output.or(self.usage_output);
        self.usage_cache_read = usage.cache_read_tokens.or(self.usage_cache_read);
        self.usage_cache_write = usage.cache_creation_tokens.or(self.usage_cache_write);
        self.usage_reasoning = usage.reasoning_tokens.or(self.usage_reasoning);
        usage.cache_read_tokens = self.usage_cache_read;
        usage.cache_creation_tokens = self.usage_cache_write;
        usage.reasoning_tokens = self.usage_reasoning;
        usage.prompt_tokens = self.usage_input.unwrap_or(0) as u32;
        usage.completion_tokens = self.usage_output.unwrap_or(0) as u32;
        usage.total_tokens = self
            .usage_input
            .zip(self.usage_output)
            .map(|(input, output)| input.saturating_add(output))
            .unwrap_or(0) as u32;
        usage.required_components_known = self.usage_input.is_some() && self.usage_output.is_some();
        deltas.push(AiStreamDelta::Usage(usage));
    }

    fn parse_openai_chunk(&mut self, chunk: &Value, deltas: &mut Vec<AiStreamDelta>) -> Result<()> {
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            deltas.extend(self.flush_pending_text());
            self.done = true;
            self.pending_tool_terminal_error = false;
            deltas.push(AiStreamDelta::StreamError {
                error: stravia_runtime_contract::protocol::ir::AiError::new(
                    stravia_runtime_contract::protocol::ir::AiErrorKind::StreamMidError,
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("upstream stream error"),
                )
                .with_raw(chunk.clone()),
            });
            return Ok(());
        }
        let choices = chunk.get("choices").and_then(Value::as_array);
        let canonical_index = self.canonical_choice.unwrap_or_else(|| {
            choices
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("index"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        });
        if self.canonical_choice.is_none() && choices.is_some_and(|choices| !choices.is_empty()) {
            self.canonical_choice = Some(canonical_index);
            if canonical_index != 0 {
                // Private native identity precedes all public canonical deltas,
                // including the role frame; cross-protocol projection drops it.
                deltas.push(AiStreamDelta::Unknown {
                    raw: serde_json::json!({"__openai_native_choice_index": canonical_index})
                        .to_string(),
                });
            }
        }
        if !self.started
            && let (Some(id), Some(model)) = (
                chunk.get("id").and_then(|v| v.as_str()),
                chunk.get("model").and_then(|v| v.as_str()),
            )
        {
            self.started = true;
            deltas.push(AiStreamDelta::MessageStart {
                id: id.to_string(),
                model: model.to_string(),
            });
        }
        let mut extra = Vec::new();
        let mut canonical = None;
        if let Some(choices) = choices {
            for (position, choice) in choices.iter().enumerate() {
                let index = choice
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(position as u64);
                if index == canonical_index {
                    canonical = Some(choice);
                    continue;
                }
                let saw_tool = self.native_choices.entry(index).or_default();
                if let Some(delta) = choice.get("delta") {
                    extract_selected_reasoning(delta, self.reasoning_source.as_deref())?;
                    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                        *saw_tool |= calls.iter().any(|call| {
                            call.get("function").is_some_and(|function| {
                                [
                                    call.get("id"),
                                    function.get("name"),
                                    function.get("arguments"),
                                ]
                                .into_iter()
                                .flatten()
                                .any(|value| value.as_str().is_some_and(|text| !text.is_empty()))
                            })
                        });
                    }
                }
                if choice.get("finish_reason").and_then(Value::as_str) == Some("tool_calls")
                    && !*saw_tool
                {
                    anyhow::bail!(
                        "upstream tool_calls terminal contains no representable tool call"
                    );
                }
                let mut native_choice = choice.clone();
                native_choice["index"] = index.into();
                extra.push(native_choice);
            }
        }
        if !extra.is_empty() {
            deltas.push(AiStreamDelta::Unknown {
                raw: serde_json::json!({"__openai_native_choices": extra}).to_string(),
            });
        }
        let Some(choice) = canonical else {
            self.emit_usage(chunk, deltas);
            return Ok(());
        };

        if let Some(delta) = choice.get("delta") {
            let reasoning = extract_selected_reasoning(delta, self.reasoning_source.as_deref())?;
            if let Some(value) = selected_empty_reasoning_field(
                delta,
                reasoning.as_deref(),
                self.reasoning_source.as_deref(),
            ) && self.reasoning_field.as_ref() != Some(value)
            {
                self.reasoning_field = Some(value.clone());
                deltas.push(AiStreamDelta::ResponseMetadata {
                    metadata: serde_json::json!({CHAT_REASONING_FIELD_META: value}),
                });
            }
            if let Some(reasoning) = reasoning
                && !reasoning.is_empty()
            {
                deltas.push(AiStreamDelta::ThinkingDelta(reasoning));
            }
            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                self.parse_text_with_think_tags(text, deltas);
            }

            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    if let Some(func) = tc.get("function") {
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        // 续包常把未出现的 name/id 写成空字符串；空值不是新的 tool call。
                        if !name.is_empty() || !id.is_empty() {
                            self.saw_tool_call = true;
                            deltas.push(AiStreamDelta::ToolCallStart {
                                index: idx,
                                id: id.to_string(),
                                name: name.to_string(),
                            });
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str())
                            && !args.is_empty()
                        {
                            self.saw_tool_call = true;
                            deltas.push(AiStreamDelta::ToolCallDelta {
                                index: idx,
                                arguments: args.to_string(),
                            });
                        }
                    }
                }
            }
        }

        self.emit_usage(chunk, deltas);
        if !self.done
            && self.pending_stop_reason.is_none()
            && !self.pending_tool_terminal_error
            && let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str())
            && !reason.is_empty()
        {
            if reason == "tool_calls" && !self.saw_tool_call {
                // 独立 usage 尾帧仍需消费；语义错误在流结束后交付，避免丢掉同批增量。
                self.pending_tool_terminal_error = true;
            } else {
                // A choice terminal is not the global stream terminal: other
                // native choices and the observed usage tail can still follow.
                self.pending_stop_reason = Some(reason.to_string());
            }
        }

        Ok(())
    }

    fn parse_text_with_think_tags(&mut self, text: &str, deltas: &mut Vec<AiStreamDelta>) {
        if text.is_empty() {
            return;
        }
        self.think_buffer.push_str(text);
        let mut consumed = 0;
        loop {
            let pending = &self.think_buffer[consumed..];
            if self.in_think_block {
                if let Some(end_idx) = pending.find("</think>") {
                    let thought = pending[..end_idx].trim().to_string();
                    if !thought.is_empty() {
                        deltas.push(AiStreamDelta::ThinkingDelta(thought));
                    }
                    consumed += end_idx + "</think>".len();
                    self.in_think_block = false;
                    continue;
                }
                break;
            }

            if let Some(start_idx) = pending.find("<think>") {
                let before = pending[..start_idx].to_string();
                if !before.is_empty() {
                    deltas.push(AiStreamDelta::TextDelta(before));
                }
                consumed += start_idx + "<think>".len();
                self.in_think_block = true;
                continue;
            }

            let keep = longest_suffix_that_can_start_tag(pending, "<think>");
            if pending.len() > keep {
                let emit = pending[..pending.len() - keep].to_string();
                if !emit.is_empty() {
                    deltas.push(AiStreamDelta::TextDelta(emit));
                }
                consumed += pending.len() - keep;
            }
            break;
        }
        self.think_buffer.drain(..consumed);
    }

    fn flush_pending_text(&mut self) -> Vec<AiStreamDelta> {
        if self.think_buffer.is_empty() {
            return vec![];
        }
        if self.in_think_block {
            let mut fallback = String::from("<think>");
            fallback.push_str(&self.think_buffer);
            self.think_buffer.clear();
            self.in_think_block = false;
            vec![AiStreamDelta::TextDelta(fallback)]
        } else {
            let remaining = std::mem::take(&mut self.think_buffer);
            vec![AiStreamDelta::TextDelta(remaining)]
        }
    }
}

fn longest_suffix_that_can_start_tag(text: &str, tag: &str) -> usize {
    let max = std::cmp::min(text.len(), tag.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if text.ends_with(&tag[..len]) {
            return len;
        }
    }
    0
}

// ── Stream formatter (deltas → OpenAI SSE) ──

pub struct OpenAIStreamFormatter {
    usage: Usage,
    id: String,
    model: String,
    saw_tool_call: bool,
    saw_reasoning: bool,
    native_choices_seen: bool,
    native_choice_index: u64,
    pending_done: bool,
}

impl Default for OpenAIStreamFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAIStreamFormatter {
    pub fn new() -> Self {
        Self {
            usage: Usage::default(),
            id: stravia_runtime_contract::identifier::new_id(),
            model: String::new(),
            saw_tool_call: false,
            saw_reasoning: false,
            native_choices_seen: false,
            native_choice_index: 0,
            pending_done: false,
        }
    }
}

impl OpenAIStreamFormatter {
    pub(crate) fn format_deltas(&mut self, deltas: &[AiStreamDelta]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for delta in deltas {
            match delta {
                AiStreamDelta::MessageStart { id, model } => {
                    if !id.is_empty() {
                        self.id = id.clone();
                    }
                    self.model = model.clone();
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {"role": "assistant"}, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::Unknown { raw } => {
                    if let Ok(value) = serde_json::from_str::<Value>(raw) {
                        if let Some(index) = value
                            .get("__openai_native_choice_index")
                            .and_then(Value::as_u64)
                        {
                            self.native_choice_index = index;
                        }
                        if let Some(choices) = value
                            .get("__openai_native_choices")
                            .and_then(Value::as_array)
                        {
                            self.native_choices_seen = true;
                            events.push(SseEvent::new(None, serde_json::json!({
                                "id": self.id, "object": "chat.completion.chunk", "model": self.model,
                                "choices": choices,
                            }).to_string()));
                        }
                    }
                }
                AiStreamDelta::ResponseMetadata { metadata } => {
                    // 已公开的真实推理不能被后续 empty/null 字段覆盖。
                    if !self.saw_reasoning
                        && let Some(value) = metadata
                            .get(CHAT_REASONING_FIELD_META)
                            .filter(|value| value.is_null() || value.as_str() == Some(""))
                    {
                        let chunk = serde_json::json!({
                            "id": self.id,
                            "object": "chat.completion.chunk",
                            "model": self.model,
                            "choices": [{"index": self.native_choice_index, "delta": {"reasoning_content": value}, "finish_reason": null}]
                        });
                        events.push(SseEvent::new(None, chunk.to_string()));
                    }
                }
                AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
                | AiStreamDelta::ReasoningSummaryDelta { text, .. } => {
                    self.saw_reasoning |= !text.is_empty();
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {"reasoning_content": text}, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::ThinkingSignature(_) => {}
                AiStreamDelta::TextDeltaWithMetadata { text, logprobs, .. } => {
                    let mut chunk = serde_json::json!({
                        "id": self.id, "object": "chat.completion.chunk", "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {"content": text}, "finish_reason": null}]
                    });
                    if !logprobs.is_empty() {
                        chunk["choices"][0]["logprobs"] = serde_json::json!({"content": logprobs});
                    }
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::TextDelta(text) => {
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {"content": text}, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::RefusalDelta(text)
                | AiStreamDelta::RefusalDeltaWithIndex { text, .. } => {
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {"refusal": text}, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::ToolCallStart { index, id, name } => {
                    self.saw_tool_call = true;
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {
                            "tool_calls": [{"index": index, "id": id, "type": "function", "function": {"name": name, "arguments": ""}}]
                        }, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::ToolCallDelta { index, arguments } => {
                    self.saw_tool_call = true;
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {
                            "tool_calls": [{"index": index, "function": {"arguments": arguments}}]
                        }, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::Usage(u) => {
                    self.usage = u.clone();
                }
                AiStreamDelta::StreamError { error } => {
                    let error = serde_json::json!({
                        "error": {
                            "message": error.message,
                            "type": error.kind.to_string(),
                            "code": error.kind.to_string(),
                        }
                    });
                    events.push(SseEvent::new(None, error.to_string()));
                }
                AiStreamDelta::Done { stop_reason } => {
                    let final_reason = if self.saw_tool_call {
                        "tool_calls".to_string()
                    } else {
                        chat_finish_reason(stop_reason).to_string()
                    };
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": self.native_choice_index, "delta": {}, "finish_reason": final_reason}],
                        "usage": openai_usage_json(&self.usage)
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                    if self.native_choices_seen {
                        self.pending_done = true;
                    } else {
                        events.push(SseEvent::new(None, "[DONE]"));
                    }
                }
                _ => {}
            }
        }
        events
    }

    pub(crate) fn format_done(&mut self) -> Vec<SseEvent> {
        if std::mem::take(&mut self.pending_done) {
            vec![SseEvent::new(None, "[DONE]")]
        } else {
            vec![]
        }
    }
}

/// Chat Completions has no `refusal` finish reason; `content_filter` is the
/// closest standard value and is what OpenAI SDKs treat as a policy stop.
fn chat_finish_reason(reason: &str) -> &str {
    match reason {
        "refusal" => "content_filter",
        other => other,
    }
}

fn extract_usage(v: &Value) -> Usage {
    extract_usage_report(v).0
}

fn extract_usage_report(v: &Value) -> (Usage, Option<u64>, Option<u64>) {
    let usage = v.get("usage").or_else(|| v.get("usageMetadata"));
    let Some(u) = usage else {
        return (Usage::default(), None, None);
    };

    let input = first_u64(
        u,
        &[
            "prompt_tokens",
            "promptTokenCount",
            "input_tokens",
            "inputTokenCount",
        ],
    );
    let output = first_u64(
        u,
        &[
            "completion_tokens",
            "candidatesTokenCount",
            "output_tokens",
            "outputTokenCount",
        ],
    );

    // Cache-hit tokens are reported by different providers under different
    // keys. Whichever shape we see, surface it under `Usage.cache_read_tokens`
    // so downstream cost analytics stop treating cached prompt tokens as
    // full-price input.
    //
    // - DeepSeek native:  `usage.prompt_cache_hit_tokens`
    // - OpenAI newer fmt: `usage.prompt_tokens_details.cached_tokens`
    // - Gemini-compat:    `usage.cached_content_token_count`
    let cache_read = u
        .get("prompt_cache_hit_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            u.get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_u64)
        })
        .or_else(|| u.get("cached_content_token_count").and_then(Value::as_u64));

    // 已报告的 0 必须保留为 Some(0)。prompt_tokens_details.reasoning_tokens 是输入侧字段，忽略。
    let cache_write = first_u64(
        u,
        &[
            "prompt_cache_write_tokens",
            "cache_creation_input_tokens",
            "cache_creation_tokens",
        ],
    );
    let reasoning = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| first_u64(u, &["completion_thinking_tokens"]));

    (
        Usage {
            prompt_tokens: input.unwrap_or(0) as u32,
            completion_tokens: output.unwrap_or(0) as u32,
            total_tokens: input
                .zip(output)
                .map(|(input, output)| input.saturating_add(output))
                .unwrap_or(0) as u32,
            required_components_known: input.is_some() && output.is_some(),
            cache_read_tokens: cache_read.map(|v| v as u32),
            cache_creation_tokens: cache_write.map(|v| v as u32),
            reasoning_tokens: reasoning.map(|v| v as u32),
            ..Usage::default()
        },
        input,
        output,
    )
}

fn first_u64(obj: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_u64()))
}

fn empty_reasoning_field<'a>(message: &'a Value, text: Option<&str>) -> Option<&'a Value> {
    let fields = ["reasoning_content", "reasoning"];
    match text {
        Some("") => fields
            .iter()
            .filter_map(|field| message.get(*field))
            .find(|value| value.as_str() == Some("")),
        None => fields
            .iter()
            .filter_map(|field| message.get(*field))
            .find(|value| value.is_null()),
        _ => None,
    }
}

fn extract_selected_reasoning(message: &Value, source: Option<&str>) -> Result<Option<String>> {
    let Some(field) = source else {
        return Ok(extract_reasoning_from_message(message));
    };
    match message.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => anyhow::bail!("upstream reasoning field `{field}` must be a string or null"),
    }
}

fn selected_empty_reasoning_field<'a>(
    message: &'a Value,
    text: Option<&str>,
    source: Option<&str>,
) -> Option<&'a Value> {
    let Some(field) = source else {
        return empty_reasoning_field(message, text);
    };
    message.get(field).filter(|value| match text {
        Some("") => value.as_str() == Some(""),
        None => value.is_null(),
        _ => false,
    })
}

pub(crate) fn extract_reasoning_from_message(message: &Value) -> Option<String> {
    if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()) {
        return Some(reasoning.to_string());
    }
    // Some backends (e.g. mlx-lm) send the field as "reasoning" instead
    // of "reasoning_content".  Accept both.
    if let Some(reasoning) = message.get("reasoning").and_then(|v| v.as_str()) {
        return Some(reasoning.to_string());
    }

    let details = message
        .get("reasoning_details")
        .and_then(|v| v.as_array())?;
    let mut parts: Vec<String> = Vec::new();
    for detail in details {
        if let Some(text) = detail
            .get("text")
            .or_else(|| detail.get("content"))
            .and_then(|v| v.as_str())
            && !text.is_empty()
        {
            parts.push(text.to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests;

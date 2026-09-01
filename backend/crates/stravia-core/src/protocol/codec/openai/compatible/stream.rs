use anyhow::Result;
use serde_json::Value;
use uuid::Uuid;

use crate::protocol::ir::request::ToolCall;
use crate::protocol::ir::usage::Usage;
use crate::protocol::ir::{AiItem, AiResponse, AiStreamDelta, ContentBlock, MessageContent, Role};
use crate::protocol::*;

// ── Non-streaming response parser ──

pub struct OpenAIResponseParser;

impl OpenAIResponseParser {
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
        let reasoning_content = message.and_then(extract_reasoning_from_message);

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
                            id: tc.get("id")?.as_str()?.to_string(),
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

        let usage = extract_usage(&resp);

        let mut ai_resp = AiResponse::new(id, model);
        if let Some(reasoning) = reasoning_content {
            ai_resp.push_reasoning(reasoning, None);
        }
        ai_resp.push_output_text(content);
        ai_resp
            .items
            .extend(tool_calls.into_iter().map(AiItem::function_call));
        ai_resp.stop_reason = stop_reason;
        ai_resp.usage = usage;
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
            resp.stop_reason.as_deref()
        } else {
            Some("tool_calls")
        };
        let mut message = serde_json::json!({
            "role": "assistant",
            "content": resp.output_text(),
        });
        let reasoning = visible_reasoning_text(resp);
        if !reasoning.is_empty() {
            message
                .as_object_mut()
                .expect("message is an object")
                .insert("reasoning_content".into(), Value::String(reasoning));
        }
        if !tool_calls.is_empty() {
            message
                .as_object_mut()
                .expect("message is an object")
                .insert("tool_calls".into(), Value::Array(tool_calls));
        }

        serde_json::json!({
            "id": resp.id,
            "object": "chat.completion",
            "model": resp.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }],
            "usage": openai_usage_json(&resp.usage)
        })
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

pub(crate) fn client_history_output_item(resp: &AiResponse) -> AiItem {
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
            text: output,
            cache_control: None,
        });
    }

    let tool_calls = resp.tool_calls().cloned().collect::<Vec<_>>();
    AiItem {
        role: Role::Assistant,
        content: if blocks.is_empty() {
            MessageContent::Text(String::new())
        } else {
            MessageContent::Blocks(blocks)
        },
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
        meta: (!reasoning.is_empty()).then(|| {
            serde_json::json!({
                "reasoning_content": reasoning,
            })
        }),
    }
}

fn openai_usage_json(usage: &Usage) -> Value {
    let mut value = serde_json::json!({
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.prompt_tokens + usage.completion_tokens,
    });
    if let Some(cache_read_tokens) = usage.cache_read_tokens {
        value["prompt_tokens_details"] = serde_json::json!({
            "cached_tokens": cache_read_tokens
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
        }
    }
}

impl OpenAIStreamParser {
    pub(crate) fn parse_chunk(&mut self, raw: &str) -> Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();

        while let Some(pos) = self.buffer.find("\n\n") {
            let block = self.buffer[..pos].to_string();
            self.buffer = self.buffer[pos + 2..].to_string();

            for line in block.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        if !self.done {
                            self.done = true;
                            deltas.push(AiStreamDelta::Done {
                                stop_reason: "stop".to_string(),
                            });
                        }
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                        self.parse_openai_chunk(&chunk, &mut deltas);
                    }
                }
            }
        }

        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<AiStreamDelta>> {
        let mut ai_deltas: Vec<AiStreamDelta> = Vec::new();
        if !self.buffer.trim().is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            ai_deltas.extend(self.parse_chunk(&format!("{remaining}\n\n"))?);
        }
        ai_deltas.extend(self.flush_pending_text());
        Ok(ai_deltas)
    }
}

impl OpenAIStreamParser {
    fn parse_openai_chunk(&mut self, chunk: &Value, deltas: &mut Vec<AiStreamDelta>) {
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

        let Some(choice) = chunk
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
        else {
            let u = extract_usage(chunk);
            if u.prompt_tokens > 0 || u.completion_tokens > 0 {
                deltas.push(AiStreamDelta::Usage(u));
            }
            return;
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(reasoning) = extract_reasoning_from_message(delta)
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
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            let id = tc
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            deltas.push(AiStreamDelta::ToolCallStart {
                                index: idx,
                                id,
                                name: name.to_string(),
                            });
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str())
                            && !args.is_empty()
                        {
                            deltas.push(AiStreamDelta::ToolCallDelta {
                                index: idx,
                                arguments: args.to_string(),
                            });
                        }
                    }
                }
            }
        }

        if !self.done
            && let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str())
            && !reason.is_empty()
        {
            self.done = true;
            deltas.push(AiStreamDelta::Done {
                stop_reason: reason.to_string(),
            });
        }

        let u = extract_usage(chunk);
        if u.prompt_tokens > 0 || u.completion_tokens > 0 {
            deltas.push(AiStreamDelta::Usage(u));
        }
    }

    fn parse_text_with_think_tags(&mut self, text: &str, deltas: &mut Vec<AiStreamDelta>) {
        if text.is_empty() {
            return;
        }
        self.think_buffer.push_str(text);

        loop {
            if self.in_think_block {
                if let Some(end_idx) = self.think_buffer.find("</think>") {
                    let thought = self.think_buffer[..end_idx].trim().to_string();
                    if !thought.is_empty() {
                        deltas.push(AiStreamDelta::ThinkingDelta(thought));
                    }
                    self.think_buffer = self.think_buffer[end_idx + "</think>".len()..].to_string();
                    self.in_think_block = false;
                    continue;
                }
                break;
            }

            if let Some(start_idx) = self.think_buffer.find("<think>") {
                let before = self.think_buffer[..start_idx].to_string();
                if !before.is_empty() {
                    deltas.push(AiStreamDelta::TextDelta(before));
                }
                self.think_buffer = self.think_buffer[start_idx + "<think>".len()..].to_string();
                self.in_think_block = true;
                continue;
            }

            let keep = longest_suffix_that_can_start_tag(&self.think_buffer, "<think>");
            if self.think_buffer.len() > keep {
                let emit = self.think_buffer[..self.think_buffer.len() - keep].to_string();
                if !emit.is_empty() {
                    deltas.push(AiStreamDelta::TextDelta(emit));
                }
                self.think_buffer = self.think_buffer[self.think_buffer.len() - keep..].to_string();
            }
            break;
        }
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
            id: format!("chatcmpl-{}", Uuid::new_v4()),
            model: String::new(),
            saw_tool_call: false,
        }
    }
}

impl OpenAIStreamFormatter {
    pub(crate) fn format_deltas(&mut self, deltas: &[AiStreamDelta]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for delta in deltas {
            match delta {
                AiStreamDelta::MessageStart { id, model } => {
                    self.id = id.clone();
                    self.model = model.clone();
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::ThinkingDelta(text)
                | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
                | AiStreamDelta::ReasoningSummaryDelta { text, .. } => {
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": 0, "delta": {"reasoning_content": text}, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::ThinkingSignature(_) => {}
                AiStreamDelta::TextDelta(text)
                | AiStreamDelta::TextDeltaWithMetadata { text, .. }
                | AiStreamDelta::RefusalDelta(text)
                | AiStreamDelta::RefusalDeltaWithIndex { text, .. } => {
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                }
                AiStreamDelta::ToolCallStart { index, id, name } => {
                    self.saw_tool_call = true;
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": 0, "delta": {
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
                        "choices": [{"index": 0, "delta": {
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
                        stop_reason.clone()
                    };
                    let chunk = serde_json::json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{"index": 0, "delta": {}, "finish_reason": final_reason}],
                        "usage": openai_usage_json(&self.usage)
                    });
                    events.push(SseEvent::new(None, chunk.to_string()));
                    events.push(SseEvent::new(None, "[DONE]"));
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

fn extract_usage(v: &Value) -> Usage {
    let usage = v.get("usage").or_else(|| v.get("usageMetadata"));
    let Some(u) = usage else {
        return Usage::default();
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

    Usage {
        prompt_tokens: input.unwrap_or(0) as u32,
        completion_tokens: output.unwrap_or(0) as u32,
        total_tokens: input
            .zip(output)
            .map(|(input, output)| input.saturating_add(output))
            .unwrap_or(0) as u32,
        required_components_known: input.is_some() && output.is_some(),
        cache_read_tokens: cache_read.map(|v| v as u32),
        ..Usage::default()
    }
}

fn first_u64(obj: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_u64()))
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

//! Incremental decoder for Kiro's AWS Event Stream response.

use serde_json::Value;
use std::collections::HashMap;
use stravia_runtime_contract::protocol::ir::{
    AiError, AiErrorKind, AiStreamDelta, ToolCall, Usage,
};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, PluginError};

// Keep host publication failures intact while classifying malformed wire data.
enum DecodeError {
    Invalid(String),
    Host(PluginError),
}
impl From<&str> for DecodeError {
    fn from(value: &str) -> Self {
        Self::Invalid(value.into())
    }
}
impl From<String> for DecodeError {
    fn from(value: String) -> Self {
        Self::Invalid(value)
    }
}
impl From<PluginError> for DecodeError {
    fn from(value: PluginError) -> Self {
        Self::Host(value)
    }
}
impl From<DecodeError> for PluginError {
    fn from(value: DecodeError) -> Self {
        match value {
            DecodeError::Invalid(message) => {
                common::plugin_error(ErrorKind::InvalidResponse, message)
            }
            DecodeError::Host(error) => error,
        }
    }
}
fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::InvalidResponse, message)
}

// AWS Event Stream permits messages up to 24 MiB and headers up to 128 KiB.
const MAX_MESSAGE: usize = 24 * 1024 * 1024;
const MAX_HEADERS: usize = 128 * 1024;

struct ToolState {
    index: usize,
    id: String,
    name: String,
    arguments: String,
    complete: bool,
}

#[derive(Default)]
pub struct KiroStream {
    buffer: Vec<u8>,
    frame_len: Option<usize>,
    tools: Vec<ToolState>,
    tool_ids: HashMap<String, usize>,
    active_tool: Option<usize>,
    stop_reason: Option<String>,
    saw_event: bool,
    terminal: bool,
    failed: bool,
}

impl KiroStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(
        &mut self,
        bytes: &[u8],
        emit: &mut impl FnMut(AiStreamDelta) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        if self.terminal || self.failed {
            return Err(invalid("Kiro stream already terminated"));
        }
        let result = self.feed_inner(bytes, emit);
        if result.is_err() {
            self.failed = true;
        }
        result.map_err(PluginError::from)
    }

    fn feed_inner(
        &mut self,
        mut bytes: &[u8],
        emit: &mut impl FnMut(AiStreamDelta) -> Result<(), PluginError>,
    ) -> Result<(), DecodeError> {
        while !bytes.is_empty() {
            let target = match self.frame_len {
                Some(total) => total,
                None if self.buffer.len() < 12 => 12,
                None => {
                    let total = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
                    let headers =
                        u32::from_be_bytes(self.buffer[4..8].try_into().unwrap()) as usize;
                    if total < 16
                        || total > MAX_MESSAGE
                        || headers > MAX_HEADERS
                        || headers > total - 16
                    {
                        return Err("invalid Kiro event stream frame size".into());
                    }
                    if crc32fast::hash(&self.buffer[..8])
                        != u32::from_be_bytes(self.buffer[8..12].try_into().unwrap())
                    {
                        return Err("invalid Kiro event stream prelude checksum".into());
                    }
                    self.frame_len = Some(total);
                    total
                }
            };
            let n = (target - self.buffer.len()).min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.buffer.len() == target && self.frame_len.is_some() {
                let mut frame = std::mem::take(&mut self.buffer);
                let result = self.frame(&frame, emit);
                frame.clear();
                self.buffer = frame;
                self.frame_len = None;
                result?;
                if self.terminal && !bytes.is_empty() {
                    return Err("data after Kiro stream error".into());
                }
            }
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        emit: &mut impl FnMut(AiStreamDelta) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        if self.failed {
            return Err(invalid("Kiro stream failed"));
        }
        if self.terminal {
            return Ok(());
        }
        if !self.buffer.is_empty()
            || !self.saw_event
            || self.stop_reason.is_none()
            || self.tools.iter().any(|t| !t.complete)
        {
            self.failed = true;
            return Err(invalid("truncated or incomplete Kiro event stream"));
        }
        let reason = self.stop_reason.take().unwrap();
        if let Err(error) = emit(AiStreamDelta::Done {
            stop_reason: reason,
        }) {
            self.failed = true;
            return Err(error);
        }
        self.terminal = true;
        Ok(())
    }

    fn frame(
        &mut self,
        frame: &[u8],
        emit: &mut impl FnMut(AiStreamDelta) -> Result<(), PluginError>,
    ) -> Result<(), DecodeError> {
        let end = frame.len() - 4;
        if crc32fast::hash(&frame[..end]) != u32::from_be_bytes(frame[end..].try_into().unwrap()) {
            return Err("invalid Kiro event stream message checksum".into());
        }
        let header_len = u32::from_be_bytes(frame[4..8].try_into().unwrap()) as usize;
        let headers = parse_headers(&frame[12..12 + header_len])?;
        let message_type = headers
            .message_type
            .ok_or("missing event stream message type")?;
        let payload_bytes = &frame[12 + header_len..end];
        let payload: Value = if payload_bytes.is_empty()
            && (message_type == "exception" || message_type == "error")
        {
            serde_json::json!({})
        } else {
            serde_json::from_slice(payload_bytes)
                .map_err(|e| format!("invalid Kiro event payload: {e}"))?
        };
        if !payload.is_object() {
            return Err("invalid Kiro event payload: expected object".into());
        }
        let event_name = headers.event_type.unwrap_or("");
        if message_type == "exception"
            || message_type == "error"
            || (message_type == "event"
                && matches!(
                    event_name,
                    "error" | "throttlingError" | "validationError" | "serviceUnavailableError"
                ))
        {
            let code =
                headers
                    .exception_type
                    .or(headers.error_code)
                    .unwrap_or(if event_name.is_empty() {
                        "KiroError"
                    } else {
                        event_name
                    });
            let kind = match code.to_ascii_lowercase().as_str() {
                "throttlingexception" | "throttlingerror" => AiErrorKind::RateLimitError,
                "validationexception" | "validationerror" => AiErrorKind::InvalidRequest,
                "accessdeniedexception" => AiErrorKind::AuthorizationError,
                "serviceunavailableexception" | "serviceunavailableerror" => {
                    AiErrorKind::ServiceUnavailable
                }
                "internalserverexception" | "internalservererror" => AiErrorKind::ServerError,
                _ => AiErrorKind::StreamMidError,
            };
            let message = payload
                .get("message")
                .or_else(|| payload.get("Message"))
                .and_then(Value::as_str)
                .or(headers.error_message)
                .unwrap_or(code);
            self.terminal = true;
            emit(AiStreamDelta::StreamError {
                error: AiError::new(kind, message),
            })?;
            return Ok(());
        }
        if message_type != "event" {
            return Err(format!("unknown Kiro event stream message type: {message_type}").into());
        }
        let event = headers
            .event_type
            .ok_or("missing event stream event type")?;
        self.saw_event = true;
        match event {
            "assistantResponseEvent" => {
                let text = required_string(&payload, "content")?;
                if !text.is_empty() {
                    emit(AiStreamDelta::TextDelta(text.to_owned()))?;
                }
            }
            "reasoningContentEvent" => {
                if let Some(text) = optional_string(&payload, "text")? {
                    if !text.is_empty() {
                        emit(AiStreamDelta::ThinkingDelta(text.to_owned()))?;
                    }
                }
                if let Some(signature) = optional_string(&payload, "signature")? {
                    if !signature.is_empty() {
                        emit(AiStreamDelta::ThinkingSignature(signature.to_owned()))?;
                    }
                }
            }
            "toolUseEvent" => self.tool_event(&payload, emit)?,
            "metadataEvent" => {
                if let Some(usage) = payload.get("tokenUsage") {
                    emit(AiStreamDelta::Usage(parse_usage(usage)?))?;
                }
                if let Some(reason) = optional_string(&payload, "stopReason")? {
                    self.stop_reason = Some(
                        match reason {
                            "END_TURN" => {
                                if self.tools.is_empty() {
                                    "stop"
                                } else {
                                    "tool_calls"
                                }
                            }
                            "TOOL_USE" => "tool_calls",
                            "MAX_TOKENS"
                            | "MAX_OUTPUT_TOKENS"
                            | "MODEL_CONTEXT_WINDOW_EXCEEDED" => "length",
                            "CONTENT_FILTERED" | "CONTENT_FILTER" => "content_filter",
                            _ => reason,
                        }
                        .to_owned(),
                    );
                }
            }
            "meteringEvent"
            | "contextUsageEvent"
            | "codeReferenceEvent"
            | "documentCitationEvent"
            | "toolResultEvent" => {}
            _ => {}
        }
        Ok(())
    }

    fn tool_event(
        &mut self,
        payload: &Value,
        emit: &mut impl FnMut(AiStreamDelta) -> Result<(), PluginError>,
    ) -> Result<(), DecodeError> {
        let id = optional_string(payload, "toolUseId")?;
        let name = optional_string(payload, "name")?;
        let input = optional_string(payload, "input")?;
        let stop = match payload.get("stop") {
            None => false,
            Some(v) => v.as_bool().ok_or("invalid tool stop flag")?,
        };
        let index = if let Some(id) = id {
            if let Some(&index) = self.tool_ids.get(id) {
                index
            } else {
                let name = name
                    .filter(|s| !s.is_empty())
                    .ok_or("new Kiro tool has no name")?;
                if id.is_empty() {
                    return Err("new Kiro tool has no id".into());
                }
                let index = self.tools.len();
                self.tools.push(ToolState {
                    index,
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: String::new(),
                    complete: false,
                });
                self.tool_ids.insert(id.to_owned(), index);
                emit(AiStreamDelta::ToolCallStart {
                    index,
                    id: id.to_owned(),
                    name: name.to_owned(),
                })?;
                index
            }
        } else {
            self.active_tool
                .ok_or("Kiro tool fragment without an active tool")?
        };
        let tool = &mut self.tools[index];
        if tool.complete {
            return Err("Kiro tool fragment after completion".into());
        }
        if let Some(name) = name {
            if name != tool.name {
                return Err("Kiro tool name changed".into());
            }
        }
        self.active_tool = Some(index);
        if let Some(input) = input {
            if !input.is_empty() {
                tool.arguments.push_str(input);
                emit(AiStreamDelta::ToolCallDelta {
                    index,
                    arguments: input.to_owned(),
                })?;
            }
        }
        if stop {
            tool.complete = true;
            emit(AiStreamDelta::ToolCallComplete {
                index: tool.index,
                tool_call: ToolCall {
                    id: tool.id.clone().into(),
                    name: tool.name.clone(),
                    arguments: tool.arguments.clone(),
                },
            })?;
            self.active_tool = None;
        }
        Ok(())
    }
}

fn required_string<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    optional_string(v, key)?.ok_or_else(|| format!("missing Kiro {key}"))
}
fn optional_string<'a>(v: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    v.get(key)
        .map(|x| x.as_str().ok_or_else(|| format!("invalid Kiro {key}")))
        .transpose()
}
fn parse_usage(v: &Value) -> Result<Usage, String> {
    if !v.is_object() {
        return Err("invalid Kiro token usage".into());
    }
    let count = |key: &str| -> Result<Option<u32>, String> {
        v.get(key)
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| format!("invalid Kiro token usage {key}"))
            })
            .transpose()
    };
    let uncached = count("uncachedInputTokens")?;
    let cache_read = count("cacheReadInputTokens")?;
    let cache_write = count("cacheWriteInputTokens")?;
    let output = count("outputTokens")?;
    let total = count("totalTokens")?;
    let prompt = uncached
        .unwrap_or(0)
        .checked_add(cache_read.unwrap_or(0))
        .and_then(|n| n.checked_add(cache_write.unwrap_or(0)))
        .ok_or("Kiro token usage overflow")?;
    Ok(Usage {
        prompt_tokens: prompt,
        completion_tokens: output.unwrap_or(0),
        total_tokens: total.unwrap_or(0),
        required_components_known: uncached.is_some() && output.is_some() && total.is_some(),
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_write,
        ..Usage::default()
    })
}

#[derive(Default)]
struct Headers<'a> {
    message_type: Option<&'a str>,
    event_type: Option<&'a str>,
    exception_type: Option<&'a str>,
    error_code: Option<&'a str>,
    error_message: Option<&'a str>,
}

// Validate all header encodings while borrowing only the pseudo-headers we use.
fn parse_headers(mut bytes: &[u8]) -> Result<Headers<'_>, String> {
    let mut result = Headers::default();
    while !bytes.is_empty() {
        let n = bytes[0] as usize;
        bytes = &bytes[1..];
        if n == 0 || bytes.len() < n + 1 {
            return Err("invalid event stream header".into());
        }
        let name =
            std::str::from_utf8(&bytes[..n]).map_err(|_| "invalid event stream header name")?;
        let ty = bytes[n];
        bytes = &bytes[n + 1..];
        let (length, prefix) = match ty {
            0 | 1 => (0, 0),
            2 => (1, 0),
            3 => (2, 0),
            4 => (4, 0),
            5 | 8 => (8, 0),
            9 => (16, 0),
            6 | 7 => {
                if bytes.len() < 2 {
                    return Err("truncated event stream header".into());
                }
                (u16::from_be_bytes([bytes[0], bytes[1]]) as usize, 2)
            }
            _ => return Err("invalid event stream header type".into()),
        };
        if bytes.len() < prefix + length {
            return Err("truncated event stream header".into());
        }
        if name.starts_with(':') {
            if ty != 7 {
                return Err("invalid event stream pseudo-header type".into());
            }
            let value = std::str::from_utf8(&bytes[prefix..prefix + length])
                .map_err(|_| "invalid event stream header value")?;
            let slot = match name {
                ":message-type" => Some(&mut result.message_type),
                ":event-type" => Some(&mut result.event_type),
                ":exception-type" => Some(&mut result.exception_type),
                ":error-code" => Some(&mut result.error_code),
                ":error-message" => Some(&mut result.error_message),
                _ => None,
            };
            if let Some(slot) = slot {
                if slot.replace(value).is_some() {
                    return Err("duplicate event stream pseudo-header".into());
                }
            }
        }
        bytes = &bytes[prefix + length..];
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(kind: &str, event: &str, payload: Value) -> Vec<u8> {
        fn header(out: &mut Vec<u8>, key: &str, value: &str) {
            out.push(key.len() as u8);
            out.extend_from_slice(key.as_bytes());
            out.push(7);
            out.extend_from_slice(&(value.len() as u16).to_be_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        let mut headers = Vec::new();
        header(&mut headers, ":message-type", kind);
        header(
            &mut headers,
            if kind == "event" {
                ":event-type"
            } else {
                ":exception-type"
            },
            event,
        );
        let payload = payload.to_string();
        let total = 16 + headers.len() + payload.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
        out
    }
    #[test]
    fn split_frames_and_tool_lifecycle() {
        let mut wire = frame(
            "event",
            "assistantResponseEvent",
            json!({"content":"hello"}),
        );
        wire.extend(frame(
            "event",
            "toolUseEvent",
            json!({"toolUseId":"id", "name":"read", "input":"{\"path\":"}),
        ));
        wire.extend(frame(
            "event",
            "toolUseEvent",
            json!({"input":"\"a\"}", "stop":true}),
        ));
        wire.extend(frame("event", "metadataEvent", json!({"stopReason":"END_TURN", "tokenUsage":{"uncachedInputTokens":3,"outputTokens":2,"totalTokens":5}})));
        let mut decoder = KiroStream::new();
        let mut deltas = Vec::new();
        for chunk in wire.chunks(3) {
            decoder
                .feed(chunk, &mut |d| {
                    deltas.push(d);
                    Ok(())
                })
                .unwrap();
        }
        decoder
            .finish(&mut |d| {
                deltas.push(d);
                Ok(())
            })
            .unwrap();
        assert!(matches!(&deltas[0], AiStreamDelta::TextDelta(s) if s == "hello"));
        assert!(
            matches!(&deltas[1], AiStreamDelta::ToolCallStart { id, name, .. } if id == "id" && name == "read")
        );
        assert!(
            matches!(&deltas[4], AiStreamDelta::ToolCallComplete { tool_call, .. } if tool_call.arguments == "{\"path\":\"a\"}")
        );
        assert!(
            matches!(&deltas[5], AiStreamDelta::Usage(u) if u.prompt_tokens == 3 && u.total_tokens == 5)
        );
        assert!(
            matches!(&deltas[6], AiStreamDelta::Done { stop_reason } if stop_reason == "tool_calls")
        );
    }
    #[test]
    fn decodes_complete_tool_input_followed_by_separate_stop() {
        let mut wire = frame(
            "event",
            "assistantResponseEvent",
            json!({"content":"Checking the file."}),
        );
        wire.extend(frame("event", "toolUseEvent", json!({"toolUseId":"synthetic-tool-1","name":"read_file","input":"{\"path\":\"/workspace/input.txt\"}"})));
        wire.extend(frame(
            "event",
            "toolUseEvent",
            json!({"toolUseId":"synthetic-tool-1","name":"read_file","stop":true}),
        ));
        wire.extend(frame(
            "event",
            "metadataEvent",
            json!({"stopReason":"TOOL_USE"}),
        ));
        let mut decoder = KiroStream::new();
        let mut deltas = Vec::new();
        decoder
            .feed(&wire, &mut |delta| {
                deltas.push(delta);
                Ok(())
            })
            .unwrap();
        decoder
            .finish(&mut |delta| {
                deltas.push(delta);
                Ok(())
            })
            .unwrap();
        assert!(
            matches!(&deltas[..], [AiStreamDelta::TextDelta(text), AiStreamDelta::ToolCallStart { id, name, .. }, AiStreamDelta::ToolCallDelta { arguments, .. }, AiStreamDelta::ToolCallComplete { tool_call, .. }, AiStreamDelta::Done { stop_reason }] if text == "Checking the file." && id == "synthetic-tool-1" && name == "read_file" && arguments == "{\"path\":\"/workspace/input.txt\"}" && tool_call.arguments == *arguments && stop_reason == "tool_calls")
        );
    }
    #[test]
    fn rejects_truncation_and_bad_checksums() {
        let wire = frame("event", "assistantResponseEvent", json!({"content":"x"}));
        let mut stream = KiroStream::new();
        stream
            .feed(&wire[..wire.len() - 1], &mut |_| Ok(()))
            .unwrap();
        assert!(stream.finish(&mut |_| Ok(())).is_err());
        let mut corrupt = wire;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        assert!(KiroStream::new().feed(&corrupt, &mut |_| Ok(())).is_err());
    }
    #[test]
    fn preserves_host_error_and_ignores_unknown_metadata() {
        let wire = frame("event", "assistantResponseEvent", json!({"content":"x"}));
        let error = KiroStream::new()
            .feed(&wire, &mut |_| {
                Err(common::plugin_error(ErrorKind::Unsupported, "host failure"))
            })
            .unwrap_err();
        assert!(matches!(error.kind, ErrorKind::Unsupported));
        let mut wire = frame("event", "futureEvent", json!({"private":"do not publish"}));
        wire.extend(frame("event", "metadataEvent", json!({"stopReason":"MAX_TOKENS", "tokenUsage":{"uncachedInputTokens":3,"cacheReadInputTokens":4,"cacheWriteInputTokens":2,"outputTokens":5,"totalTokens":14}})));
        let mut decoder = KiroStream::new();
        let mut deltas = Vec::new();
        decoder
            .feed(&wire, &mut |d| {
                deltas.push(d);
                Ok(())
            })
            .unwrap();
        decoder
            .finish(&mut |d| {
                deltas.push(d);
                Ok(())
            })
            .unwrap();
        assert!(
            matches!(&deltas[..], [AiStreamDelta::Usage(u), AiStreamDelta::Done { stop_reason }] if u.prompt_tokens == 9 && u.cache_read_tokens == Some(4) && u.cache_creation_tokens == Some(2) && stop_reason == "length")
        );
    }
    #[test]
    fn exception_is_terminal_error() {
        let wire = frame(
            "exception",
            "ThrottlingException",
            json!({"message":"slow down"}),
        );
        let mut decoder = KiroStream::new();
        let mut deltas = Vec::new();
        decoder
            .feed(&wire, &mut |d| {
                deltas.push(d);
                Ok(())
            })
            .unwrap();
        decoder
            .finish(&mut |d| {
                deltas.push(d);
                Ok(())
            })
            .unwrap();
        assert!(
            matches!(&deltas[..], [AiStreamDelta::StreamError { error }] if error.kind == AiErrorKind::RateLimitError)
        );
    }
}

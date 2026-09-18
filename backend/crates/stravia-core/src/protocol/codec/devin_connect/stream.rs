//! `GetChatMessageResponse` stream parser — Connect frames → `AiStreamDelta`.
//!
//! Top-level response fields (verified against live captures by the
//! reference project):
//!
//! ```text
//! GetChatMessageResponse
//!   #3  string  delta_text
//!   #5  varint  finish signal (2 == stop; unknown values map to "stop")
//!   #6  repeated ChatToolCall { #1 id, #2 name, #3 arguments_json,
//!                               #4 invalid_json_str, #5 invalid_json_err,
//!                               #6 is_custom_tool_call }
//!   #7  metadata { #2 prompt_tokens, #3 completion_tokens,
//!                  #9 actual_model_uid }
//!   #9  string  delta_thinking
//!   #28 UsageStats { #1 label, #2 repeated UsageEntry {
//!          #4 dimension { #1 label, #2 fixed32 value, #3 unit },
//!          #5 metric_id } }   — billed token breakdown, final frame only
//! ```
//!
//! The UsageStats block is the upstream's billed usage accounting: entries
//! are selected by `metric_id` (`input_tokens`, `output_tokens`,
//! `cached_input_tokens`, `cache_read_input_tokens`,
//! `cache_creation_input_tokens`, `reasoning_tokens`). A well-formed block
//! that omits a cache metric means nothing was cached, so absent cache
//! metrics are read as zero rather than unknown.
//!
//! A single logical tool call streams across MULTIPLE frames: the first
//! carries `{id, name}`, later frames carry only an `arguments_json`
//! fragment. Fragments are merged by id; an id-less fragment appends to the
//! currently open call.

use std::collections::BTreeMap;

use anyhow::bail;
use serde_json::Value;

use super::connect::{ConnectFrame, ConnectFrameReader};
use super::proto::{ProtoField, parse_fields};
use stravia_runtime_contract::protocol::ir::AiError;
use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::Usage;

fn clamp_tokens(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// One in-flight native tool call being accumulated across frames.
struct OpenToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// UTF-8 accumulator for text that can split a multi-byte character across
/// two frames. Holds the incomplete trailing sequence until the next frame
/// completes it.
#[derive(Default)]
struct Utf8Pending(Vec<u8>);

impl Utf8Pending {
    fn push(&mut self, bytes: &[u8]) -> anyhow::Result<String> {
        self.0.extend_from_slice(bytes);
        match std::str::from_utf8(&self.0) {
            Ok(text) => {
                let out = text.to_string();
                self.0.clear();
                Ok(out)
            }
            Err(error) => {
                let valid = error.valid_up_to();
                let prefix = std::str::from_utf8(&self.0[..valid])
                    .expect("bytes before valid_up_to must be valid UTF-8")
                    .to_string();
                match error.error_len() {
                    // Incomplete tail sequence — wait for the next frame.
                    None => {
                        let tail = self.0[valid..].to_vec();
                        self.0.clear();
                        self.0 = tail;
                        Ok(prefix)
                    }
                    // Genuinely invalid bytes: keep going with a replacement
                    // char instead of stalling the whole stream.
                    Some(_) => {
                        let tail = self.0[valid..].to_vec();
                        let mut out = prefix;
                        out.push_str(&String::from_utf8_lossy(&tail));
                        self.0.clear();
                        Ok(out)
                    }
                }
            }
        }
    }
}

/// Billed token breakdown from the final-frame `UsageStats` block (#28).
/// Cache/reasoning counts accumulate across metric entries; `None` marks a
/// metric the upstream never reported.
#[derive(Default, Clone, Copy)]
struct UsageStats {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    reasoning_tokens: u64,
}

pub struct DevinConnectStreamParser {
    reader: ConnectFrameReader,
    started: bool,
    done: bool,
    saw_end_stream: bool,
    last_finish: Option<u64>,
    usage_emitted: bool,
    usage_prompt: Option<u64>,
    usage_completion: Option<u64>,
    usage_stats: Option<UsageStats>,
    content: Utf8Pending,
    reasoning: Utf8Pending,
    tools: BTreeMap<usize, OpenToolCall>,
    open_tool: Option<usize>,
    next_tool_index: usize,
}

impl DevinConnectStreamParser {
    pub fn new() -> Self {
        Self {
            reader: ConnectFrameReader::new(),
            started: false,
            done: false,
            saw_end_stream: false,
            last_finish: None,
            usage_emitted: false,
            usage_prompt: None,
            usage_completion: None,
            usage_stats: None,
            content: Utf8Pending::default(),
            reasoning: Utf8Pending::default(),
            tools: BTreeMap::new(),
            open_tool: None,
            next_tool_index: 0,
        }
    }

    pub(crate) fn parse_chunk(&mut self, raw: &[u8]) -> anyhow::Result<Vec<AiStreamDelta>> {
        let mut deltas = Vec::new();
        for frame in self.reader.push(raw)? {
            match frame {
                ConnectFrame::Data(payload) => self.parse_data_frame(&payload, &mut deltas),
                ConnectFrame::EndStream(payload) => {
                    self.saw_end_stream = true;
                    self.finish_tools(&mut deltas);
                    self.parse_trailer(&payload, &mut deltas);
                }
            }
        }
        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        let mut deltas = Vec::new();
        if !self.reader.is_empty() {
            bail!("incomplete Connect frame at end of response");
        }
        self.finish_tools(&mut deltas);
        if self.done {
            return Ok(deltas);
        }
        if self.saw_end_stream {
            self.done = true;
            deltas.push(AiStreamDelta::Done {
                stop_reason: map_finish_reason(self.last_finish).into(),
            });
        } else {
            deltas.push(AiStreamDelta::UnexpectedEof);
        }
        Ok(deltas)
    }

    fn ensure_started(&mut self, deltas: &mut Vec<AiStreamDelta>) {
        if !self.started {
            self.started = true;
            deltas.push(AiStreamDelta::MessageStart {
                id: String::new(),
                model: String::new(),
            });
        }
    }

    fn parse_data_frame(&mut self, payload: &[u8], deltas: &mut Vec<AiStreamDelta>) {
        // A malformed upstream frame must not tear down the stream: it is a
        // local decode problem, not an auth/transient signal.
        let fields = match parse_fields(payload) {
            Ok(fields) => fields,
            Err(error) => {
                tracing::debug!(
                    target: "stravia_core::protocol::devin_connect",
                    "skipping malformed Connect data frame: {error}"
                );
                return;
            }
        };
        for field in &fields {
            match (field.number, field.wire_type) {
                (3, 2) => {
                    self.ensure_started(deltas);
                    match self.content.push(field.bytes) {
                        Ok(text) if !text.is_empty() => {
                            deltas.push(AiStreamDelta::TextDelta(text));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::debug!(
                                target: "stravia_core::protocol::devin_connect",
                                "invalid UTF-8 in text delta: {error}"
                            );
                        }
                    }
                }
                (5, 0) => {
                    self.last_finish = Some(field.scalar);
                }
                (6, 2) => self.parse_tool_call(field, deltas),
                (7, 2) => self.parse_metadata(field.bytes, deltas),
                (28, 2) => self.parse_usage_stats(field.bytes, deltas),
                (9, 2) => {
                    self.ensure_started(deltas);
                    match self.reasoning.push(field.bytes) {
                        Ok(text) if !text.is_empty() => {
                            deltas.push(AiStreamDelta::ThinkingDelta(text));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::debug!(
                                target: "stravia_core::protocol::devin_connect",
                                "invalid UTF-8 in thinking delta: {error}"
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn parse_metadata(&mut self, payload: &[u8], deltas: &mut Vec<AiStreamDelta>) {
        let fields = match parse_fields(payload) {
            Ok(fields) => fields,
            Err(error) => {
                tracing::debug!(
                    target: "stravia_core::protocol::devin_connect",
                    "skipping malformed frame metadata: {error}"
                );
                return;
            }
        };
        let varint = |number: u32| {
            fields
                .iter()
                .find(|f| f.number == number && f.wire_type == 0)
                .map(|f| f.scalar)
        };
        let string = |number: u32| {
            fields
                .iter()
                .find(|f| f.number == number && f.wire_type == 2)
                .and_then(|f| std::str::from_utf8(f.bytes).ok())
                .map(str::to_string)
        };
        if let Some(model) = string(9) {
            if self.started {
                deltas.push(AiStreamDelta::ResponseMetadata {
                    metadata: serde_json::json!({"model": model}),
                });
            } else {
                self.started = true;
                deltas.push(AiStreamDelta::MessageStart {
                    id: String::new(),
                    model,
                });
            }
        }
        if let Some(prompt) = varint(2) {
            self.usage_prompt = Some(prompt);
        }
        // completion_tokens only rides the final metadata frame; treat the
        // pair as usage only when the completion count is present.
        if let Some(completion) = varint(3) {
            self.usage_completion = Some(completion);
            if !self.usage_emitted {
                self.usage_emitted = true;
                self.emit_usage(deltas);
            }
        }
    }

    /// `UsageStats` block (#28) — the upstream's billed usage accounting,
    /// sent on the final frame. Entries are keyed by `metric_id` inside a
    /// displayed-dimension submessage:
    ///
    /// ```text
    /// UsageStats  { #1 label, #2 repeated UsageEntry }
    /// UsageEntry  { #4 dimension, #5 metric_id }
    /// dimension   { #1 label, #2 fixed32 float value, #3 unit, #4 plural }
    /// ```
    fn parse_usage_stats(&mut self, payload: &[u8], deltas: &mut Vec<AiStreamDelta>) {
        let fields = match parse_fields(payload) {
            Ok(fields) => fields,
            Err(error) => {
                tracing::debug!(
                    target: "stravia_core::protocol::devin_connect",
                    "skipping malformed UsageStats block: {error}"
                );
                return;
            }
        };
        let mut stats = UsageStats::default();
        let mut recognized = false;
        for field in &fields {
            if field.number != 2 || field.wire_type != 2 {
                continue;
            }
            let Ok(entry) = parse_fields(field.bytes) else {
                continue;
            };
            let mut metric = None;
            let mut value = None;
            for entry_field in &entry {
                match (entry_field.number, entry_field.wire_type) {
                    (5, 2) => metric = std::str::from_utf8(entry_field.bytes).ok(),
                    (4, 2) => {
                        value = parse_fields(entry_field.bytes)
                            .ok()
                            .and_then(|dimension| {
                                dimension
                                    .iter()
                                    .find(|d| d.number == 2 && d.wire_type == 5)
                                    .map(|d| f32::from_bits(d.scalar as u32))
                            })
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .map(|v| v.round() as u64);
                    }
                    _ => {}
                }
            }
            let (Some(metric), Some(value)) = (metric, value) else {
                continue;
            };
            match metric {
                "input_tokens" => stats.input_tokens = Some(value),
                "output_tokens" => stats.output_tokens = Some(value),
                "cached_input_tokens"
                | "cache_read_input_tokens"
                | "cache_read_tokens"
                | "cached_tokens" => stats.cache_read_tokens += value,
                "cache_creation_input_tokens"
                | "cache_creation_tokens"
                | "cache_write_input_tokens"
                | "cache_write_tokens" => stats.cache_creation_tokens += value,
                "reasoning_tokens" | "output_reasoning_tokens" | "thought_tokens" => {
                    stats.reasoning_tokens += value
                }
                _ => continue,
            }
            recognized = true;
        }
        if !recognized {
            return;
        }
        self.usage_stats = Some(stats);
        self.emit_usage(deltas);
    }

    /// Merge the #7 metadata counts with the #28 billed breakdown. Both the
    /// metadata `prompt_tokens` and the billed `input_tokens` count only the
    /// fresh (uncached) input — live captures show them far below the
    /// reported `cache_read_tokens` — so the processed prompt total is
    /// rebuilt Anthropic-style: fresh + cache-read + cache-write. Emitted
    /// deltas are last-wins downstream, so each emission carries the most
    /// complete view so far.
    fn merged_usage(&self) -> Option<Usage> {
        let stats = self.usage_stats.unwrap_or_default();
        let base_input = self
            .usage_prompt
            .into_iter()
            .chain(stats.input_tokens)
            .max();
        let prompt = base_input.map(|base| {
            base.saturating_add(stats.cache_read_tokens)
                .saturating_add(stats.cache_creation_tokens)
        });
        let completion = stats.output_tokens.or(self.usage_completion);
        if prompt.is_none() && completion.is_none() {
            return None;
        }
        // A well-formed UsageStats block reports the complete billed
        // accounting: a cache metric absent from it means nothing was
        // cached, so the count reads as zero rather than unknown.
        let billed = self
            .usage_stats
            .is_some_and(|s| s.input_tokens.is_some() || s.output_tokens.is_some());
        Some(Usage {
            prompt_tokens: clamp_tokens(prompt.unwrap_or(0)),
            completion_tokens: clamp_tokens(completion.unwrap_or(0)),
            total_tokens: clamp_tokens(prompt.unwrap_or(0).saturating_add(completion.unwrap_or(0))),
            required_components_known: prompt.is_some() && completion.is_some(),
            cache_read_tokens: billed.then(|| clamp_tokens(stats.cache_read_tokens)),
            cache_creation_tokens: billed.then(|| clamp_tokens(stats.cache_creation_tokens)),
            reasoning_tokens: (stats.reasoning_tokens > 0)
                .then(|| clamp_tokens(stats.reasoning_tokens)),
            ..Usage::default()
        })
    }

    fn emit_usage(&mut self, deltas: &mut Vec<AiStreamDelta>) {
        if let Some(usage) = self.merged_usage() {
            deltas.push(AiStreamDelta::Usage(usage));
        }
    }

    fn parse_tool_call(&mut self, field: &ProtoField<'_>, deltas: &mut Vec<AiStreamDelta>) {
        self.ensure_started(deltas);
        let sub = match parse_fields(field.bytes) {
            Ok(sub) => sub,
            Err(error) => {
                tracing::debug!(
                    target: "stravia_core::protocol::devin_connect",
                    "skipping malformed ChatToolCall sub-message: {error}"
                );
                return;
            }
        };
        let string_of = |number: u32| {
            sub.iter()
                .find(|f| f.number == number && f.wire_type == 2)
                .and_then(|f| std::str::from_utf8(f.bytes).ok())
                .map(str::to_string)
        };
        let id = string_of(1).unwrap_or_default();
        let name = string_of(2).unwrap_or_default();
        let invalid_json = string_of(4).is_some() || string_of(5).is_some();
        let raw_args = string_of(3);
        // Upstream substitutes a {} placeholder for malformed arguments and
        // flags them via #4/#5; mirror that so the tool-call chain doesn't
        // carry corrupt JSON.
        let args = match raw_args {
            Some(_) if invalid_json => "{}".to_string(),
            Some(raw) => raw,
            None if invalid_json => "{}".to_string(),
            None => String::new(),
        };

        let index = match self.open_tool {
            // A fragment carrying a DIFFERENT id starts a new call; an id-less
            // fragment appends to the currently open one.
            Some(open) if id.is_empty() || id == self.tools[&open].id => open,
            _ => {
                let index = self.next_tool_index;
                self.next_tool_index += 1;
                self.tools.insert(
                    index,
                    OpenToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                    },
                );
                self.open_tool = Some(index);
                deltas.push(AiStreamDelta::ToolCallStart {
                    index,
                    id,
                    name: name.clone(),
                });
                index
            }
        };
        let call = self.tools.get_mut(&index).expect("open tool call exists");
        if call.name.is_empty() && !name.is_empty() {
            call.name = name;
        }
        if !args.is_empty() {
            call.arguments.push_str(&args);
            deltas.push(AiStreamDelta::ToolCallDelta {
                index,
                arguments: args,
            });
        }
    }

    /// Close every open tool call. Emitted when the stream ends or an
    /// end-of-stream trailer arrives.
    fn finish_tools(&mut self, deltas: &mut Vec<AiStreamDelta>) {
        for (index, call) in std::mem::take(&mut self.tools) {
            deltas.push(AiStreamDelta::ToolCallComplete {
                index,
                tool_call: ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                },
            });
        }
        self.open_tool = None;
    }

    /// End-of-stream trailer: `{}` on success or `{"error":{code,message}}`.
    fn parse_trailer(&mut self, payload: &[u8], deltas: &mut Vec<AiStreamDelta>) {
        let text = match std::str::from_utf8(payload) {
            Ok(text) => text.trim(),
            Err(_) => return,
        };
        if text.is_empty() || text == "{}" {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let error = parsed.get("error").unwrap_or(&parsed);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Devin Connect stream error")
            .to_string();
        let kind = connect_error_kind(error.get("code").and_then(Value::as_str));
        deltas.push(AiStreamDelta::StreamError {
            error: AiError::new(kind, message).with_raw(parsed),
        });
    }
}

/// Connect-RPC status codes → unified error classification.
fn connect_error_kind(code: Option<&str>) -> AiErrorKind {
    match code.unwrap_or_default() {
        "unauthenticated" => AiErrorKind::AuthenticationError,
        "permission_denied" => AiErrorKind::AuthorizationError,
        "not_found" | "unimplemented" => AiErrorKind::NotFoundError,
        "resource_exhausted" => AiErrorKind::RateLimitError,
        "deadline_exceeded" => AiErrorKind::Timeout,
        "unavailable" | "aborted" => AiErrorKind::ServiceUnavailable,
        "internal" | "data_loss" | "unknown" => AiErrorKind::ServerError,
        "invalid_argument" | "failed_precondition" | "out_of_range" => AiErrorKind::InvalidRequest,
        _ => AiErrorKind::StreamMidError,
    }
}

/// Only 2 == stop is pinned by live captures; every other value resolves to
/// "stop" so a completed stream never reads as truncated.
fn map_finish_reason(_finish: Option<u64>) -> &'static str {
    "stop"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::codec::devin_connect::proto::{
        write_fixed32_field, write_message_field, write_string_field, write_varint_field,
    };

    fn data_frame(payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn trailer_frame(payload: &str) -> Vec<u8> {
        let mut frame = vec![0x02u8];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload.as_bytes());
        frame
    }

    fn text_payload(text: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        write_string_field(&mut payload, 3, text);
        payload
    }

    #[test]
    fn text_delta_across_two_frames() {
        let mut parser = DevinConnectStreamParser::new();
        let mut wire = data_frame(&text_payload("hello "));
        wire.extend_from_slice(&data_frame(&text_payload("world")));
        wire.extend_from_slice(&trailer_frame("{}"));
        let deltas = parser.parse_chunk(&wire).unwrap();
        assert!(matches!(deltas[0], AiStreamDelta::MessageStart { .. }));
        assert!(matches!(&deltas[1], AiStreamDelta::TextDelta(t) if t == "hello "));
        assert!(matches!(&deltas[2], AiStreamDelta::TextDelta(t) if t == "world"));
        let tail = parser.finish().unwrap();
        assert!(matches!(tail.last(), Some(AiStreamDelta::Done { .. })));
    }

    #[test]
    fn split_utf8_char_across_frames() {
        // '世' is 3 UTF-8 bytes; split it across two proto strings.
        let mut parser = DevinConnectStreamParser::new();
        let mut payload_a = Vec::new();
        write_len(&mut payload_a, 3, &[0xE4, 0xB8]);
        let mut payload_b = Vec::new();
        write_len(&mut payload_b, 3, &[0x96]);
        let mut wire = data_frame(&payload_a);
        wire.extend_from_slice(&data_frame(&payload_b));
        let deltas = parser.parse_chunk(&wire).unwrap();
        let texts: Vec<String> = deltas
            .iter()
            .filter_map(|d| match d {
                AiStreamDelta::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.concat(), "世");
    }

    fn write_len(out: &mut Vec<u8>, field: u32, payload: &[u8]) {
        crate::protocol::codec::devin_connect::proto::write_len_field(out, field, payload);
    }

    #[test]
    fn metadata_emits_usage_and_model() {
        let mut parser = DevinConnectStreamParser::new();
        let mut meta = Vec::new();
        write_varint_field(&mut meta, 2, 150);
        write_varint_field(&mut meta, 3, 42);
        write_string_field(&mut meta, 9, "swe-1-7");
        let mut payload = Vec::new();
        write_message_field(&mut payload, 7, &meta);
        let deltas = parser.parse_chunk(&data_frame(&payload)).unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            AiStreamDelta::Usage(u) if u.prompt_tokens == 150 && u.completion_tokens == 42
        )));
        assert!(deltas.iter().any(|d| matches!(
            d,
            AiStreamDelta::MessageStart { model, .. } if model == "swe-1-7"
        )));
    }

    /// UsageStats block (#28): UsageEntry { #4 dimension { #2 fixed32 float },
    /// #5 metric_id }.
    fn usage_stats_frame(entries: &[(&str, f32)]) -> Vec<u8> {
        let mut block = Vec::new();
        write_string_field(&mut block, 1, "Token Usage");
        for (metric, value) in entries {
            let mut dimension = Vec::new();
            write_string_field(&mut dimension, 1, metric);
            write_fixed32_field(&mut dimension, 2, *value);
            write_string_field(&mut dimension, 3, " tokens");
            let mut entry = Vec::new();
            write_message_field(&mut entry, 4, &dimension);
            write_string_field(&mut entry, 5, metric);
            write_message_field(&mut block, 2, &entry);
        }
        let mut payload = Vec::new();
        write_message_field(&mut payload, 28, &block);
        data_frame(&payload)
    }

    fn last_usage(deltas: &[AiStreamDelta]) -> &Usage {
        deltas
            .iter()
            .rev()
            .find_map(|d| match d {
                AiStreamDelta::Usage(u) => Some(u),
                _ => None,
            })
            .expect("usage delta")
    }

    #[test]
    fn usage_stats_carries_cache_and_reasoning_metrics() {
        let mut parser = DevinConnectStreamParser::new();
        let mut meta = Vec::new();
        write_varint_field(&mut meta, 2, 150);
        write_varint_field(&mut meta, 3, 42);
        let mut payload = Vec::new();
        write_message_field(&mut payload, 7, &meta);
        let mut wire = data_frame(&payload);
        wire.extend_from_slice(&usage_stats_frame(&[
            ("input_tokens", 150.0),
            ("output_tokens", 42.0),
            ("cached_input_tokens", 90.0),
            ("cache_creation_input_tokens", 12.0),
            ("reasoning_tokens", 7.0),
        ]));
        let deltas = parser.parse_chunk(&wire).unwrap();
        let usage = last_usage(&deltas);
        // Fresh input (150) + cache read (90) + cache write (12) — the billed
        // buckets add up to the processed prompt total.
        assert_eq!(usage.prompt_tokens, 252);
        assert_eq!(usage.completion_tokens, 42);
        assert_eq!(usage.cache_read_tokens, Some(90));
        assert_eq!(usage.cache_creation_tokens, Some(12));
        assert_eq!(usage.reasoning_tokens, Some(7));
    }

    #[test]
    fn usage_stats_without_cache_metrics_reads_as_zero() {
        let mut parser = DevinConnectStreamParser::new();
        let deltas = parser
            .parse_chunk(&usage_stats_frame(&[
                ("input_tokens", 200.0),
                ("output_tokens", 30.0),
            ]))
            .unwrap();
        let usage = last_usage(&deltas);
        assert_eq!(usage.prompt_tokens, 200);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.cache_read_tokens, Some(0));
        assert_eq!(usage.cache_creation_tokens, Some(0));
        assert!(usage.required_components_known);
    }

    #[test]
    fn usage_stats_alone_rebuilds_prompt_total() {
        // No #7 metadata: the billed fresh-input bucket plus the cache
        // buckets reconstruct the processed prompt total.
        let mut parser = DevinConnectStreamParser::new();
        let deltas = parser
            .parse_chunk(&usage_stats_frame(&[
                ("input_tokens", 603.0),
                ("output_tokens", 132.0),
                ("cache_read_input_tokens", 10240.0),
            ]))
            .unwrap();
        let usage = last_usage(&deltas);
        assert_eq!(usage.prompt_tokens, 10843);
        assert_eq!(usage.cache_read_tokens, Some(10240));
        assert_eq!(usage.cache_creation_tokens, Some(0));
    }

    #[test]
    fn metadata_only_usage_leaves_cache_unknown() {
        let mut parser = DevinConnectStreamParser::new();
        let mut meta = Vec::new();
        write_varint_field(&mut meta, 2, 150);
        write_varint_field(&mut meta, 3, 42);
        let mut payload = Vec::new();
        write_message_field(&mut payload, 7, &meta);
        let deltas = parser.parse_chunk(&data_frame(&payload)).unwrap();
        let usage = last_usage(&deltas);
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.cache_creation_tokens, None);
    }

    #[test]
    fn tool_call_fragments_merge_by_id() {
        let mut parser = DevinConnectStreamParser::new();
        // Frame 1: {id, name}; frame 2: args fragment (no id); frame 3: rest.
        let mut call1 = Vec::new();
        write_string_field(&mut call1, 1, "call_1");
        write_string_field(&mut call1, 2, "grep");
        let mut f1 = Vec::new();
        write_message_field(&mut f1, 6, &call1);
        let mut call2 = Vec::new();
        write_string_field(&mut call2, 3, "{\"pat");
        let mut f2 = Vec::new();
        write_message_field(&mut f2, 6, &call2);
        let mut call3 = Vec::new();
        write_string_field(&mut call3, 3, "tern\":\"x\"}");
        let mut f3 = Vec::new();
        write_message_field(&mut f3, 6, &call3);
        let mut wire = data_frame(&f1);
        wire.extend_from_slice(&data_frame(&f2));
        wire.extend_from_slice(&data_frame(&f3));
        wire.extend_from_slice(&trailer_frame("{}"));
        let deltas = parser.parse_chunk(&wire).unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            AiStreamDelta::ToolCallStart { index: 0, name, .. } if name == "grep"
        )));
        assert!(deltas.iter().any(|d| matches!(
            d,
            AiStreamDelta::ToolCallComplete { tool_call, .. }
                if tool_call.arguments == "{\"pattern\":\"x\"}"
        )));
    }

    #[test]
    fn trailer_error_surfaces_stream_error() {
        let mut parser = DevinConnectStreamParser::new();
        let wire = trailer_frame(r#"{"error":{"code":"resource_exhausted","message":"quota"}}"#);
        let deltas = parser.parse_chunk(&wire).unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            AiStreamDelta::StreamError { error } if error.kind == AiErrorKind::RateLimitError
        )));
    }

    #[test]
    fn missing_trailer_reports_unexpected_eof() {
        let mut parser = DevinConnectStreamParser::new();
        parser
            .parse_chunk(&data_frame(&text_payload("hi")))
            .unwrap();
        let tail = parser.finish().unwrap();
        assert!(
            tail.iter()
                .any(|d| matches!(d, AiStreamDelta::UnexpectedEof))
        );
    }

    #[test]
    fn malformed_data_frame_is_skipped() {
        let mut parser = DevinConnectStreamParser::new();
        let deltas = parser.parse_chunk(&data_frame(&[0x0b, 0x00])).unwrap();
        assert!(deltas.is_empty());
    }

    #[test]
    fn finish_flag_maps_to_stop() {
        let mut parser = DevinConnectStreamParser::new();
        let mut payload = Vec::new();
        write_varint_field(&mut payload, 5, 2);
        let mut wire = data_frame(&payload);
        wire.extend_from_slice(&trailer_frame("{}"));
        parser.parse_chunk(&wire).unwrap();
        let tail = parser.finish().unwrap();
        assert!(tail.iter().any(|d| matches!(
            d,
            AiStreamDelta::Done { stop_reason } if stop_reason == "stop"
        )));
    }
}

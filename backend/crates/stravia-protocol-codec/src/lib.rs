//! Standard wire codecs for Stravia.
//!
//! This crate owns the four public protocol families, their adapter registry,
//! and canonical request/response/stream transforms. Proprietary upstream wire
//! codecs live in their owning Wasm vendor crates and use the public transform
//! extension points without being linked into host builds.
//!
//! # Three-layer identity
//!
//! Canonical form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: closed `Protocol` enum (`openai-compatible` / `open-responses` / `anthropic-messages` / `google-gemini`).
//! - `name`: wire-format endpoint name (`chat-completions`, `responses`, `messages`, `generate-content`).
//! - `version`: dated or vendor schema version (`v1`, `2026-04-24`, `2023-06-01`, `v1beta`).
//!
//! See `stravia_runtime_contract::protocol::ids`, [`registry`], and the
//! crate-private protocol conversion tests for the model.
//!
//! ## Codec layout
//!
//! Each `codec/<vendor>/<protocol>/` directory co-locates one wire adapter.
//!
//! - `codec/openai/compatible/chat_completions.rs` — `OpenAICompatibleChatCompletionsV1`
//! - `codec/openai/compatible/embeddings.rs` — `OpenAICompatibleEmbeddingsV1`
//! - `codec/open_responses/adapter.rs` — `OpenResponses20260424`
//! - `codec/anthropic/messages/adapter.rs` — `AnthropicMessages2023`
//! - `codec/google/gemini/generate_content.rs` — `GoogleGeminiGenerateContentV1Beta`
//!
//! Shared semantic utilities live in `codec/reasoning.rs` and
//! `codec/tool_correlation.rs`.
//!
//! ## Alias table
//!
//! See [`registry::ProtocolRegistry`] for three-tier resolution of endpoint aliases
//! and [`registry::ProtocolRegistry::parse_protocol`] for Protocol-level resolution.

pub mod accumulator;
pub mod attachment;
pub mod codec;

#[cfg(test)]
mod conversion;
pub mod registry;
#[cfg(test)]
mod registry_tests;
pub mod transform;

// ── SSE helper ──

pub(crate) fn consume_sse_frames(
    buffer: &mut String,
    raw: &str,
    mut consume: impl FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let input = if buffer.is_empty() {
        std::borrow::Cow::Borrowed(raw)
    } else {
        buffer.push_str(raw);
        std::borrow::Cow::Owned(std::mem::take(buffer))
    };
    let mut consumed = 0;
    let result = (|| {
        while let Some((end, delimiter_len)) = sse_frame_boundary(&input[consumed..]) {
            let start = consumed;
            consumed += end + delimiter_len;
            consume(&input[start..start + end])?;
        }
        Ok(())
    })();
    match input {
        std::borrow::Cow::Borrowed(input) => buffer.push_str(&input[consumed..]),
        std::borrow::Cow::Owned(mut input) => {
            input.drain(..consumed);
            *buffer = input;
        }
    }
    result
}

fn sse_frame_boundary(buffer: &str) -> Option<(usize, usize)> {
    let bytes = buffer.as_bytes();
    let mut previous_eol = None;
    let mut index = 0;
    while index < bytes.len() {
        let eol_len = match bytes[index] {
            b'\n' => 1,
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
            b'\r' if index + 1 == bytes.len() => break,
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

#[cfg(test)]
mod sse_tests {
    #[test]
    fn borrowed_frames_preserve_partial_unicode_and_crlf() {
        let mut buffer = String::new();
        let mut frames = Vec::new();
        super::consume_sse_frames(
            &mut buffer,
            "data: 世界\r\n\r\ndata: second\n\ndata: par",
            |frame| {
                frames.push(frame.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(frames, ["data: 世界", "data: second"]);
        assert_eq!(buffer, "data: par");
        super::consume_sse_frames(&mut buffer, "tial\r\n\r\n", |frame| {
            assert_eq!(frame, "data: partial");
            Ok(())
        })
        .unwrap();
        assert!(buffer.is_empty());
    }

    #[test]
    fn failed_frame_is_consumed_but_later_frames_are_retained() {
        let mut buffer = "data: first\n\ndata: invalid\n\ndata: last\n\n".to_string();
        let mut seen = Vec::new();
        let result = super::consume_sse_frames(&mut buffer, "", |frame| {
            seen.push(frame.to_string());
            anyhow::ensure!(frame != "data: invalid", "invalid frame");
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(seen, ["data: first", "data: invalid"]);
        assert_eq!(buffer, "data: last\n\n");
    }
}

/// One Server-Sent Events frame: optional `event:` name plus `data:` payload.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn new(event: Option<&str>, data: impl Into<String>) -> Self {
        Self {
            event: event.map(|e| e.to_string()),
            data: data.into(),
        }
    }

    pub fn to_sse_string(&self) -> String {
        let capacity = 8 + self.data.len() + self.event.as_ref().map_or(0, |event| 8 + event.len());
        let mut s = String::with_capacity(capacity);
        if let Some(event) = &self.event {
            s.push_str("event: ");
            s.push_str(event);
            s.push('\n');
        }
        s.push_str("data: ");
        s.push_str(&self.data);
        s.push_str("\n\n");
        s
    }
}

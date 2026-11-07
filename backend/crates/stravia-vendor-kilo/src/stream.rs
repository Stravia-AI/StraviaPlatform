use super::*;
use std::borrow::Cow;
use stravia_protocol_codec::{
    accumulator::StreamResponseAccumulator, transform::ProtocolTransform,
};
use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind, AiResponse, AiStreamDelta};

pub(super) fn normalize(value: &mut Value) {
    if value.get("type").and_then(Value::as_str) == Some("response.failed")
        && let Some(error) = value
            .pointer_mut("/response/error")
            .and_then(Value::as_object_mut)
        && error.get("code").is_none_or(Value::is_null)
        && let Some(kind) = error
            .get("type")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    {
        error.insert("code".into(), Value::String(kind));
    }
}
pub(super) fn failure(value: &Value) -> Option<AiError> {
    let error_finish = value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices
                .iter()
                .any(|v| v.get("finish_reason").and_then(Value::as_str) == Some("error"))
        });
    let resource = value.get("response").unwrap_or(value);
    let failed_resource = resource.get("status").and_then(Value::as_str) == Some("failed");
    if !error_finish && !failed_resource && value.get("error").is_none() {
        return None;
    }
    let raw = resource
        .get("error")
        .or_else(|| value.get("error"))
        .unwrap_or(resource);
    let message = raw
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Kilo gateway stream failed");
    let status = raw
        .get("code")
        .and_then(Value::as_u64)
        .or_else(|| raw.get("status").and_then(Value::as_u64))
        .and_then(|v| u16::try_from(v).ok());
    let code = raw
        .get("code")
        .or_else(|| raw.get("type"))
        .and_then(Value::as_str);
    let kind = if status == Some(402)
        || matches!(
            code,
            Some("billing_error" | "insufficient_quota" | "insufficient_credits")
        ) {
        AiErrorKind::QuotaExceeded
    } else if let Some(status) = status {
        AiError::kind_from_status(status, Some(raw))
    } else {
        AiErrorKind::ServerError
    };
    let mut error = AiError::new(kind, message);
    if let Some(status) = status {
        error = error.with_status(status);
    }
    Some(error)
}
fn invalid(e: impl std::fmt::Display) -> PluginError {
    common::model_error(
        AiErrorKind::ServerError,
        format!("Invalid Kilo response: {e}"),
    )
}
// SSE framing operates on bytes, preserving split UTF-8 and CRLF boundaries.
#[derive(Default)]
pub(super) struct Frames {
    pending: Vec<u8>,
    consumed: usize,
}
impl Frames {
    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<Vec<&[u8]>, PluginError> {
        self.pending.drain(..self.consumed);
        self.consumed = 0;
        if self.pending.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(invalid("SSE event exceeds size limit"));
        }
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        let mut pos = 0;
        let mut line_start = 0;
        while pos < self.pending.len() {
            let byte = self.pending[pos];
            if byte != b'\r' && byte != b'\n' {
                pos += 1;
                continue;
            }
            if byte == b'\r' && pos + 1 == self.pending.len() {
                break;
            }
            let end = pos
                + if byte == b'\r' && self.pending.get(pos + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            if pos == line_start {
                events.push(&self.pending[start..end]);
                start = end;
            }
            line_start = end;
            pos = end;
        }
        self.consumed = start;
        Ok(events)
    }
    pub(super) fn finish(&mut self) -> Vec<u8> {
        self.pending.drain(..self.consumed);
        self.consumed = 0;
        std::mem::take(&mut self.pending)
    }
}
pub(super) fn event(event: &[u8]) -> Result<(Cow<'_, [u8]>, Option<AiError>), PluginError> {
    let text = std::str::from_utf8(event).map_err(invalid)?;
    let mut lines = text.split(['\r', '\n']).filter_map(|line| {
        line.strip_prefix("data:")
            .map(|v| v.strip_prefix(' ').unwrap_or(v))
    });
    let mut payload = Cow::Borrowed(lines.next().unwrap_or_default());
    for line in lines {
        let payload = payload.to_mut();
        payload.push('\n');
        payload.push_str(line);
    }
    if payload.is_empty() || payload == "[DONE]" {
        return Ok((Cow::Borrowed(event), None));
    }
    let mut value: Value = serde_json::from_str(&payload).map_err(invalid)?;
    let failure = failure(&value);
    normalize(&mut value);
    let name = text
        .split(['\r', '\n'])
        .find_map(|line| line.strip_prefix("event:").map(str::trim))
        .or_else(|| value.get("type").and_then(Value::as_str));
    let wire = if let Some(name) = name {
        if name.contains(['\r', '\n']) {
            return Err(invalid("Invalid SSE event type"));
        }
        format!("event: {name}\ndata: {value}\n\n")
    } else {
        format!("data: {value}\n\n")
    };
    Ok((Cow::Owned(wire.into_bytes()), failure))
}
pub(super) fn decode(
    host: &GuestHost,
    protocol: &str,
    response: HttpResponse,
) -> Result<AiResponse, PluginError> {
    let status = response.status()?;
    let headers = response.headers()?;
    if !(200..300).contains(&status) {
        let bytes = read_http_body(&response, MAX_BODY)?;
        // 402 is a billing failure, not a request to replace valid credentials.
        if status == 402 {
            let mut error =
                common::model_error(AiErrorKind::QuotaExceeded, "Kilo balance is insufficient");
            error.upstream_status = Some(402);
            return Err(error);
        }
        return Err(common::upstream_error(status, &headers, &bytes));
    }
    let endpoint = common::endpoint(protocol)?;
    if !headers.iter().any(|field| {
        field.name.eq_ignore_ascii_case("content-type")
            && String::from_utf8_lossy(&field.bytes).to_ascii_lowercase().contains("text/event-stream")
    }) {
        let bytes = read_http_body(&response, MAX_BODY)?;
        let mut value: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        if let Some(error) = failure(&value) {
            return Err(common::model_error(error.kind, error.message));
        }
        normalize(&mut value);
        return ProtocolTransform::global()
            .bind(endpoint, endpoint)
            .map_err(invalid)?
            .decode_response(value)
            .map_err(invalid);
    }
    let mut decoder = ProtocolTransform::global()
        .decode_stream(endpoint)
        .map_err(invalid)?;
    let mut accumulator = StreamResponseAccumulator::default();
    let mut frames = Frames::default();
    while let Some(chunk) = response.read_body()? {
        for frame in frames.push(&chunk)? {
            let (wire, error) = event(frame)?;
            if let Some(error) = error {
                common::emit_deltas(
                    host,
                    &mut accumulator,
                    &[AiStreamDelta::StreamError { error }],
                )?;
            }
            let deltas = decoder.decode_chunk(&wire).map_err(invalid)?;
            common::emit_deltas(host, &mut accumulator, &deltas)?;
        }
    }
    let last = frames.finish();
    if !last.is_empty() {
        let (wire, error) = event(&last)?;
        if let Some(error) = error {
            common::emit_deltas(
                host,
                &mut accumulator,
                &[AiStreamDelta::StreamError { error }],
            )?;
        }
        common::emit_deltas(
            host,
            &mut accumulator,
            &decoder.decode_chunk(&wire).map_err(invalid)?,
        )?;
    }
    common::emit_deltas(host, &mut accumulator, &decoder.finish().map_err(invalid)?)?;
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(complete)
}

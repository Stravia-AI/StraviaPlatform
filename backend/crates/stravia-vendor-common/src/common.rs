use std::time::Duration;

use chrono::{DateTime, Utc};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_protocol_codec::transform::{EncodedRequest, ProtocolTransform, TransformError};
use stravia_runtime_contract::protocol::ids::{Protocol, ProtocolEndpoint};
use stravia_runtime_contract::protocol::ir::{
    AiError, AiErrorKind, AiRequest, AiResponse, AiStreamDelta, NativeCompactionResponse,
};
use stravia_vendor_sdk::{
    ErrorKind, GuestHost, HttpResponse, OperationOutput, PluginError, read_http_body,
};

const MAX_ERROR_BODY: usize = 256 * 1024;
const MAX_UNARY_BODY: usize = 32 * 1024 * 1024;

pub fn plugin_error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

pub fn model_error(kind: AiErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::upstream(Some(kind), None),
        message: message.into(),
        upstream_status: None,
    }
}

pub fn unsupported(operation: &str, vendor_id: &str, channel: &str) -> PluginError {
    plugin_error(
        ErrorKind::Unsupported,
        format!("{vendor_id}/{channel} does not support {operation}"),
    )
}

pub fn endpoint(protocol: &str) -> Result<ProtocolEndpoint, PluginError> {
    ProtocolRegistry::global()
        .resolve_alias(protocol)
        .ok_or_else(|| {
            plugin_error(
                ErrorKind::Unsupported,
                format!("unsupported protocol endpoint `{protocol}`"),
            )
        })
}

/// 拒绝无法兑现的原生压缩要求，避免把能力缺失误报为普通输入错误。
pub fn ensure_no_native_compaction(request: &AiRequest) -> Result<(), PluginError> {
    if stravia_protocol_codec::codec::compaction::native_compaction_requested(request) {
        return Err(plugin_error(
            ErrorKind::Unsupported,
            "native compaction requires the Open Responses protocol",
        ));
    }
    Ok(())
}

pub fn encode_inference_request(
    protocol: &str,
    request: &AiRequest,
) -> Result<EncodedRequest, PluginError> {
    let egress = endpoint(protocol)?;
    if egress.protocol != Protocol::OpenResponses {
        ensure_no_native_compaction(request)?;
    }
    let ingress = ProtocolTransform::inferred_ingress(request).unwrap_or(egress);
    ProtocolTransform::global()
        .bind(ingress, egress)
        .and_then(|pair| pair.encode_request(request))
        .map_err(map_request_transform_error)
}

pub fn header_pairs(headers: &http::HeaderMap) -> Result<Vec<(String, String)>, PluginError> {
    headers
        .iter()
        .map(|(name, value)| {
            value
                .to_str()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
                .map_err(|_| {
                    plugin_error(ErrorKind::Invalid, "codec produced a non-text HTTP header")
                })
        })
        .collect()
}

/// Join a codec-owned absolute path to a configured base URL without producing
/// `/v1/v1/...` for endpoints whose saved base already ends in a version segment.
pub fn endpoint_url(base_url: &str, path: &str) -> Result<String, PluginError> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() {
        return Err(plugin_error(
            ErrorKind::Invalid,
            "provider base URL is empty",
        ));
    }
    let path = if base_ends_with_version_segment(base) {
        strip_leading_version(path).unwrap_or(path)
    } else {
        path
    };
    Ok(format!("{base}/{}", path.trim_start_matches('/')))
}

/// Resolve the conventional model inventory relative to the configured API base.
/// Root origins use the OpenAI `/v1/models` default; an explicit base path is
/// already an API prefix and receives only `/models`.
pub fn model_discovery_url(base_url: &str) -> Result<String, PluginError> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() {
        return Err(plugin_error(
            ErrorKind::Invalid,
            "provider base URL is empty",
        ));
    }
    let authority_and_path = base.split_once("://").map_or(base, |(_, rest)| rest);
    let has_api_path = authority_and_path
        .split_once('/')
        .is_some_and(|(_, path)| !path.is_empty());
    Ok(format!(
        "{base}/{}",
        if has_api_path { "models" } else { "v1/models" }
    ))
}

fn strip_leading_version(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/')?;
    let (first, rest) = path.split_once('/')?;
    is_version_segment(first).then_some(rest)
}

fn base_ends_with_version_segment(base: &str) -> bool {
    let without_query = base.split(['?', '#']).next().unwrap_or(base);
    without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(is_version_segment)
}

fn is_version_segment(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('v') else {
        return false;
    };
    let digit_count = rest.bytes().take_while(u8::is_ascii_digit).count();
    digit_count > 0
        && rest[digit_count..]
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic())
}

/// Decode an inference response with the shared codec. Streaming bodies are
/// decoded and emitted chunk-by-chunk; they are never buffered before emit.
pub fn decode_inference(
    host: &GuestHost,
    protocol: &str,
    response: HttpResponse,
) -> Result<OperationOutput, PluginError> {
    decode_ai_response(host, protocol, response)
        .map(Box::new)
        .map(OperationOutput::Infer)
}

/// Decode standalone Open Responses compaction while retaining a failed
/// Provider envelope in the canonical terminal event for direct relay.
pub fn decode_compaction_preserving_upstream_errors(
    host: &GuestHost,
    response: HttpResponse,
) -> Result<NativeCompactionResponse, PluginError> {
    let status = response.status()?;
    if (200..300).contains(&status) {
        return decode_compaction(response);
    }
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_ERROR_BODY)?;
    let failure = upstream_error(status, &headers, &body);
    if let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&body) {
        let error = AiError::new(
            AiError::kind_from_status(status, Some(&raw)),
            failure.message.clone(),
        )
        .with_status(status)
        .with_raw(raw);
        host.emit_delta(&AiStreamDelta::StreamError { error })?;
    }
    Err(failure)
}

pub fn decode_compaction(response: HttpResponse) -> Result<NativeCompactionResponse, PluginError> {
    let status = response.status()?;
    let headers = response.headers()?;
    if !(200..300).contains(&status) {
        let body = read_http_body(&response, MAX_ERROR_BODY)?;
        return Err(upstream_error(status, &headers, &body));
    }
    let body = read_http_body(&response, MAX_UNARY_BODY)?;
    let value = serde_json::from_slice(&body).map_err(|error| {
        model_error(
            AiErrorKind::ServerError,
            format!("upstream returned invalid compaction JSON: {error}"),
        )
    })?;
    stravia_protocol_codec::codec::open_responses::parser::parse_compaction_response(&value)
        .map_err(|error| {
            model_error(
                AiErrorKind::ServerError,
                format!("invalid Open Responses compaction response: {error}"),
            )
        })
}

pub fn decode_ai_response(
    host: &GuestHost,
    protocol: &str,
    response: HttpResponse,
) -> Result<AiResponse, PluginError> {
    decode_ai_response_with_error_classifier(host, protocol, response, no_http_stream_error)
}

pub fn decode_ai_response_with_error_classifier(
    host: &GuestHost,
    protocol: &str,
    response: HttpResponse,
    classify_error: fn(&serde_json::Value, bool) -> Option<PluginError>,
) -> Result<AiResponse, PluginError> {
    decode_ai_response_with_error_policy(host, protocol, response, classify_error, false)
}

/// Decode a response while retaining the exact upstream error envelope in the
/// canonical terminal event. This is restricted to operations such as native
/// compaction whose public contract is to relay the Provider error unchanged.
pub fn decode_ai_response_preserving_upstream_errors(
    host: &GuestHost,
    protocol: &str,
    response: HttpResponse,
    classify_error: fn(&serde_json::Value, bool) -> Option<PluginError>,
) -> Result<AiResponse, PluginError> {
    decode_ai_response_with_error_policy(host, protocol, response, classify_error, true)
}

fn decode_ai_response_with_error_policy(
    host: &GuestHost,
    protocol: &str,
    response: HttpResponse,
    classify_error: fn(&serde_json::Value, bool) -> Option<PluginError>,
    preserve_upstream_errors: bool,
) -> Result<AiResponse, PluginError> {
    let status = response.status()?;
    let headers = response.headers()?;
    if !(200..300).contains(&status) {
        let body = read_http_body(&response, MAX_ERROR_BODY)?;
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
            if preserve_upstream_errors {
                let upstream = upstream_error(status, &headers, &body);
                let error = AiError::new(
                    AiError::kind_from_status(status, Some(&value)),
                    upstream.message.clone(),
                )
                .with_status(status)
                .with_raw(value);
                host.emit_delta(&AiStreamDelta::StreamError { error })?;
                return Err(upstream);
            }
            if let Some(mut error) = classify_error(&value, false) {
                error.upstream_status.get_or_insert(status);
                return Err(error);
            }
        }
        return Err(upstream_error(status, &headers, &body));
    }

    let endpoint = endpoint(protocol)?;
    let streaming = headers.iter().any(|(name, value)| {
        if !name.eq_ignore_ascii_case("content-type") {
            return false;
        }
        let value = value.to_ascii_lowercase();
        value.contains("text/event-stream")
            || value.contains("application/x-ndjson")
            || value.contains("application/connect+proto")
            || value.contains("application/vnd.amazon.eventstream")
    });

    if !streaming {
        let body = read_http_body(&response, MAX_UNARY_BODY)?;
        let value = serde_json::from_slice(&body).map_err(|error| {
            model_error(
                AiErrorKind::ServerError,
                format!("upstream returned invalid JSON: {error}"),
            )
        })?;
        let pair = ProtocolTransform::global()
            .bind(endpoint, endpoint)
            .map_err(map_response_transform_error)?;
        return pair
            .decode_response(value)
            .map_err(map_response_transform_error);
    }

    let mut decoder = ProtocolTransform::global()
        .decode_stream(endpoint)
        .map_err(map_response_transform_error)?;
    let mut accumulator = StreamResponseAccumulator::default();
    let mut saw_response_event = false;
    while let Some(chunk) = response.read_body()? {
        let deltas = decoder
            .decode_chunk(&chunk)
            .map_err(map_response_transform_error)?;
        if !preserve_upstream_errors {
            classify_stream_error_before_emit(
                endpoint.protocol,
                saw_response_event,
                &deltas,
                classify_error,
            )?;
        }
        saw_response_event |= deltas.iter().any(is_response_event);
        emit_deltas_with_policy(host, &mut accumulator, &deltas, preserve_upstream_errors)?;
    }
    let deltas = decoder.finish().map_err(map_response_transform_error)?;
    if !preserve_upstream_errors {
        classify_stream_error_before_emit(
            endpoint.protocol,
            saw_response_event,
            &deltas,
            classify_error,
        )?;
    }
    emit_deltas_with_policy(host, &mut accumulator, &deltas, preserve_upstream_errors)?;
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(complete)
}

/// Decode Open Responses-style WebSocket JSON events through the same shared
/// stream codec. Each text frame is wrapped as one SSE event solely for the
/// codec parser; canonical deltas are still emitted immediately.
pub fn decode_ai_response_websocket(
    host: &GuestHost,
    protocol: &str,
    connection: stravia_vendor_sdk::WsConnection,
) -> Result<AiResponse, PluginError> {
    decode_normalized_websocket(host, protocol, connection, false, false, no_websocket_error)
}

pub fn decode_ai_response_websocket_with_error_classifier(
    host: &GuestHost,
    protocol: &str,
    connection: stravia_vendor_sdk::WsConnection,
    preserve_upstream_errors: bool,
    classify_error: fn(&serde_json::Value, bool) -> Option<PluginError>,
) -> Result<AiResponse, PluginError> {
    decode_normalized_websocket(
        host,
        protocol,
        connection,
        false,
        preserve_upstream_errors,
        classify_error,
    )
}

pub fn decode_codex_ai_response_websocket(
    host: &GuestHost,
    protocol: &str,
    connection: stravia_vendor_sdk::WsConnection,
) -> Result<AiResponse, PluginError> {
    decode_normalized_websocket(host, protocol, connection, true, false, no_websocket_error)
}

pub fn decode_codex_ai_response_websocket_with_error_classifier(
    host: &GuestHost,
    protocol: &str,
    connection: stravia_vendor_sdk::WsConnection,
    preserve_upstream_errors: bool,
    classify_error: fn(&serde_json::Value, bool) -> Option<PluginError>,
) -> Result<AiResponse, PluginError> {
    decode_normalized_websocket(
        host,
        protocol,
        connection,
        true,
        preserve_upstream_errors,
        classify_error,
    )
}

fn no_websocket_error(_: &serde_json::Value, _: bool) -> Option<PluginError> {
    None
}

fn decode_normalized_websocket(
    host: &GuestHost,
    protocol: &str,
    connection: stravia_vendor_sdk::WsConnection,
    drop_response_metadata: bool,
    preserve_upstream_errors: bool,
    classify_error: fn(&serde_json::Value, bool) -> Option<PluginError>,
) -> Result<AiResponse, PluginError> {
    let endpoint = endpoint(protocol)?;
    let mut decoder = ProtocolTransform::global()
        .decode_stream(endpoint)
        .map_err(map_response_transform_error)?;
    let mut accumulator = StreamResponseAccumulator::default();
    let mut terminal = false;
    let mut saw_response_event = false;
    while !terminal {
        let Some(message) = connection.next()? else {
            break;
        };
        let text = match message {
            stravia_vendor_sdk::WsMessage::Text(text) => text,
            stravia_vendor_sdk::WsMessage::Ping(payload) => {
                connection.send(&stravia_vendor_sdk::WsMessage::Pong(payload))?;
                continue;
            }
            stravia_vendor_sdk::WsMessage::Pong(_) => continue,
            stravia_vendor_sdk::WsMessage::Close(_) => break,
            stravia_vendor_sdk::WsMessage::Binary(_) => {
                return Err(model_error(
                    AiErrorKind::StreamMidError,
                    "inference WebSocket returned an unexpected binary event",
                ));
            }
        };
        let mut value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
            model_error(
                AiErrorKind::StreamMidError,
                format!("inference WebSocket returned invalid JSON: {error}"),
            )
        })?;
        let original_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("message");
        if !preserve_upstream_errors
            && original_type == "error"
            && !saw_response_event
            && previous_response_not_found(&value)
        {
            let _ = connection.close(false);
            return Err(PluginError {
                kind: ErrorKind::ContinuationNotFound,
                message: "upstream continuation is no longer available".into(),
                upstream_status: value
                    .get("status")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|status| u16::try_from(status).ok()),
            });
        }
        if !preserve_upstream_errors && let Some(error) = classify_error(&value, saw_response_event)
        {
            let _ = connection.close(false);
            return Err(error);
        }
        if !preserve_upstream_errors
            && original_type == "error"
            && value
                .pointer("/error/code")
                .or_else(|| value.get("code"))
                .and_then(serde_json::Value::as_str)
                == Some("websocket_connection_limit_reached")
        {
            return Err(plugin_error(
                ErrorKind::upstream_transport(
                    Some(AiErrorKind::ServiceUnavailable),
                    None,
                    stravia_vendor_sdk::TransportFailure::Websocket,
                ),
                "inference WebSocket connection limit reached",
            ));
        }
        if original_type.starts_with("codex.")
            || original_type.starts_with("responsesapi.")
            || (drop_response_metadata && original_type == "response.metadata")
        {
            continue;
        }
        if original_type.starts_with("response.") {
            saw_response_event = true;
        }
        if original_type == "response.done" {
            let normalized = match value
                .pointer("/response/status")
                .and_then(serde_json::Value::as_str)
            {
                Some("completed") => "response.completed",
                Some("incomplete") => "response.incomplete",
                Some("failed") => "response.failed",
                status => {
                    return Err(model_error(
                        AiErrorKind::StreamMidError,
                        format!(
                            "Responses WebSocket response.done has invalid terminal status: {status:?}"
                        ),
                    ));
                }
            };
            value["type"] = serde_json::Value::String(normalized.to_owned());
        }
        let event_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("message");
        let text = serde_json::to_string(&value).map_err(|error| {
            plugin_error(
                ErrorKind::Trapped,
                format!("failed to normalize WebSocket event: {error}"),
            )
        })?;
        terminal = matches!(
            event_type,
            "response.completed" | "response.failed" | "response.incomplete"
        ) || (event_type == "response.done"
            && value
                .pointer("/response/status")
                .and_then(serde_json::Value::as_str)
                .is_some());
        let frame = format!("event: {event_type}\ndata: {text}\n\n");
        let deltas = decoder
            .decode_chunk(frame.as_bytes())
            .map_err(map_response_transform_error)?;
        emit_deltas_with_policy(host, &mut accumulator, &deltas, preserve_upstream_errors)?;
    }
    let deltas = decoder.finish().map_err(map_response_transform_error)?;
    emit_deltas_with_policy(host, &mut accumulator, &deltas, preserve_upstream_errors)?;
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    let _ = connection.close(true);
    Ok(complete)
}

fn no_http_stream_error(_: &serde_json::Value, _: bool) -> Option<PluginError> {
    None
}

fn classify_stream_error_before_emit(
    protocol: Protocol,
    mut saw_response_event: bool,
    deltas: &[AiStreamDelta],
    classify_error: fn(&serde_json::Value, bool) -> Option<PluginError>,
) -> Result<(), PluginError> {
    for delta in deltas {
        if let AiStreamDelta::StreamError { error } = delta
            && let Some(raw) = &error.raw
        {
            if protocol == Protocol::OpenResponses
                && !saw_response_event
                && previous_response_not_found(raw)
            {
                return Err(PluginError {
                    kind: ErrorKind::ContinuationNotFound,
                    message: "upstream continuation is no longer available".into(),
                    upstream_status: error.status_code,
                });
            }
            if let Some(error) = classify_error(raw, saw_response_event) {
                return Err(error);
            }
        }
        saw_response_event |= is_response_event(delta);
    }
    Ok(())
}

fn previous_response_not_found(value: &serde_json::Value) -> bool {
    value
        .pointer("/error/code")
        .or_else(|| value.pointer("/response/error/code"))
        .or_else(|| value.get("code"))
        .and_then(serde_json::Value::as_str)
        == Some("previous_response_not_found")
}

fn is_response_event(delta: &AiStreamDelta) -> bool {
    !matches!(
        delta,
        AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
    )
}

pub fn emit_deltas(
    host: &GuestHost,
    accumulator: &mut StreamResponseAccumulator,
    deltas: &[AiStreamDelta],
) -> Result<(), PluginError> {
    emit_deltas_with_policy(host, accumulator, deltas, false)
}

fn emit_deltas_with_policy(
    host: &GuestHost,
    accumulator: &mut StreamResponseAccumulator,
    deltas: &[AiStreamDelta],
    preserve_upstream_errors: bool,
) -> Result<(), PluginError> {
    const MESSAGE: &str = "upstream inference stream ended with an error";
    let terminal = deltas.iter().find_map(|delta| match delta {
        AiStreamDelta::StreamError { error } => Some((
            error.kind.clone(),
            error.status_code,
            error.message.as_str(),
        )),
        AiStreamDelta::UnexpectedEof => Some((AiErrorKind::UnexpectedEof, None, MESSAGE)),
        _ => None,
    });
    for delta in deltas {
        if let AiStreamDelta::StreamError { error } = delta {
            if preserve_upstream_errors {
                host.emit_delta(delta)?;
                accumulator.apply(delta);
            } else {
                // 宿主按连接秘密保护消息；丢弃非原生路径的 raw，不丢弃实际错误原因。
                let mut canonical = AiError::new(error.kind.clone(), error.message.clone());
                canonical.status_code = error.status_code;
                let canonical = AiStreamDelta::StreamError { error: canonical };
                host.emit_delta(&canonical)?;
                accumulator.apply(&canonical);
            }
        } else {
            host.emit_delta(delta)?;
            accumulator.apply(delta);
        }
    }
    if let Some((kind, status, message)) = terminal {
        if !preserve_upstream_errors {
            host.emit_failed(model_error_with_facts(kind.clone(), status, None, message))?;
        }
        return Err(model_error_with_facts(kind, status, None, message));
    }
    Ok(())
}

pub fn upstream_error(status: u16, headers: &[(String, String)], body: &[u8]) -> PluginError {
    let value = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("upstream HTTP {status}"));
    let continuation_missing = (matches!(status, 400 | 404)
        && value
            .pointer("/error/code")
            .or_else(|| value.get("code"))
            .and_then(serde_json::Value::as_str)
            == Some("previous_response_not_found"))
        || (status == 404
            && value.get("code").and_then(serde_json::Value::as_str) == Some("not-found")
            && value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|message| {
                    message.starts_with("Previous response cannot be used")
                        && message.contains("due to Zero Data Retention")
                }));
    if continuation_missing {
        return PluginError {
            kind: ErrorKind::ContinuationNotFound,
            message,
            upstream_status: Some(status),
        };
    }

    model_error_with_facts(
        AiError::kind_from_status(status, Some(&value)),
        Some(status),
        retry_after(headers),
        message,
    )
}

fn model_error_with_facts(
    kind: AiErrorKind,
    status: Option<u16>,
    retry_after: Option<Duration>,
    message: impl Into<String>,
) -> PluginError {
    PluginError {
        kind: ErrorKind::upstream(Some(kind), retry_after),
        message: message.into(),
        upstream_status: status,
    }
}

fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| parse_retry_after(value, Utc::now()))
}

fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    let milliseconds = deadline
        .signed_duration_since(now)
        .num_milliseconds()
        .max(0);
    Some(Duration::from_millis(milliseconds as u64))
}

pub fn map_request_transform_error(error: TransformError) -> PluginError {
    let kind = match error {
        TransformError::Unsupported { .. } | TransformError::UnsupportedOperation { .. } => {
            ErrorKind::Unsupported
        }
        TransformError::Unrepresentable { .. } | TransformError::Wire { .. } => ErrorKind::Invalid,
        TransformError::StreamClosed { .. } => ErrorKind::Trapped,
    };
    plugin_error(kind, error.to_string())
}

pub fn map_response_transform_error(error: TransformError) -> PluginError {
    let message = error.to_string();
    match error {
        TransformError::Unsupported { .. } | TransformError::UnsupportedOperation { .. } => {
            plugin_error(ErrorKind::Unsupported, message)
        }
        TransformError::Wire { .. } | TransformError::Unrepresentable { .. } => {
            model_error(AiErrorKind::ServerError, message)
        }
        TransformError::StreamClosed { .. } => plugin_error(ErrorKind::Trapped, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_join_does_not_duplicate_saved_version_segment() {
        assert_eq!(
            endpoint_url("https://upstream.test/v1/", "/v1/chat/completions").unwrap(),
            "https://upstream.test/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://upstream.test/custom", "/v1/chat/completions").unwrap(),
            "https://upstream.test/custom/v1/chat/completions"
        );
    }

    #[test]
    fn model_discovery_uses_v1_only_for_root_origins() {
        assert_eq!(
            model_discovery_url("https://upstream.test").unwrap(),
            "https://upstream.test/v1/models"
        );
        assert_eq!(
            model_discovery_url("https://upstream.test/v1/").unwrap(),
            "https://upstream.test/v1/models"
        );
        assert_eq!(
            model_discovery_url("https://upstream.test/openai").unwrap(),
            "https://upstream.test/openai/models"
        );
    }

    #[test]
    fn quota_and_rate_limit_remain_distinct() {
        let quota = upstream_error(
            429,
            &[],
            br#"{"error":{"type":"insufficient_quota","message":"limit"}}"#,
        );
        assert_eq!(
            quota.kind.model_error_kind(),
            Some(AiErrorKind::QuotaExceeded)
        );
        assert_eq!(quota.kind.retry_after(), None);

        let rate_limit = upstream_error(
            429,
            &[("Retry-After".into(), "7".into())],
            br#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        assert_eq!(
            rate_limit.kind.model_error_kind(),
            Some(AiErrorKind::RateLimitError)
        );
        assert_eq!(rate_limit.kind.retry_after(), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_accepts_http_date() {
        let now = DateTime::parse_from_rfc2822("Sun, 06 Nov 1994 08:49:00 GMT")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::from_secs(37))
        );
    }

    #[test]
    fn generic_not_found_is_not_continuation_evidence() {
        let missing = upstream_error(
            404,
            &[],
            br#"{"error":{"code":"not_found","message":"model unavailable"}}"#,
        );
        assert!(!matches!(&missing.kind, ErrorKind::ContinuationNotFound));
        assert_eq!(
            missing.kind.model_error_kind(),
            Some(AiErrorKind::ModelNotAvailable)
        );

        let expired = upstream_error(
            404,
            &[],
            br#"{"error":{"code":"previous_response_not_found","message":"expired"}}"#,
        );
        assert!(matches!(expired.kind, ErrorKind::ContinuationNotFound));
    }
}

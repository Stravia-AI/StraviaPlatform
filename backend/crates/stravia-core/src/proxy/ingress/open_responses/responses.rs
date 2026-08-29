//! Thin ingress shell: POST /v1/responses

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::Gateway;
use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
use crate::protocol::ir::RawEnvelope;
use crate::protocol::transform::ProtocolTransform;
use crate::proxy::context::RequestContext;
use crate::proxy::dispatcher::{dispatch_pipeline, log_decode_error};
use crate::proxy::security::{ClientCredential, Security};

pub async fn handler(
    State(gw): State<Gateway>,
    mut ctx: axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return json_rejection(rejection),
    };
    if !has_unambiguous_bearer(&headers) {
        return authentication_error();
    }
    if body.get("background").and_then(Value::as_bool) == Some(true) {
        return protocol_error(
            StatusCode::BAD_REQUEST,
            "unsupported_feature",
            Some("background"),
            "Background responses are not supported.",
        );
    }
    ctx.ingress_protocol = OPEN_RESPONSES_2026_04_24;
    let flat_headers: std::collections::HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|vs| (k.as_str().to_lowercase(), vs.to_string()))
        })
        .collect();
    let envelope = RawEnvelope::new(Some(body.clone()), flat_headers, "POST", "/v1/responses");
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered ingress adapter");
    let request = match pair.decode_request(body) {
        Ok(request) => request,
        Err(error) => {
            let _ = log_decode_error(&gw, &envelope, OPEN_RESPONSES_2026_04_24, error);
            return protocol_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                None,
                "Request body is not a compatible Responses request.",
            );
        }
    };
    let response = dispatch_pipeline(
        gw,
        headers,
        envelope,
        request,
        OPEN_RESPONSES_2026_04_24,
        ctx.0,
    )
    .await;
    normalize_error_response(response).await
}

pub async fn compact(
    State(gw): State<Gateway>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    if let Err(response) = authenticate(&gw, &headers).await {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return json_rejection(rejection),
    };
    if let Err((param, message)) = validate_compact_request(&body) {
        return protocol_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some(param),
            message,
        );
    }
    protocol_error(
        StatusCode::BAD_REQUEST,
        "unsupported_feature",
        Some("compact"),
        "Response compaction is not supported.",
    )
}

fn validate_compact_request(body: &Value) -> Result<(), (&'static str, String)> {
    const ALLOWED_FIELDS: &[&str] = &[
        "model",
        "input",
        "previous_response_id",
        "instructions",
        "prompt_cache_key",
    ];
    let object = body
        .as_object()
        .ok_or(("body", "compact request must be a JSON object".into()))?;
    if let Some(field) = object
        .keys()
        .find(|field| !ALLOWED_FIELDS.contains(&field.as_str()))
    {
        return Err(("body", format!("unknown compact request field '{field}'")));
    }
    if !object.get("model").is_some_and(Value::is_string) {
        return Err(("model", "compact request requires string 'model'".into()));
    }
    for field in ["previous_response_id", "instructions"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return Err((field, format!("'{field}' must be a string or null")));
        }
    }
    if let Some(value) = object.get("prompt_cache_key") {
        if !value.is_null() && !value.is_string() {
            return Err((
                "prompt_cache_key",
                "'prompt_cache_key' must be a string or null".into(),
            ));
        }
        if value
            .as_str()
            .is_some_and(|value| value.chars().count() > 64)
        {
            return Err((
                "prompt_cache_key",
                "'prompt_cache_key' exceeds maximum length 64".into(),
            ));
        }
    }
    if let Some(input) = object.get("input") {
        match input {
            Value::Null => {}
            Value::String(text) if text.len() <= 10 * 1024 * 1024 => {}
            Value::String(_) => {
                return Err(("input", "'input' exceeds maximum length 10485760".into()));
            }
            Value::Array(items) => {
                let mut ordinary = Vec::with_capacity(items.len());
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("compaction") {
                        validate_compaction_item(item)?;
                    } else {
                        ordinary.push(item.clone());
                    }
                }
                if !ordinary.is_empty() {
                    let validation = serde_json::json!({
                        "model": "compact-schema-validation",
                        "input": ordinary,
                    });
                    ProtocolTransform::global()
                        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
                        .expect("registered Open Responses pair")
                        .decode_request(validation)
                        .map_err(|error| ("input", error.to_string()))?;
                }
            }
            _ => {
                return Err(("input", "'input' must be a string, array, or null".into()));
            }
        }
    }
    Ok(())
}

fn validate_compaction_item(item: &Value) -> Result<(), (&'static str, String)> {
    let object = item
        .as_object()
        .ok_or(("input", "compact input items must be objects".into()))?;
    if object
        .get("id")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err((
            "input",
            "compaction item 'id' must be a string or null".into(),
        ));
    }
    let Some(encrypted_content) = object.get("encrypted_content").and_then(Value::as_str) else {
        return Err((
            "input",
            "compaction item requires string 'encrypted_content'".into(),
        ));
    };
    if encrypted_content.len() > 10 * 1024 * 1024 {
        return Err((
            "input",
            "compaction item 'encrypted_content' exceeds maximum length 10485760".into(),
        ));
    }
    Ok(())
}

pub(super) fn has_unambiguous_bearer(headers: &HeaderMap) -> bool {
    if headers.contains_key("x-api-key") || headers.contains_key("x-goog-api-key") {
        return false;
    }
    let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or_default();
    let token = parts.next().unwrap_or_default().trim();
    scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()
}

pub(super) async fn authenticate(gateway: &Gateway, headers: &HeaderMap) -> Result<(), Response> {
    if !has_unambiguous_bearer(headers) {
        return Err(authentication_error());
    }
    let credential = ClientCredential::from_inference_headers(headers);
    match Security::new(gateway.storage.auth())
        .required_principal(&credential)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => Err(normalize_error_response(error.render(None)).await),
    }
}

pub(super) fn authentication_error() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": {
                "type": "authentication_error",
                "code": "invalid_authentication",
                "param": "authorization",
                "message": "Authorization must contain exactly one non-empty Bearer token.",
            }
        })),
    )
        .into_response()
}

fn json_rejection(rejection: JsonRejection) -> Response {
    if rejection.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        return protocol_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            Some("content_type"),
            "Content-Type must be application/json.",
        );
    }
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return protocol_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            None,
            "Request body exceeds the 100 MiB limit.",
        );
    }
    protocol_error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        None,
        "Request body must be valid JSON.",
    )
}

pub(crate) fn protocol_error(
    status: StatusCode,
    code: &'static str,
    param: Option<&'static str>,
    message: impl Into<String>,
) -> Response {
    let message = message.into();
    let error_type = if code == "unsupported_feature" {
        "invalid_request"
    } else {
        code
    };
    (
        status,
        Json(serde_json::json!({
            "error": {
                "code": code,
                "message": message,
                "param": param,
                "type": error_type,
            }
        })),
    )
        .into_response()
}

pub(super) async fn normalize_error_response(response: Response) -> Response {
    let status = response.status();
    if status.is_success() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, 1024 * 1024).await.unwrap_or_default();
    let decoded = serde_json::from_slice::<Value>(&body).ok();
    let message = decoded
        .as_ref()
        .and_then(|body| body.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("Request failed")
                .to_owned()
        });
    let existing_code = decoded
        .as_ref()
        .and_then(|body| body.pointer("/error/code"))
        .and_then(Value::as_str)
        .and_then(|code| match code {
            "invalid_request" => Some("invalid_request"),
            "unsupported_feature" => Some("unsupported_feature"),
            "previous_response_not_found" => Some("previous_response_not_found"),
            "item_reference_not_found" => Some("item_reference_not_found"),
            "web_search_unavailable" => Some("web_search_unavailable"),
            "response_in_progress" => Some("response_in_progress"),
            _ => None,
        });
    let code = existing_code.unwrap_or(match status {
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_denied",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "response_in_progress",
        StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        StatusCode::UNPROCESSABLE_ENTITY => "unsupported_feature",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_exceeded",
        StatusCode::BAD_GATEWAY => "invalid_upstream_response",
        StatusCode::SERVICE_UNAVAILABLE => "provider_unavailable",
        _ => "internal_error",
    });
    let param = existing_code.and_then(|_| {
        decoded
            .as_ref()
            .and_then(|body| body.pointer("/error/param"))
            .and_then(Value::as_str)
            .and_then(|param| match param {
                "background" => Some("background"),
                "compact" => Some("compact"),
                "content_type" => Some("content_type"),
                "input" => Some("input"),
                "previous_response_id" => Some("previous_response_id"),
                _ => None,
            })
    });
    let mut normalized = protocol_error(status, code, param, message);
    for name in [
        header::RETRY_AFTER,
        header::HeaderName::from_static("x-request-id"),
    ] {
        if let Some(value) = parts.headers.get(&name) {
            normalized.headers_mut().insert(name, value.clone());
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upstream_failures_are_normalized_to_dated_error_envelopes() {
        let response = crate::error::GatewayError::upstream_status(
            "provider",
            502,
            Some("malformed response".into()),
        )
        .render(None);
        let response = normalize_error_response(response).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("error body");
        let body: Value = serde_json::from_slice(&body).expect("JSON error");
        assert_eq!(body["error"]["code"], "invalid_upstream_response");
        assert_eq!(body["error"]["type"], "invalid_upstream_response");
        assert!(body["error"]["message"].as_str().is_some());
    }
}

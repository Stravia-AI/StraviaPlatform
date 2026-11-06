//! Thin ingress shell: POST /v1/responses

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::Gateway;
use crate::proxy::context::RequestContext;
use crate::proxy::dispatcher::dispatch_pipeline;
use crate::proxy::ingress::observation;
use crate::proxy::security::{ClientCredential, Security};
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ir::RawEnvelope;

pub async fn handler(
    State(gw): State<Gateway>,
    mut ctx: axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    ctx.ingress_protocol = OPEN_RESPONSES_2026_04_24;
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => {
            let observer = observation::begin(
                &gw,
                &ctx,
                "POST",
                "/v1/responses",
                OPEN_RESPONSES_2026_04_24,
            );
            return observation::reject(
                observer,
                "decode",
                "invalid_json",
                json_rejection(rejection),
            );
        }
    };
    let observer = observation::begin(
        &gw,
        &ctx,
        "POST",
        "/v1/responses",
        OPEN_RESPONSES_2026_04_24,
    );
    if !has_unambiguous_bearer(&headers) {
        return observation::reject(
            observer,
            "authentication",
            "unauthorized",
            authentication_error(),
        );
    }
    if body.get("background").and_then(Value::as_bool) == Some(true) {
        return observation::reject(
            observer,
            "protocol",
            "unsupported_feature",
            protocol_error(
                StatusCode::BAD_REQUEST,
                "unsupported_feature",
                Some("background"),
                "Background responses are not supported.",
            ),
        );
    }
    let flat_headers: std::collections::HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|vs| (k.as_str().to_lowercase(), vs.to_string()))
        })
        .collect();
    let envelope = RawEnvelope::new(None, flat_headers, "POST", "/v1/responses");
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered ingress adapter");
    let request = match pair.decode_request(body) {
        Ok(request) => request,
        Err(_) => {
            return observation::reject(
                observer,
                "decode",
                "invalid_request",
                protocol_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    None,
                    "Request body is not a compatible Responses request.",
                ),
            );
        }
    };
    if crate::proxy::dispatcher::is_websocket_delivery_deferred(&ctx)
        && stravia_protocol_codec::codec::compaction::standalone_compaction_requested(&request)
    {
        return observation::reject(
            observer,
            "protocol",
            "unsupported_feature",
            protocol_error(
                StatusCode::BAD_REQUEST,
                "unsupported_feature",
                Some("input"),
                "Standalone compaction requires HTTP POST /v1/responses with stream: true; WebSocket response.create is not supported.",
            ),
        );
    }
    if let Err((param, message)) =
        classify_standalone_compaction(&mut ctx, &request, OPEN_RESPONSES_2026_04_24)
    {
        return observation::reject(
            observer,
            "protocol",
            "invalid_request",
            protocol_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                Some(param),
                message,
            ),
        );
    }
    let response = dispatch_pipeline(
        gw,
        observer,
        headers,
        envelope,
        request,
        OPEN_RESPONSES_2026_04_24,
        ctx.0,
    )
    .await;
    normalize_error_response(response).await
}

pub(crate) fn classify_standalone_compaction(
    ctx: &mut axum::extract::Extension<RequestContext>,
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
    ingress: stravia_runtime_contract::protocol::ids::ProtocolId,
) -> Result<(), (&'static str, String)> {
    if !stravia_protocol_codec::codec::compaction::standalone_compaction_requested(request) {
        return Ok(());
    }
    if ingress == OPEN_RESPONSES_2026_04_24 && !request.stream.enabled {
        return Err((
            "stream",
            "Standalone compaction requires stream: true.".into(),
        ));
    }
    let triggers = request
        .items
        .iter()
        .filter(|item| item.is_compaction_trigger())
        .count();
    if triggers != 1
        || !request
            .items
            .last()
            .is_some_and(stravia_runtime_contract::protocol::ir::AiItem::is_compaction_trigger)
    {
        return Err((
            "input",
            "Standalone compaction requires exactly one trailing compaction_trigger.".into(),
        ));
    }
    ctx.extensions
        .insert(crate::model_turn::ModelTurnPurpose::Compact);
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

pub(super) async fn authenticate(
    gateway: &Gateway,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    if !has_unambiguous_bearer(headers) {
        return Err(Box::new(authentication_error()));
    }
    let credential = ClientCredential::from_inference_headers(headers);
    match Security::new(gateway.storage.auth())
        .required_principal(&credential)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => Err(Box::new(normalize_error_response(error.render(None)).await)),
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
    let error_type = if matches!(code, "allowance_suspended" | "api_key_spend_limit_reached") {
        "insufficient_quota"
    } else if code == "unsupported_feature" {
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
    if status.is_success()
        || response
            .extensions()
            .get::<crate::model_turn::UpstreamErrorResponse>()
            .is_some()
    {
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
            "allowance_suspended" => Some("allowance_suspended"),
            "api_key_spend_limit_reached" => Some("api_key_spend_limit_reached"),
            "invalid_request" => Some("invalid_request"),
            "unsupported_feature" => Some("unsupported_feature"),
            "previous_response_not_found" => Some("previous_response_not_found"),
            "item_reference_not_found" => Some("item_reference_not_found"),
            "item_reference_ambiguous" => Some("item_reference_ambiguous"),
            "web_search_unavailable" => Some("web_search_unavailable"),
            "response_in_progress" => Some("response_in_progress"),
            "compaction_unsupported" => Some("compaction_unsupported"),
            "compaction_target_mismatch" => Some("compaction_target_mismatch"),
            "compaction_conflict" => Some("compaction_conflict"),
            "compaction_unavailable" => Some("compaction_unavailable"),
            "compaction_storage_failed" => Some("compaction_storage_failed"),
            "invalid_compaction_state" => Some("invalid_compaction_state"),
            "invalid_compaction_response" => Some("invalid_compaction_response"),
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
                "content_type" => Some("content_type"),
                "input" => Some("input"),
                "previous_response_id" => Some("previous_response_id"),
                "stream" => Some("stream"),
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

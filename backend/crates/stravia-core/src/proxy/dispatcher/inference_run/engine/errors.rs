use super::*;

pub(super) fn render_completion_failure(
    failure: CompletionFailure,
    ingress: ProtocolId,
    is_stream: bool,
) -> Response {
    match failure {
        CompletionFailure::Control(control) => render_hook_control(*control, ingress, is_stream),
        CompletionFailure::Hook(message) | CompletionFailure::AfterCommit(message) => {
            hook_failure_response(message)
        }
    }
}

pub(super) fn render_hook_control(
    control: crate::hook::HookControl,
    ingress: ProtocolId,
    is_stream: bool,
) -> Response {
    match control {
        crate::hook::HookControl::Continue => error_response(500, "invalid hook control state"),
        crate::hook::HookControl::Respond(response) => {
            let mut delivery = if is_stream {
                DeliveryAdapter::buffered_stream(ingress, ingress)
            } else {
                DeliveryAdapter::non_stream(ingress, ingress)
            };
            delivery
                .deliver_canonical(&response, StatusCode::OK)
                .response
        }
        crate::hook::HookControl::Reject(rejection) => {
            let status =
                StatusCode::from_u16(rejection.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (
                status,
                axum::Json(serde_json::json!({
                    "error": {
                        "code": rejection.code,
                        "message": rejection.message,
                    }
                })),
            )
                .into_response()
        }
        crate::hook::HookControl::StreamAbort { message } => error_response(500, &message),
    }
}

pub(super) fn coded_error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": {
                "code": code,
                "message": message,
            }
        })),
    )
        .into_response()
}

pub(super) fn parameter_error_response(
    status: StatusCode,
    code: &str,
    param: &str,
    message: &str,
) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": {
                "type": "invalid_request",
                "code": code,
                "param": param,
                "message": message,
            }
        })),
    )
        .into_response()
}

pub(super) fn inference_access_error_response(error: GatewayError) -> Response {
    match error {
        GatewayError::Unauthorized {
            reason: AuthFailure::Expired,
        } => error.render(None),
        GatewayError::Unauthorized { .. } => error_response(401, "invalid api key"),
        GatewayError::Forbidden {
            reason: AccessDenial::ModelNotAllowed,
        } => error_response(403, "api key not allowed for this model"),
        _ => error.render(None),
    }
}

pub(crate) fn error_response(status: u16, message: &str) -> Response {
    let err: GatewayError = match status {
        400 => GatewayError::bad_request("bad_request", message),
        401 => GatewayError::Unauthorized {
            reason: AuthFailure::Invalid,
        },
        403 => GatewayError::Forbidden {
            reason: crate::error::AccessDenial::Custom(message.to_string()),
        },
        404 => GatewayError::ModelNotFound {
            model: message.to_string(),
        },
        429 => GatewayError::upstream_status("unknown", 429, Some(message.to_string())),
        503 => GatewayError::provider_unavailable("unknown", message),
        502 => GatewayError::upstream_status("unknown", 502, Some(message.to_string())),
        499 => GatewayError::ClientCancelled,
        _ => GatewayError::Internal {
            source: anyhow::anyhow!("{}", message),
        },
    };
    err.render(None)
}

pub(crate) fn hook_failure_response(error: impl std::fmt::Display) -> Response {
    tracing::error!(error = %error, "inference hook failed");
    error_response(500, "hook_failed")
}

pub(super) fn model_turn_error_outcome(error: crate::agent::ModelTurnError) -> RoundOutcome {
    let status = match error.code.as_str() {
        "cancelled" => StatusCode::from_u16(499).expect("valid cancellation status"),
        "deadline_exceeded" => StatusCode::GATEWAY_TIMEOUT,
        "model_not_found" | "STRAVIA_NOT_FOUND" => StatusCode::NOT_FOUND,
        "model_unavailable" | "provider_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "tools_unsupported"
        | "web_search_unsupported"
        | "input_modality_unsupported"
        | "thinking_level_unsupported"
        | "protected_context_unrepresentable" => StatusCode::BAD_REQUEST,
        "protocol_lossy_rejected" | "STRAVIA_PROTOCOL_LOSSY_REJECTED" => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        "api_key_model_forbidden" | "capability_forbidden" | "STRAVIA_FORBIDDEN" => {
            StatusCode::FORBIDDEN
        }
        "STRAVIA_AUTH_ERROR" => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_GATEWAY,
    };
    let response = if error.code.starts_with("STRAVIA_") {
        let message = match error.code.as_str() {
            "STRAVIA_FORBIDDEN" if error.message == "access to this model is not permitted" => {
                "api key not allowed for this model".to_owned()
            }
            _ => error.message,
        };
        (
            status,
            axum::Json(serde_json::json!({
                "error": {
                    "type": error.code,
                    "code": status.as_u16(),
                    "message": message,
                }
            })),
        )
            .into_response()
    } else {
        coded_error_response(status, &error.code, &error.message)
    };
    buffered_response(response)
}

pub(super) fn model_turn_execute_failure(
    gateway: &Gateway,
    request: &AiRequest,
    ingress: ProtocolId,
    start: Instant,
    request_extras: &RequestExtras,
    auth_subject: Option<&crate::proxy::context::AuthSubject>,
    error: crate::agent::ModelTurnError,
) -> RoundOutcome {
    let outcome = model_turn_error_outcome(error);
    let status = match &outcome {
        RoundOutcome::Deliver { response, .. } => response.status().as_u16(),
        RoundOutcome::NextRound { .. } => 500,
    };
    LogBuilder::from_dispatch(
        gateway,
        &ingress.to_string(),
        &request.model,
        request.reasoning.level,
        auth_subject,
        start,
    )
    .stream_flag(request.stream.enabled)
    .status(status)
    .with_req_extras(request_extras)
    .emit();
    outcome
}

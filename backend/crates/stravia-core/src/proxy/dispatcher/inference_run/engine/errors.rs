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
    control: stravia_runtime_contract::hook::HookControl,
    ingress: ProtocolId,
    is_stream: bool,
) -> Response {
    match control {
        stravia_runtime_contract::hook::HookControl::Continue => {
            error_response(500, "invalid hook control state")
        }
        stravia_runtime_contract::hook::HookControl::Respond(response) => {
            let mut delivery = if is_stream {
                DeliveryAdapter::buffered_stream(ingress, ingress)
            } else {
                DeliveryAdapter::non_stream(ingress, ingress)
            };
            delivery
                .deliver_canonical(&response, StatusCode::OK)
                .response
        }
        stravia_runtime_contract::hook::HookControl::Reject(rejection) => {
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
        stravia_runtime_contract::hook::HookControl::StreamAbort { message } => {
            error_response(500, &message)
        }
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

pub(super) fn model_turn_error_outcome(
    error: stravia_runtime_contract::model_turn::ModelTurnError,
) -> RoundOutcome {
    buffered_response(model_turn_error_response(error))
}

pub(super) fn compaction_stream_error_outcome(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
    error: &stravia_runtime_contract::protocol::ir::AiError,
) -> Option<RoundOutcome> {
    if !crate::compaction::NativeCompactionControls::classify(request).requested() {
        return None;
    }
    let raw = error.raw.as_ref()?;
    let upstream = raw
        .pointer("/response/error")
        .or_else(|| raw.get("error"))?;
    let mut failure = stravia_runtime_contract::model_turn::ModelTurnError::new(
        "upstream_stream_error",
        error.message.clone(),
    );
    failure.upstream_status = error.status_code.filter(|status| *status >= 400);
    failure.upstream_body = Some(serde_json::json!({ "error": upstream }));
    Some(model_turn_error_outcome(failure))
}

pub(super) fn model_turn_error_response(
    error: stravia_runtime_contract::model_turn::ModelTurnError,
) -> Response {
    if let Some(body) = error.upstream_body {
        let status = error
            .upstream_status
            .and_then(|status| StatusCode::from_u16(status).ok())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let mut response = (status, axum::Json(body)).into_response();
        response
            .extensions_mut()
            .insert(crate::model_turn::UpstreamErrorResponse);
        return response;
    }
    if let Some(status) = error
        .upstream_status
        .and_then(|status| StatusCode::from_u16(status).ok())
    {
        let mut response = coded_error_response(status, &error.code, &error.message);
        response
            .extensions_mut()
            .insert(crate::model_turn::UpstreamErrorResponse);
        return response;
    }
    let status = match error.code.as_str() {
        "cancelled" => StatusCode::from_u16(499).expect("valid cancellation status"),
        "deadline_exceeded" => StatusCode::GATEWAY_TIMEOUT,
        "model_not_found" | "STRAVIA_NOT_FOUND" => StatusCode::NOT_FOUND,
        "model_unavailable" | "provider_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "tools_unsupported"
        | "web_search_unsupported"
        | "input_modality_unsupported"
        | "thinking_level_unsupported"
        | "compaction_unsupported"
        | "compaction_target_mismatch"
        | "invalid_compaction_state"
        | "compaction_conflict"
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
    response
}

pub(super) fn model_turn_execute_failure(
    error: stravia_runtime_contract::model_turn::ModelTurnError,
) -> RoundOutcome {
    model_turn_error_outcome(error)
}

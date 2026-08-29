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

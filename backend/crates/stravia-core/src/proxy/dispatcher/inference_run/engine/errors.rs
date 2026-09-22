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
                DeliveryAdapter::buffered_stream(ingress, Some(ingress))
            } else {
                DeliveryAdapter::non_stream(ingress, Some(ingress))
            };
            delivery
                .deliver_canonical(&response, StatusCode::OK)
                .response
        }
        stravia_runtime_contract::hook::HookControl::Reject(rejection) => {
            let status =
                StatusCode::from_u16(rejection.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            coded_error_response(status, &rejection.code, &rejection.message)
        }
        stravia_runtime_contract::hook::HookControl::StreamAbort { message } => {
            error_response(500, &message)
        }
    }
}

pub(super) fn coded_error_response(status: StatusCode, code: &str, message: &str) -> Response {
    coded_error_response_with_diagnostic(status, code, message, message)
}

fn coded_error_response_with_diagnostic(
    status: StatusCode,
    code: &str,
    public_message: &str,
    diagnostic_message: &str,
) -> Response {
    let mut response = (
        status,
        axum::Json(serde_json::json!({
            "error": {
                "code": code,
                "message": public_message,
            }
        })),
    )
        .into_response();
    response
        .extensions_mut()
        .insert(crate::interaction_observation::FailureDiagnostic::platform(
            code,
            diagnostic_message,
            status.as_u16(),
        ));
    response
}

pub(super) fn attachment_ingest_error_response(
    error: stravia_runtime_contract::artifact::ArtifactError,
) -> Response {
    let mapping = crate::agent::artifact::artifact_error_mapping(&error);
    coded_error_response_with_diagnostic(
        StatusCode::from_u16(mapping.status).expect("Artifact error has a valid status"),
        "attachment_ingest_failed",
        &mapping.public_message,
        &mapping.diagnostic_message,
    )
}

pub(super) fn parameter_error_response(
    status: StatusCode,
    code: &str,
    param: &str,
    message: &str,
) -> Response {
    let mut response = (
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
        .into_response();
    response
        .extensions_mut()
        .insert(crate::interaction_observation::FailureDiagnostic::platform(
            code,
            message,
            status.as_u16(),
        ));
    response
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
    let mut response = error_response(500, "hook_failed");
    response
        .extensions_mut()
        .insert(crate::interaction_observation::FailureDiagnostic::platform(
            "hook_failed",
            error.to_string(),
            500,
        ));
    response
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
    if !stravia_protocol_codec::codec::compaction::native_compaction_requested(request) {
        return None;
    }
    let raw = error.raw.as_ref()?;
    let upstream = raw
        .pointer("/response/error")
        .or_else(|| raw.get("error"))?;
    let mut body = serde_json::json!({ "error": upstream });
    crate::interaction_observation::redact_value(&mut body);
    let mut failure = stravia_runtime_contract::model_turn::ModelTurnError::new(
        "upstream_stream_error",
        error.message.clone(),
    );
    failure.upstream_status = error.status_code.filter(|status| *status >= 400);
    failure.upstream_body = Some(Box::new(body));
    Some(model_turn_error_outcome(failure))
}

pub(super) fn model_turn_error_status(
    error: &stravia_runtime_contract::model_turn::ModelTurnError,
) -> StatusCode {
    if error.upstream_body.is_some() {
        return error
            .upstream_status
            .and_then(|status| StatusCode::from_u16(status).ok())
            .unwrap_or(StatusCode::BAD_GATEWAY);
    }
    if let Some(status) = error
        .upstream_status
        .and_then(|status| StatusCode::from_u16(status).ok())
    {
        return status;
    }
    match error.code.as_str() {
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
        | "vendor_request_invalid"
        | "compaction_conflict" => StatusCode::BAD_REQUEST,
        "protocol_lossy_rejected" | "STRAVIA_PROTOCOL_LOSSY_REJECTED" => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        "api_key_model_forbidden" | "capability_forbidden" | "STRAVIA_FORBIDDEN" => {
            StatusCode::FORBIDDEN
        }
        "STRAVIA_AUTH_ERROR" => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_GATEWAY,
    }
}

pub(super) fn model_turn_error_response(
    error: stravia_runtime_contract::model_turn::ModelTurnError,
) -> Response {
    let status = model_turn_error_status(&error);
    if let Some(body) = error.upstream_body {
        let mut response = (status, axum::Json(body)).into_response();
        response
            .extensions_mut()
            .insert(crate::model_turn::UpstreamErrorResponse);
        return response;
    }
    if error.code == "attachment_ingest_failed" {
        let public_message = if status.is_server_error() {
            crate::agent::artifact::ARTIFACT_STORAGE_FAILURE_MESSAGE
        } else {
            &error.message
        };
        return coded_error_response_with_diagnostic(
            status,
            &error.code,
            public_message,
            &error.message,
        );
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
    if error.code.starts_with("STRAVIA_") {
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
    }
}

pub(super) fn model_turn_execute_failure(
    error: stravia_runtime_contract::model_turn::ModelTurnError,
) -> RoundOutcome {
    model_turn_error_outcome(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attachment_model_turn_storage_error_is_platform_500_without_internal_detail() {
        let secret = "attachment-model-turn-secret";
        let mut error = stravia_runtime_contract::model_turn::ModelTurnError::new(
            "attachment_ingest_failed",
            format!("Artifact storage failed: database_url=sqlite:///{secret}"),
        );
        error.upstream_status = Some(500);

        let response = model_turn_error_response(error);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            response
                .extensions()
                .get::<crate::model_turn::UpstreamErrorResponse>()
                .is_none()
        );
        let diagnostic = response
            .extensions()
            .get::<crate::interaction_observation::FailureDiagnostic>()
            .expect("platform diagnostic");
        assert_eq!(diagnostic.source.as_deref(), Some("platform"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("attachment error body");
        assert!(!String::from_utf8_lossy(&body).contains(secret));
    }
}

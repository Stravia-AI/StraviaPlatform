use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use stravia_core::Gateway;
use stravia_core::admin::provider_allowance::ProviderAllowanceGuardError;

#[derive(Deserialize)]
pub(super) struct ReplaceProviderAllowanceGuards {
    keys: Vec<String>,
}

pub(super) async fn replace_provider_allowance_guards(
    State(gateway): State<Gateway>,
    Path(provider_id): Path<String>,
    Json(input): Json<ReplaceProviderAllowanceGuards>,
) -> impl IntoResponse {
    match gateway
        .admin()
        .replace_provider_allowance_guards(&provider_id, input.keys)
        .await
    {
        Ok(Some(snapshot)) => Json(serde_json::json!({ "data": snapshot })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "provider not found",
                "code": "PROVIDER_NOT_FOUND",
            })),
        )
            .into_response(),
        Err(error)
            if error
                .downcast_ref::<ProviderAllowanceGuardError>()
                .is_some() =>
        {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": error.to_string(),
                    "code": "PROVIDER_ALLOWANCE_GUARDS_INVALID",
                })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(provider_id, %error, "Failed to replace provider allowance guards");
            internal_error(
                "PROVIDER_ALLOWANCE_GUARDS_SAVE_FAILED",
                "failed to save provider allowance guards",
            )
        }
    }
}

pub(super) async fn list_provider_allowances(State(gateway): State<Gateway>) -> impl IntoResponse {
    match gateway.admin().list_provider_allowance_targets().await {
        Ok(targets) => Json(serde_json::json!({ "data": targets })).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "Failed to list provider allowances");
            internal_error(
                "PROVIDER_ALLOWANCE_LOAD_FAILED",
                "failed to load provider allowances",
            )
        }
    }
}

pub(super) async fn get_provider_allowance(
    State(gateway): State<Gateway>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    match gateway.admin().get_provider_allowance(&provider_id).await {
        Ok(Some(snapshot)) => Json(serde_json::json!({ "data": snapshot })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "provider allowance is unavailable",
                "code": "PROVIDER_ALLOWANCE_UNAVAILABLE",
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(
                provider_id,
                error = %error,
                "Failed to load provider allowance"
            );
            internal_error(
                "PROVIDER_ALLOWANCE_LOAD_FAILED",
                "failed to load provider allowance",
            )
        }
    }
}

pub(super) async fn refresh_provider_allowance(
    State(gateway): State<Gateway>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    match gateway
        .admin()
        .refresh_provider_allowance(&provider_id)
        .await
    {
        Ok(Some(snapshot)) => Json(serde_json::json!({ "data": snapshot })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "provider allowance is unavailable",
                "code": "PROVIDER_ALLOWANCE_UNAVAILABLE",
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(
                provider_id,
                error = %error,
                "Failed to refresh provider allowance"
            );
            internal_error(
                "PROVIDER_ALLOWANCE_REFRESH_FAILED",
                "failed to refresh provider allowance",
            )
        }
    }
}

fn internal_error(code: &'static str, message: &'static str) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": message,
            "code": code,
        })),
    )
        .into_response()
}

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use stravia_core::Gateway;

pub(super) async fn list_provider_allowances(State(gateway): State<Gateway>) -> impl IntoResponse {
    match gateway.admin().list_provider_allowances().await {
        Ok(snapshots) => Json(serde_json::json!({ "data": snapshots })).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "Failed to list provider allowances");
            internal_error(
                "PROVIDER_ALLOWANCE_LOAD_FAILED",
                "failed to load provider allowances",
            )
        }
    }
}

pub(super) async fn refresh_provider_allowances(
    State(gateway): State<Gateway>,
) -> impl IntoResponse {
    match gateway.admin().refresh_provider_allowances().await {
        Ok(snapshots) => Json(serde_json::json!({ "data": snapshots })).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "Failed to refresh provider allowances");
            internal_error(
                "PROVIDER_ALLOWANCE_REFRESH_FAILED",
                "failed to refresh provider allowances",
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

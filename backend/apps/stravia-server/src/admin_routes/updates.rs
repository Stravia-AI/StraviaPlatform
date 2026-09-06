use axum::http::StatusCode;
use stravia_core::admin::updates::UpdateCheckMode;

use super::*;

pub(super) async fn get_updates(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().get_update_status().await {
        Ok(status) => Json(serde_json::json!({ "data": status })).into_response(),
        Err(error) => update_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Deserialize)]
pub(super) struct CheckUpdatesBody {
    mode: UpdateCheckMode,
}

pub(super) async fn check_updates(
    State(gw): State<Gateway>,
    Json(body): Json<CheckUpdatesBody>,
) -> impl IntoResponse {
    match gw.admin().check_for_updates(body.mode).await {
        Ok(status) => Json(serde_json::json!({ "data": status })).into_response(),
        Err(error) => update_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Deserialize)]
pub(super) struct SkippedVersionBody {
    version: Option<String>,
}

pub(super) async fn set_skipped_update_version(
    State(gw): State<Gateway>,
    Json(body): Json<SkippedVersionBody>,
) -> impl IntoResponse {
    match gw
        .admin()
        .set_skipped_update_version(body.version.as_deref())
        .await
    {
        Ok(status) => Json(serde_json::json!({ "data": status })).into_response(),
        Err(error) => update_error(StatusCode::BAD_REQUEST, error),
    }
}

fn update_error(status: StatusCode, error: anyhow::Error) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "code": "UPDATE_STATE_UNAVAILABLE",
            "error": error.to_string(),
        })),
    )
        .into_response()
}

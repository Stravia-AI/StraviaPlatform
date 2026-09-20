use super::*;
use stravia_core::media_generation::{GenerationError, MediaGenerationConfig};

pub(super) async fn get_media_generation_config(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().get_media_generation_config().await {
        Ok(value) => Json(serde_json::json!({"data":value})).into_response(),
        Err(error) => generation_error(error),
    }
}

pub(super) async fn update_media_generation_config(
    State(gw): State<Gateway>,
    Json(input): Json<MediaGenerationConfig>,
) -> impl IntoResponse {
    match gw.admin().update_media_generation_config(input).await {
        Ok(value) => Json(serde_json::json!({"data":value})).into_response(),
        Err(error) => generation_error(error),
    }
}

pub(super) async fn eligible_media_generation_routes(
    State(gw): State<Gateway>,
) -> impl IntoResponse {
    match gw.admin().list_eligible_media_generation_routes().await {
        Ok(value) => Json(serde_json::json!({"data":value})).into_response(),
        Err(error) => generation_error(error),
    }
}

fn generation_error(error: GenerationError) -> axum::response::Response {
    let status = if matches!(
        error.code,
        "media_generation_unavailable" | "media_generation_config_invalid"
    ) {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(serde_json::json!({"code":error.code,"error":error.message})),
    )
        .into_response()
}

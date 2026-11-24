use super::*;
use stravia_core::platform_llm::{
    ConversationTitleInput, PlatformLlmError, UpdatePlatformLlmConfig,
};

pub(super) async fn get_platform_llm_config(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().get_platform_llm_config().await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => platform_llm_error(error),
    }
}

pub(super) async fn update_platform_llm_config(
    State(gw): State<Gateway>,
    Json(input): Json<UpdatePlatformLlmConfig>,
) -> impl IntoResponse {
    match gw.admin().update_platform_llm_config(input).await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => platform_llm_error(error),
    }
}

pub(super) async fn generate_conversation_title(
    State(gw): State<Gateway>,
    Json(input): Json<ConversationTitleInput>,
) -> impl IntoResponse {
    match gw.admin().generate_conversation_title(input).await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => platform_llm_error(error),
    }
}

fn platform_llm_error(error: PlatformLlmError) -> axum::response::Response {
    let status = match error.code {
        "platform_llm_route_invalid" | "platform_llm_input_invalid" => StatusCode::BAD_REQUEST,
        "platform_llm_unauthorized" => StatusCode::UNAUTHORIZED,
        "platform_llm_not_configured" | "platform_llm_route_unavailable" => StatusCode::CONFLICT,
        "platform_llm_execution_failed" => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "code": error.code, "error": error.message })),
    )
        .into_response()
}

use super::*;

// ── Models ──

#[derive(serde::Deserialize)]
pub(super) struct ResetThinkingMappingInput {
    level: stravia_core::thinking::ThinkingLevel,
}

pub(super) async fn list_models_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_models().await {
        Ok(models) => Json(serde_json::json!({ "data": models })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn create_model_handler(
    State(gw): State<Gateway>,
    Json(input): Json<CreateModel>,
) -> impl IntoResponse {
    match gw.admin().create_model(input).await {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn bind_route_handler(
    State(gw): State<Gateway>,
    Json(input): Json<BindRouteInput>,
) -> impl IntoResponse {
    match gw.admin().bind_route(input).await {
        Ok(route) => Json(serde_json::json!({ "data": route })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn unbind_route_handler(
    State(gw): State<Gateway>,
    Json(input): Json<UnbindRouteInput>,
) -> impl IntoResponse {
    match gw.admin().unbind_route(input).await {
        Ok(route) => Json(serde_json::json!({ "data": route })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn update_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateModel>,
) -> impl IntoResponse {
    match gw.admin().update_model(&id, input).await {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn delete_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().delete_model(&id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn reset_target_thinking_mapping_handler(
    State(gw): State<Gateway>,
    Path((route_id, target_id)): Path<(String, String)>,
    Json(input): Json<ResetThinkingMappingInput>,
) -> impl IntoResponse {
    match gw
        .admin()
        .reset_target_thinking_mapping(&route_id, &target_id, input.level)
        .await
    {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn regenerate_target_thinking_map_handler(
    State(gw): State<Gateway>,
    Path((route_id, target_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match gw
        .admin()
        .regenerate_target_thinking_map(&route_id, &target_id)
        .await
    {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => err(error),
    }
}

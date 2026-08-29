use super::*;

// ── API Keys ──

pub(super) async fn list_api_keys_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_api_keys().await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn get_api_key_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_api_key(&id).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn create_api_key_handler(
    State(gw): State<Gateway>,
    Json(input): Json<CreateApiKey>,
) -> impl IntoResponse {
    match gw.admin().create_api_key(input).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn update_api_key_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateApiKey>,
) -> impl IntoResponse {
    match gw.admin().update_api_key(&id, input).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn delete_api_key_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().delete_api_key(&id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

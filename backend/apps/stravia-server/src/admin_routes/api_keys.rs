use super::*;

// ── API Keys ──

pub(super) async fn list_api_keys_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_api_keys().await {
        Ok(v) => {
            let admin = gw.admin();
            let custom = match admin.custom_currency().await {
                Ok(custom) => custom,
                Err(e) => return err(e),
            };
            let mut items = Vec::with_capacity(v.len());
            for key in &v {
                match admin.api_key_json_with_currency(key, custom.as_ref()) {
                    Ok(value) => items.push(value),
                    Err(e) => return err(e),
                }
            }
            Json(serde_json::json!({ "data": items })).into_response()
        }
        Err(e) => err(e),
    }
}

pub(super) async fn get_api_key_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_api_key(&id).await {
        Ok(v) => match gw.admin().api_key_json(&v).await {
            Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
            Err(e) => err(e),
        },
        Err(e) => err(e),
    }
}

pub(super) async fn create_api_key_handler(
    State(gw): State<Gateway>,
    Json(input): Json<CreateApiKey>,
) -> impl IntoResponse {
    match gw.admin().create_api_key(input).await {
        Ok(v) => match gw.admin().api_key_json(&v).await {
            Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
            Err(e) => err(e),
        },
        Err(e) => err(e),
    }
}

pub(super) async fn update_api_key_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateApiKey>,
) -> impl IntoResponse {
    match gw.admin().update_api_key(&id, input).await {
        Ok(v) => match gw.admin().api_key_json(&v).await {
            Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
            Err(e) => err(e),
        },
        Err(e) => err(e),
    }
}

pub(super) async fn reset_api_key_spend_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().reset_api_key_spend(&id).await {
        Ok(v) => match gw.admin().api_key_spend_json(&v).await {
            Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
            Err(e) => err(e),
        },
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

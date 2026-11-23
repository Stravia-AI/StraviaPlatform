use super::*;

// ── Media Understanding ──

pub(super) async fn get_media_understanding_handler(
    State(gw): State<Gateway>,
) -> impl IntoResponse {
    match gw.admin().get_media_understanding_config().await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => media_understanding_config_error(error),
    }
}

pub(super) async fn update_media_understanding_handler(
    State(gw): State<Gateway>,
    Path(kind): Path<stravia_media::MediaKind>,
    Json(input): Json<MediaUnderstandingConfigUpdate>,
) -> impl IntoResponse {
    match gw
        .admin()
        .update_media_understanding_config(kind, input)
        .await
    {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => media_understanding_config_error(error),
    }
}

pub(super) fn media_understanding_config_error(
    error: stravia_media::admin::MediaUnderstandingConfigError,
) -> axum::response::Response {
    let status = if error.code == "MEDIA_UNDERSTANDING_CONFIG_UNAVAILABLE" {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(serde_json::json!({
            "code": error.code,
            "error": error.message,
        })),
    )
        .into_response()
}

pub(super) async fn list_web_provider_kinds_handler(
    State(gw): State<Gateway>,
) -> impl IntoResponse {
    Json(serde_json::json!({ "data": gw.admin().list_web_provider_kinds() }))
}

pub(super) async fn list_web_providers_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_web_provider_views().await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn get_web_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_web_provider_view(&id).await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn create_web_provider_handler(
    State(gw): State<Gateway>,
    Json(input): Json<CreateWebProvider>,
) -> impl IntoResponse {
    match gw.admin().create_web_provider(input).await {
        Ok(value) => {
            let view = gw.admin().web_provider_view(value).await;
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "data": view })),
            )
                .into_response()
        }
        Err(error) => err(error),
    }
}

pub(super) async fn update_web_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateWebProvider>,
) -> impl IntoResponse {
    match gw.admin().update_web_provider(&id, input).await {
        Ok(value) => {
            let view = gw.admin().web_provider_view(value).await;
            Json(serde_json::json!({ "data": view })).into_response()
        }
        Err(error) => err(error),
    }
}

pub(super) async fn delete_web_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().delete_web_provider(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn test_web_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().test_web_provider(&id).await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn get_web_access_settings_handler(
    State(gw): State<Gateway>,
) -> impl IntoResponse {
    match gw.admin().get_web_access_settings().await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn update_web_access_settings_handler(
    State(gw): State<Gateway>,
    Json(input): Json<WebAccessSettings>,
) -> impl IntoResponse {
    match gw.admin().update_web_access_settings(input).await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => {
            let message = error.to_string();
            if let Ok(coded) = serde_json::from_str::<serde_json::Value>(&message)
                && let Some(code) = coded["code"].as_str()
            {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "code": code,
                        "error": coded["message"],
                        "params": coded["params"],
                    })),
                )
                    .into_response();
            }
            err(error)
        }
    }
}

pub(super) async fn list_eligible_web_search_models_handler(
    State(gw): State<Gateway>,
) -> impl IntoResponse {
    match gw.admin().list_eligible_web_search_models().await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => web_search_config_error(error),
    }
}

pub(super) async fn list_external_search_routes_handler(
    State(gw): State<Gateway>,
) -> impl IntoResponse {
    match gw.admin().list_external_search_routes().await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => web_search_config_error(error),
    }
}

pub(super) async fn get_web_search_config_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().get_web_search_config().await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => web_search_config_error(error),
    }
}

pub(super) async fn update_web_search_config_handler(
    State(gw): State<Gateway>,
    Json(input): Json<WebSearchConfig>,
) -> impl IntoResponse {
    match gw.admin().update_web_search_config(input).await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => web_search_config_error(error),
    }
}

pub(super) fn web_search_config_error(
    error: stravia_web_search::admin::WebSearchConfigError,
) -> axum::response::Response {
    let status = if error.code == "WEB_SEARCH_CONFIG_UNAVAILABLE" {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(serde_json::json!({
            "code": error.code,
            "error": error.message,
        })),
    )
        .into_response()
}

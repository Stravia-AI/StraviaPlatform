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
    Json(input): Json<MediaUnderstandingConfigUpdate>,
) -> impl IntoResponse {
    match gw.admin().update_media_understanding_config(input).await {
        Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
        Err(error) => media_understanding_config_error(error),
    }
}

pub(super) fn media_understanding_config_error(
    error: stravia_core::admin::MediaUnderstandingConfigError,
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

pub(super) async fn list_web_providers_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_web_providers().await {
        Ok(value) => Json(serde_json::json!({
            "data": value.into_iter().map(web_provider_value).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn get_web_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_web_provider(&id).await {
        Ok(value) => Json(serde_json::json!({ "data": web_provider_value(value) })).into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn create_web_provider_handler(
    State(gw): State<Gateway>,
    Json(input): Json<CreateWebProvider>,
) -> impl IntoResponse {
    match gw.admin().create_web_provider(input).await {
        Ok(value) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "data": web_provider_value(value) })),
        )
            .into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn update_web_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateWebProvider>,
) -> impl IntoResponse {
    match gw.admin().update_web_provider(&id, input).await {
        Ok(value) => Json(serde_json::json!({ "data": web_provider_value(value) })).into_response(),
        Err(error) => err(error),
    }
}

fn web_provider_value(provider: WebProvider) -> serde_json::Value {
    let capabilities = provider.capabilities();
    let local_engines = provider.local_engine_views();
    let mut value = serde_json::to_value(provider).expect("Web Provider serializes");
    let object = value
        .as_object_mut()
        .expect("Web Provider serializes as object");
    object.insert(
        "capabilities".into(),
        serde_json::to_value(capabilities).expect("Web Provider capabilities serialize"),
    );
    if let Some(local_engines) = local_engines {
        object.insert(
            "local_engines".into(),
            serde_json::to_value(local_engines).expect("Local Search Engines serialize"),
        );
    }
    value
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
        Err(error) => err(error),
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

pub(super) async fn list_compatible_codex_search_providers_handler(
    State(gw): State<Gateway>,
) -> impl IntoResponse {
    match gw.admin().list_compatible_codex_search_providers().await {
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
    error: stravia_core::admin::WebSearchConfigError,
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

#[cfg(test)]
mod tests {
    use super::web_provider_value;
    use stravia_core::db::models::{WebProvider, default_local_search_engines};

    #[test]
    fn local_web_provider_payload_exposes_engine_state_without_secrets() {
        let mut engines = default_local_search_engines();
        engines
            .get_mut("google")
            .expect("Google config")
            .private_settings = Some(
            [("cookies".to_string(), "SID=private-session".to_string())]
                .into_iter()
                .collect(),
        );
        let value = web_provider_value(WebProvider {
            id: "web-provider-local".into(),
            name: "Local".into(),
            kind: "local".into(),
            api_key: None,
            use_proxy: false,
            local_engines: Some(engines.into()),
            last_test_success: None,
            last_test_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        });

        assert_eq!(value["local_engines"]["google"]["enabled"], true);
        let payload = value.to_string();
        assert!(!payload.contains("SID=private-session"));
        assert!(!payload.contains("private_settings"));
        assert!(value.get("api_key").is_none());
    }
}

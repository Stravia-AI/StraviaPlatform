use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use stravia_core::Gateway;
use stravia_core::admin::MediaUnderstandingConfigUpdate;
use stravia_core::admin::{BindRouteInput, CopyProviderOptions, UnbindRouteInput};
use stravia_core::auth::{AuthExchangeInput, AuthSessionStatusData, OAuthCallbackMode};
use stravia_core::provider_catalog::CatalogError;
use stravia_core::provider_models::{
    CreateManualProviderModel, ProviderModelSelectionPolicy, UpdateProviderModel,
    UpdateProviderModelSelection,
};
use stravia_core::web_search::WebSearchConfig;

use crate::oauth_callback::OAuthCallbackManager;
use stravia_core::db::models::*;

#[derive(Clone)]
struct AdminToken(String);

async fn admin_auth(
    token_ext: Option<Extension<AdminToken>>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    let Some(Extension(admin_token)) = token_ext else {
        return next.run(req).await;
    };

    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let token = auth_header.strip_prefix("Bearer ").unwrap_or(auth_header);

    if token == admin_token.0 {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid admin token"})),
        )
            .into_response()
    }
}

pub fn create_router(gateway: Gateway, admin_token: Option<String>) -> Router {
    let oauth_callbacks = OAuthCallbackManager::new(gateway.clone());
    let providers_item = get(get_provider_handler)
        .put(update_provider_handler)
        .delete(delete_provider_handler);
    let web_providers_item = get(get_web_provider_handler)
        .put(update_web_provider_handler)
        .delete(delete_web_provider_handler);

    let models_item = put(update_model_handler).delete(delete_model_handler);
    let api_keys_item = get(get_api_key_handler)
        .put(update_api_key_handler)
        .delete(delete_api_key_handler);

    let mut api = Router::new()
        .route("/system/extensions", get(list_loaded_extensions))
        .route("/vendors", get(list_vendor_metadata))
        .route(
            "/media-understanding",
            get(get_media_understanding_handler).put(update_media_understanding_handler),
        )
        .route("/catalog/providers", get(catalog_providers_handler))
        .route("/catalog/models", get(canonical_models_handler))
        .route(
            "/catalog/providers/{provider_id}/channels/{channel_id}/models",
            get(catalog_models_handler),
        )
        .route("/catalog/refresh", post(catalog_refresh_handler))
        // Tombstone the removed endpoint before `/providers/{id}` can capture
        // `presets` as a provider ID.
        .route(
            "/providers/presets",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        // Capability drift persistence was removed. Keep this former list path
        // from being interpreted as a provider ID while older UIs phase out.
        .route(
            "/providers/image-capability-drifts",
            get(|| async { Json(serde_json::json!({ "data": [] })) }),
        )
        .route(
            "/providers",
            get(list_providers).post(create_provider_handler),
        )
        .route(
            "/providers/base-url/preview",
            post(preview_provider_base_url_handler),
        )
        .route("/providers/{id}/copy", post(copy_provider_handler))
        .route("/providers/{id}", providers_item)
        .route("/providers/{id}/test", get(test_provider_handler))
        .route(
            "/web-providers",
            get(list_web_providers_handler).post(create_web_provider_handler),
        )
        .route("/web-providers/{id}", web_providers_item)
        .route("/web-providers/{id}/test", post(test_web_provider_handler))
        .route(
            "/web-access/settings",
            get(get_web_access_settings_handler).put(update_web_access_settings_handler),
        )
        .route(
            "/web-search/config",
            get(get_web_search_config_handler).put(update_web_search_config_handler),
        )
        .route(
            "/web-search/eligible-models",
            get(list_eligible_web_search_models_handler),
        )
        .route(
            "/web-search/codex-providers",
            get(list_compatible_codex_search_providers_handler),
        )
        .route(
            "/providers/{id}/test-models",
            get(test_provider_models_handler),
        )
        .route(
            "/providers/{id}/models",
            get(provider_models_handler).post(create_provider_model_handler),
        )
        .route(
            "/providers/{id}/models/sync",
            post(sync_provider_models_handler),
        )
        .route(
            "/providers/{id}/model",
            get(get_provider_model_handler)
                .put(update_provider_model_handler)
                .delete(delete_provider_model_handler),
        )
        .route(
            "/providers/{id}/model/prepare",
            post(prepare_provider_model_handler),
        )
        .route(
            "/providers/{id}/model/selection",
            put(update_provider_model_selection_handler),
        )
        .route(
            "/providers/{id}/model/reimport",
            post(reimport_provider_model_handler),
        )
        .route(
            "/providers/{id}/model-capabilities",
            get(provider_model_capabilities_handler),
        )
        .route(
            "/providers/{id}/oauth/status",
            get(get_provider_oauth_status_handler),
        )
        .route(
            "/providers/{id}/oauth/reconnect",
            post(reconnect_provider_oauth_handler),
        )
        .route(
            "/providers/{id}/oauth/logout",
            post(logout_provider_oauth_handler),
        )
        .route(
            "/providers/{id}/oauth/bind",
            post(bind_provider_oauth_handler),
        )
        .route("/providers/oauth", post(create_oauth_provider_handler))
        .route("/oauth/sessions/init", post(init_oauth_session_handler))
        .route(
            "/oauth/sessions/{id}/status",
            get(get_oauth_session_status_handler),
        )
        .route(
            "/oauth/sessions/{id}/cancel",
            post(cancel_oauth_session_handler),
        )
        .route(
            "/oauth/sessions/{id}/complete",
            post(complete_oauth_session_handler),
        )
        .route(
            "/oauth/sessions/{id}/proxy",
            put(update_oauth_session_proxy_handler),
        )
        .route(
            "/models",
            get(list_models_handler).post(create_model_handler),
        )
        .route("/models/bind", post(bind_route_handler))
        .route("/models/unbind", post(unbind_route_handler))
        .route(
            "/models/{route_id}/targets/{target_id}/thinking-map/reset",
            post(reset_target_thinking_mapping_handler),
        )
        .route(
            "/models/{route_id}/targets/{target_id}/thinking-map/regenerate",
            post(regenerate_target_thinking_map_handler),
        )
        .route("/models/{id}", models_item.clone())
        // Deprecated: use /models instead
        .route(
            "/routes",
            get(list_models_handler).post(create_model_handler),
        )
        .route("/routes/{id}", models_item)
        .route(
            "/api-keys",
            get(list_api_keys_handler).post(create_api_key_handler),
        )
        .route("/api-keys/{id}", api_keys_item)
        .route("/logs", get(query_logs_handler).delete(clear_logs_handler))
        .route("/logs/{id}", get(get_log_handler))
        .route("/stats/overview", get(stats_overview))
        .route("/stats/hourly", get(stats_hourly))
        .route("/stats/models", get(stats_by_model))
        .route("/stats/providers", get(stats_by_provider))
        .route("/stats/api-keys", get(stats_by_api_key))
        .route("/settings/{key}", get(get_setting).put(set_setting))
        .route("/status", get(get_status))
        .layer(Extension(oauth_callbacks))
        .with_state(gateway.clone());
    // Catalog logos proxy only public Provider Catalog assets and remain unauthenticated
    // so browser image requests do not need to expose the admin bearer token.
    let public_api = Router::new()
        .route(
            "/catalog/providers/{provider_id}/logo",
            get(catalog_logo_handler),
        )
        .with_state(gateway.clone());

    if let Some(token) = admin_token
        && !token.is_empty()
    {
        api = api
            .layer(middleware::from_fn(admin_auth))
            .layer(Extension(AdminToken(token)));
    }

    // Health probes are unauthenticated so K8s/load-balancers can reach them
    // without an admin token.
    let health_routes = Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(gateway);

    Router::new()
        .merge(health_routes)
        .nest("/api/v1", public_api.merge(api))
}

async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, r#"{"status":"ok"}"#)
}

async fn readyz_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.storage.bootstrap().health().await {
        Ok(h) if h.can_connect && h.schema_compatible => (StatusCode::OK, r#"{"status":"ok"}"#),
        Ok(h) if h.can_connect => (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"schema_pending"}"#,
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"unavailable"}"#,
        ),
    }
}

mod api_keys;
mod logs;
mod models;
mod providers;
mod settings;
mod stats;
mod status;
mod web;

use api_keys::*;
use logs::*;
use models::*;
use providers::*;
use settings::*;
use stats::*;
use status::*;
use web::*;

#[cfg(test)]
mod tests;

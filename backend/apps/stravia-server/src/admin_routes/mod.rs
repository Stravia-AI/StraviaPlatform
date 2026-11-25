use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};

#[cfg(test)]
use crate::extensions::CeCapabilityProvider;
use crate::extensions::{CapabilitiesResponse, CapabilityProvider};
use crate::http_auth::{AdminHttpState, auth_router, require_admin};
use serde::Deserialize;
use stravia_core::Gateway;
use stravia_core::admin::{
    BindRouteInput, CopyProviderOptions, ProviderConnectionCheckInput, UnbindRouteInput,
};
use stravia_core::auth::{
    AuthCompletionInput, AuthSessionCandidate, AuthSessionStatusData, OAuthCallbackMode,
};
use stravia_core::provider_catalog::CatalogError;
use stravia_core::provider_models::{
    CreateManualProviderModel, ModelChangeQuery, ProviderModelSelectionPolicy,
    ProviderModelSyncFailure, RestoreProviderModelSync, UpdateProviderModel,
    UpdateProviderModelSelection, UpdateProviderModelSyncState,
};
use stravia_media::admin::MediaUnderstandingConfigUpdate;
use stravia_web_search::WebSearchConfig;

use crate::oauth_callback::OAuthCallbackManager;
use stravia_core::db::models::*;

mod connect_clients;
mod money;
mod performance;
mod platform_llm;
mod provider_allowances;
use connect_clients::preview_connect_client_handler;
use provider_allowances::{
    get_provider_allowance, list_provider_allowances, refresh_provider_allowance,
    replace_provider_allowance_guards, reset_provider_allowance,
};

#[cfg(test)]
pub(crate) fn create_unprotected_router(gateway: Gateway) -> Router {
    create_router_inner(gateway, None, Arc::new(CeCapabilityProvider), None, None)
}

pub(crate) fn create_router(
    gateway: Gateway,
    auth: AdminHttpState,
    capability_provider: Arc<dyn CapabilityProvider>,
    extra_admin_routes: Option<Router<Gateway>>,
    extra_public_routes: Option<Router>,
) -> Router {
    create_router_inner(
        gateway,
        Some(auth),
        capability_provider,
        extra_admin_routes,
        extra_public_routes,
    )
}

fn create_router_inner(
    gateway: Gateway,
    auth: Option<AdminHttpState>,
    capability_provider: Arc<dyn CapabilityProvider>,
    extra_admin_routes: Option<Router<Gateway>>,
    extra_public_routes: Option<Router>,
) -> Router {
    let oauth_callbacks = OAuthCallbackManager::new(gateway.clone());
    let providers_item = get(get_provider_handler)
        .put(update_provider_handler)
        .delete(delete_provider_handler);
    let web_providers_item = get(get_web_provider_handler)
        .put(update_web_provider_handler)
        .delete(delete_web_provider_handler);

    let models_item = get(get_model_handler)
        .put(update_model_handler)
        .delete(delete_model_handler);
    let api_keys_item = get(get_api_key_handler)
        .put(update_api_key_handler)
        .delete(delete_api_key_handler);

    let mut api = Router::new()
        .route("/system/extensions", get(list_loaded_extensions))
        .merge(vendor_plugins::routes())
        .route(
            "/connect-clients/preview",
            post(preview_connect_client_handler),
        )
        .route("/vendors", get(list_vendor_metadata))
        .route(
            "/media-generation/config",
            get(get_media_generation_config).put(update_media_generation_config),
        )
        .route(
            "/media-generation/eligible-routes",
            get(eligible_media_generation_routes),
        )
        .route("/media-understanding", get(get_media_understanding_handler))
        .route(
            "/media-understanding/{kind}",
            put(update_media_understanding_handler),
        )
        .route(
            "/platform-llm",
            get(platform_llm::get_platform_llm_config)
                .put(platform_llm::update_platform_llm_config),
        )
        .route(
            "/platform-llm/conversation-title",
            post(platform_llm::generate_conversation_title),
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
            "/providers/configuration-preview",
            post(preview_provider_configuration_handler),
        )
        .route("/providers/{id}/copy", post(copy_provider_handler))
        .route(
            "/providers/{id}/cost-plans",
            get(money::list_cost_plans).post(money::append_cost_plan),
        )
        .route("/providers/{id}", providers_item)
        .route("/providers/{id}/test", post(test_provider_handler))
        .route(
            "/currency",
            get(money::get_currency).put(money::put_currency),
        )
        .route("/web-provider-kinds", get(list_web_provider_kinds_handler))
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
            "/web-search/external-routes",
            get(list_external_search_routes_handler),
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
            "/providers/{id}/models/sync-state",
            get(provider_model_sync_state_handler).put(update_provider_model_sync_state_handler),
        )
        .route("/model-changes", get(list_model_changes_handler))
        .route("/model-changes/{id}", get(get_model_change_handler))
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
            "/providers/{id}/model/restore-sync",
            post(restore_provider_model_sync_handler),
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
            "/oauth/sessions/refresh-token",
            post(refresh_token_oauth_session_handler),
        )
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
            "/models/{route_id}/target-statuses",
            get(get_model_target_statuses_handler),
        )
        .route(
            "/models/{route_id}/targets/{target_id}/thinking-map/reset",
            post(reset_target_thinking_mapping_handler),
        )
        .route(
            "/models/{route_id}/targets/{target_id}/thinking-map/regenerate",
            post(regenerate_target_thinking_map_handler),
        )
        .route("/models/{route_id}", models_item.clone())
        // Deprecated: use /models instead
        .route(
            "/routes",
            get(list_models_handler).post(create_model_handler),
        )
        .route("/routes/{route_id}", models_item)
        .route(
            "/api-keys",
            get(list_api_keys_handler).post(create_api_key_handler),
        )
        .route(
            "/api-keys/{id}/spend/reset",
            post(reset_api_key_spend_handler),
        )
        .route("/api-keys/{id}", api_keys_item)
        .route("/reversible-redaction/rules", get(credential_rules))
        .route(
            "/reversible-redaction/disabled-rules",
            put(set_disabled_credential_rules),
        )
        .route("/reversible-redaction/test", post(test_credentials))
        .route(
            "/reversible-redaction/custom-rules",
            get(list_custom_credential_rules).post(create_custom_credential_rule),
        )
        .route(
            "/reversible-redaction/custom-rules/{id}",
            put(update_custom_credential_rule).delete(delete_custom_credential_rule),
        )
        .route(
            "/reversible-redaction/discoveries",
            get(credential_discoveries),
        )
        .route(
            "/reversible-redaction/discoveries/{id}/hits/{index}",
            get(credential_discovery_hit),
        )
        .route(
            "/reversible-redaction/discoveries/{id}/reuses",
            get(credential_discovery_reuses),
        )
        .route("/observations/interactions", get(interaction_forest))
        .route("/observations/contents", get(observation_contents))
        .route("/observations/interactions/{id}", get(interaction_detail))
        .route(
            "/observations/interactions/{id}/events",
            get(interaction_events),
        )
        .route(
            "/observations/interactions/changes",
            post(interaction_changes),
        )
        .route("/observations/rejections", get(rejection_list))
        .route("/observations/client-errors", get(client_error_list))
        .route(
            "/observations/client-errors/{kind}/{id}",
            get(client_error_detail),
        )
        .route("/observations/upstream-errors", get(upstream_error_list))
        .route(
            "/observations/upstream-errors/{id}",
            get(upstream_error_detail),
        )
        .route("/observations/rejections/{id}", get(rejection_detail))
        .route("/observations/events", get(observation_events))
        .route(
            "/observations/interactions/{id}/live",
            get(interaction_live),
        )
        .route(
            "/observations/debug",
            get(observation_debug)
                .put(update_observation_debug)
                .delete(clear_observation_debug),
        )
        .route(
            "/observations/history",
            axum::routing::delete(clear_observation_history),
        )
        .route(
            "/observations/interactions/{id}/debug-bundle-tickets",
            post(issue_interaction_bundle_ticket),
        )
        .route(
            "/observations/rejections/{id}/debug-bundle-tickets",
            post(issue_rejection_bundle_ticket),
        )
        .route("/stats/overview", get(stats_overview))
        .route("/stats/series", get(stats_series))
        .route("/stats/models", get(stats_by_model))
        .route("/stats/providers", get(stats_by_provider))
        .route("/stats/api-keys", get(stats_by_api_key))
        .route("/performance/snapshot", get(performance::metrics))
        .route("/performance/timeline", get(performance::timeline))
        .route("/performance/export", get(performance::export))
        .route("/provider-allowances", get(list_provider_allowances))
        .route(
            "/provider-allowances/{provider_id}",
            get(get_provider_allowance),
        )
        .route(
            "/provider-allowances/{provider_id}/refresh",
            post(refresh_provider_allowance),
        )
        .route(
            "/provider-allowances/{provider_id}/reset",
            post(reset_provider_allowance),
        )
        .route(
            "/provider-allowances/{provider_id}/guards",
            put(replace_provider_allowance_guards),
        )
        .route("/updates", get(get_updates))
        .route("/updates/check", post(check_updates))
        .route("/updates/channel", put(set_update_channel))
        .route("/updates/skipped-version", put(set_skipped_update_version))
        .route("/settings/{key}", get(get_setting).put(set_setting))
        .route("/status", get(get_status))
        .layer(Extension(oauth_callbacks));
    if let Some(extra_admin) = extra_admin_routes {
        api = api.merge(extra_admin);
    }
    let mut api = api.with_state(gateway.clone());
    let auth_mode = auth
        .as_ref()
        .map(|state| state.auth_provider.auth_mode())
        .unwrap_or("single_admin");
    let capabilities_route = Router::new().route(
        "/capabilities",
        get({
            let provider = Arc::clone(&capability_provider);
            move || {
                let provider = Arc::clone(&provider);
                async move {
                    let snapshot = provider.capabilities();
                    let mut capabilities = snapshot.capabilities;
                    capabilities.sort();
                    capabilities.dedup();
                    Json(CapabilitiesResponse {
                        edition: snapshot.edition,
                        license_tier: snapshot.license_tier,
                        license_status: snapshot.license_status,
                        capabilities,
                        degraded_reason: snapshot.degraded_reason,
                        auth_mode: auth_mode.to_string(),
                    })
                }
            }
        }),
    );
    api = api.merge(capabilities_route);
    if let Some(auth_state) = auth.clone() {
        api = api.layer(middleware::from_fn_with_state(auth_state, require_admin));
    }
    // Catalog logos are public assets. Debug bundle downloads are separately authorized
    // by a single-use opaque ticket so browser navigation never exposes Admin credentials.
    let mut public_api = Router::new()
        .route(
            "/catalog/providers/{provider_id}/logo",
            get(catalog_logo_handler),
        )
        .route(
            "/observations/debug-bundles/{ticket}",
            get(consume_bundle_ticket),
        )
        .with_state(gateway.clone());
    if let Some(extra_public) = extra_public_routes {
        public_api = public_api.merge(extra_public);
    }

    // Health probes are unauthenticated so K8s/load-balancers can reach them
    // without an admin token.
    let health_routes = Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(gateway);

    let mut router = Router::new()
        .merge(health_routes)
        .nest("/api/v1", public_api.merge(api));
    if let Some(auth) = auth {
        router = router.merge(auth_router(auth));
    }
    router
}

async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, r#"{"status":"ok"}"#)
}

/// `If-None-Match` 是否命中给定的（带引号的）ETag；目录与观测正文端点共用。
fn request_matches_etag(request_headers: &HeaderMap, etag: &str) -> bool {
    request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag))
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
mod credential_protection;
mod media_generation;
mod models;
mod observations;
mod providers;
mod settings;
mod stats;
mod status;
mod updates;
mod vendor_plugins;
mod web;

use api_keys::*;
use credential_protection::*;
use media_generation::*;
use models::*;
use observations::*;
use providers::*;
use settings::*;
use stats::*;
use status::*;
use updates::*;
use web::*;

#[cfg(test)]
mod tests;

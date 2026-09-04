//! Ingress handlers that remain in the legacy handler module.
//!
//! The full proxy pipeline lives in `proxy/dispatcher.rs`.
//! Old ingress handlers (`openai_proxy`, `anthropic_proxy`, etc.) have been
//! replaced by `proxy/ingress/*.rs` thin shells wired directly in `server.rs`.
//!
//! This file now contains only `models_list`, which is a read-only endpoint
//! that does not go through the proxy pipeline.

use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::Gateway;
use crate::proxy::security::{ClientCredential, Security};

// ── GET /v1/models ────────────────────────────────────────────────────────────

pub async fn models_list(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let credential = ClientCredential::from_inference_headers(&headers);
    let accessible_route_ids = match Security::new(gw.storage.auth())
        .visible_model_ids(&credential)
        .await
    {
        Ok(model_ids) => model_ids.into_iter().collect::<HashSet<_>>(),
        Err(error) => return error.render(None),
    };
    let unrestricted_model_access = accessible_route_ids.is_empty();

    let cache = gw.model_cache.read().await;
    let mut models = cache
        .models
        .iter()
        .filter(|model| unrestricted_model_access || accessible_route_ids.contains(&model.id))
        .filter(|model| !model.model_id.trim().is_empty())
        .map(|model| {
            (
                model.model_id.trim().to_string(),
                model.effective_display_name().to_string(),
                model.supported_thinking_levels.0.clone(),
            )
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));

    let data = models
        .into_iter()
        .map(|(model_id, display_name, thinking_levels)| {
            let mut value = serde_json::json!({
                "id": model_id,
                "display_name": display_name,
                "object": "model",
                "created": 0,
                "owned_by": "Stravia"
            });
            if !thinking_levels.is_empty() {
                value["stravia:thinking_levels"] = serde_json::json!(thinking_levels);
            }
            value
        })
        .collect::<Vec<_>>();

    Json(serde_json::json!({
        "object": "list",
        "data": data
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::{HeaderValue, header};

    use super::*;
    use crate::db::models::{
        CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput, ProviderSourceInput,
        Route,
    };
    use crate::provider_models::CreateManualProviderModel;

    async fn listed_model_ids(response: Response) -> Vec<String> {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("models response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("models response JSON");
        value["data"]
            .as_array()
            .expect("models data")
            .iter()
            .filter_map(|model| model["id"].as_str().map(ToOwned::to_owned))
            .collect()
    }

    async fn listed_models(response: Response) -> Vec<serde_json::Value> {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("models response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("models response JSON");
        value["data"].as_array().expect("models data").clone()
    }

    #[tokio::test]
    async fn models_list_requires_a_valid_bound_api_key() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let config = crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        };
        let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some("Models list provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: Some("custom".into()),
                    protocol: "openai-compatible".into(),
                    base_url: "http://127.0.0.1:9/v1".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "provider-secret".into(),
                },
                use_proxy: false,
            })
            .await
            .expect("provider");
        gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                "provider-model",
                CreateManualProviderModel {
                    metadata: serde_json::json!({
                        "id": "provider-model",
                        "name": "provider-model",
                    }),
                },
            )
            .await
            .expect("Provider Model");
        let create_model = |model_id: &str, display_name: Option<&str>| CreateRoute {
            model_id: model_id.into(),
            display_name: display_name.map(ToOwned::to_owned),
            balance: Some("traffic_equalization".into()),
            target_provider: String::new(),
            target_model: String::new(),
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "provider-model".into(),
                enabled: true,
                priority: Some(1),
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
        };
        let unbound = gateway
            .admin()
            .create_model(create_model("unbound-model", Some("Shared label")))
            .await
            .expect("unbound model");
        let bound = gateway
            .admin()
            .create_model(create_model("bound-model", Some("Shared label")))
            .await
            .expect("bound model");
        gateway
            .admin()
            .create_model(create_model("unnamed-model", None))
            .await
            .expect("unnamed model");
        let key = gateway
            .admin()
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: "Models list key".into(),
                concurrency_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                inject_web_search: false,
                model_ids: vec![bound.id.clone()],
                inject_media_understanding: false,
            })
            .await
            .expect("API key");

        let missing = models_list(State(gateway.clone()), HeaderMap::new()).await;
        assert_eq!(missing.status(), axum::http::StatusCode::UNAUTHORIZED);

        let mut invalid_headers = HeaderMap::new();
        invalid_headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer invalid"),
        );
        let invalid = models_list(State(gateway.clone()), invalid_headers).await;
        assert_eq!(invalid.status(), axum::http::StatusCode::UNAUTHORIZED);

        let mut valid_headers = HeaderMap::new();
        valid_headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", key.token)).expect("auth header"),
        );
        let security = Security::new(gateway.storage.auth());
        let principal = crate::hook::Principal::new(key.id.clone());
        security
            .authorize_principal_model(&principal, &bound)
            .await
            .expect("bound model authorization");
        assert!(matches!(
            security
                .authorize_principal_model(&principal, &unbound)
                .await,
            Err(crate::error::GatewayError::Forbidden {
                reason: crate::error::AccessDenial::ModelNotAllowed
            })
        ));
        assert_eq!(
            listed_model_ids(models_list(State(gateway.clone()), valid_headers.clone()).await)
                .await,
            ["bound-model"]
        );
        let listed =
            listed_models(models_list(State(gateway.clone()), valid_headers.clone()).await).await;
        assert_eq!(listed[0]["display_name"], "Shared label");
        assert_eq!(
            listed[0]["stravia:thinking_levels"],
            serde_json::json!(["off", "minimal", "low", "medium", "high"])
        );
        gateway
            .admin()
            .update_api_key(
                &key.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: None,
                    is_enabled: None,
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    expires_at: None,
                    model_ids: Some(Vec::new()),
                    inject_media_understanding: None,
                },
            )
            .await
            .expect("allow every model");
        security
            .authorize_principal_model(&principal, &unbound)
            .await
            .expect("unrestricted key authorizes a model");
        assert_eq!(
            listed_model_ids(models_list(State(gateway.clone()), valid_headers.clone()).await)
                .await,
            ["bound-model", "unbound-model", "unnamed-model"]
        );
        let listed =
            listed_models(models_list(State(gateway.clone()), valid_headers.clone()).await).await;
        assert_eq!(
            listed
                .iter()
                .map(|model| model["display_name"].as_str().expect("display name"))
                .collect::<Vec<_>>(),
            ["Shared label", "Shared label", "unnamed-model"],
            "duplicate display names must not collapse distinct Model IDs"
        );

        let targets = bound
            .targets
            .iter()
            .map(|target| {
                let mut map = target.thinking_level_map.0.clone();
                for row in &mut map {
                    row.control = crate::thinking::TargetThinkingControl::Hidden;
                }
                crate::db::models::UpsertTarget {
                    id: Some(target.id.clone()),
                    provider_id: target.provider_id.clone(),
                    model: target.model.clone(),
                    enabled: target.enabled,
                    priority: Some(target.priority),
                    first_token_timeout_ms: Some(target.first_token_timeout_ms),
                    target_retry_budget: Some(target.target_retry_budget),
                    target_cooldown_ms: Some(target.target_cooldown_ms),
                    thinking_level_map: map,
                }
            })
            .collect();
        gateway
            .admin()
            .update_model(
                &bound.model_id,
                crate::db::models::UpdateRoute {
                    targets: Some(targets),
                    ..Default::default()
                },
            )
            .await
            .expect("close every Thinking Level");
        let listed =
            listed_models(models_list(State(gateway.clone()), valid_headers.clone()).await).await;
        assert!(listed[0].get("stravia:thinking_levels").is_none());

        gateway
            .admin()
            .update_api_key(
                &key.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: None,
                    is_enabled: Some(false),
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    expires_at: None,
                    model_ids: None,
                    inject_media_understanding: None,
                },
            )
            .await
            .expect("disable API key");
        let disabled = models_list(State(gateway.clone()), valid_headers.clone()).await;
        assert_eq!(disabled.status(), axum::http::StatusCode::FORBIDDEN);

        gateway
            .admin()
            .update_api_key(
                &key.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: None,
                    is_enabled: Some(true),
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    expires_at: Some("2000-01-01T00:00:00Z".into()),
                    model_ids: None,
                    inject_media_understanding: None,
                },
            )
            .await
            .expect("expire API key");
        let expired = models_list(State(gateway), valid_headers).await;
        assert_eq!(expired.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn models_list_rejects_when_authentication_is_unavailable() {
        let storage = std::sync::Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            vec![Route {
                id: "model-id".into(),
                model_id: "model".into(),
                display_name: None,
                balance: "traffic_equalization".into(),
                target_provider: String::new(),
                target_model: String::new(),
                is_enabled: true,
                created_at: "2000-01-01T00:00:00Z".into(),
                supported_thinking_levels: sqlx::types::Json(Vec::new()),
                context_window: None,
                output_max_tokens: None,
                supports_image_input: false,
                targets: Vec::new(),
            }],
            Vec::new(),
        ));
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let config = crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        };
        let (gateway, _logs) = crate::Gateway::builder(config)
            .storage(storage)
            .build()
            .await
            .expect("gateway");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer unavailable"),
        );

        let response = models_list(State(gateway), headers).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}

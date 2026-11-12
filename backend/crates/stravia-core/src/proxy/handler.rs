//! Ingress handlers that remain in the legacy handler module.
//!
//! The full proxy pipeline lives in `proxy/dispatcher.rs`.
//! Old ingress handlers (`openai_proxy`, `anthropic_proxy`, etc.) have been
//! replaced by `proxy/ingress/*.rs` thin shells wired directly in `server.rs`.
//!
//! Read-only model discovery endpoints that do not go through the proxy pipeline:
//! `models_list` (`/v1/models`) and `codex_models_list` (`/codex/models`).

use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::Gateway;
use crate::admin::routes::RouteModule;
use crate::connect_client_apply::{ConnectClientModel, codex_catalog};
use crate::db::models::RouteOutputModality;
use crate::error::GatewayError;
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
        .filter(|model| {
            unrestricted_model_access || accessible_route_ids.contains(model.id.as_str())
        })
        .filter(|model| !model.model_id.trim().is_empty())
        .map(|model| {
            (
                model.model_id.trim().to_string(),
                model.effective_display_name().to_string(),
                model.supported_thinking_levels.clone(),
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

// ── GET /codex/models ─────────────────────────────────────────────────────────

/// Codex `model_catalog_url` 端点：返回 `{"models": [ModelInfo, ...]}`，不是 `/v1/models` 的 `{"data": [...]}`。
///
/// 认证与可见范围同 `/v1/models`。Codex 只能用文本输出的 Route 做对话，其余 Route 不进入目录。
/// 401/403 会让 Codex 清空目录，所以认证失败按原状态码返回，不降级为空目录。
pub async fn codex_models_list(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let credential = ClientCredential::from_inference_headers(&headers);
    let access = match Security::new(gw.storage.auth())
        .model_catalog_access(&credential)
        .await
    {
        Ok(access) => access,
        Err(error) => return error.render(None),
    };

    let mut routes = gw
        .model_cache
        .read()
        .await
        .models
        .iter()
        .filter(|route| {
            route.output_modality == Some(RouteOutputModality::Text)
                && !route.model_id.trim().is_empty()
                && access.allows(&route.id)
        })
        .cloned()
        .collect::<Vec<_>>();
    // 缓存里的 Route 不带上下文窗口与图片输入能力，需按已启用 Target 的 Provider Model 投影。
    if let Err(error) = RouteModule::new(&gw)
        .project_client_capabilities(&mut routes)
        .await
    {
        return GatewayError::internal(error).render(None);
    }
    routes.sort_by(|left, right| {
        left.effective_display_name()
            .cmp(right.effective_display_name())
            .then_with(|| left.model_id.cmp(&right.model_id))
    });

    let models = routes
        .iter()
        .map(ConnectClientModel::from)
        .collect::<Vec<_>>();
    Json(codex_catalog(&models, access.media_understanding())).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::{HeaderValue, header};

    use super::*;
    use crate::db::models::{
        CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput, ProviderSourceInput,
        RouteConfig,
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
        let gateway = Gateway::new(config).await.expect("gateway init");
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some("Models list provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some("openai-compatible".into()),
                    base_url: "http://127.0.0.1:9/v1".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "provider-secret".into(),
                },
                vendor_options: Default::default(),
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
                        "modalities": {"input": ["text"], "output": ["text"]},
                    }),
                    template_id: None,
                },
            )
            .await
            .expect("Provider Model");
        let create_model = |model_id: &str, display_name: Option<&str>| CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: model_id.into(),
            display_name: display_name.map(ToOwned::to_owned),
            balance: Some("traffic_equalization".into()),
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
            default_thinking_level: None,
            billing: None,
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
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                transparent_injections: Default::default(),
                model_ids: vec![bound.id.clone().into()],
                spend_limit: None,
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
        let principal = stravia_runtime_contract::Principal::new(key.id.clone());
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
        gateway
            .admin()
            .update_api_key(
                &key.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    rpm_limit: None,
                    is_enabled: None,
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    transparent_injections: None,
                    expires_at: None,
                    model_ids: Some(Vec::new()),
                    spend_limit: None,
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
                let mut map = target.thinking_level_map.clone();
                for row in &mut map {
                    row.control = stravia_runtime_contract::thinking::TargetThinkingControl::Hidden;
                }
                crate::db::models::CreateTarget {
                    provider_id: target.provider_id().clone().into(),
                    model: target.model().clone().into(),
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
                    rpm_limit: None,
                    is_enabled: Some(false),
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    transparent_injections: None,
                    expires_at: None,
                    model_ids: None,
                    spend_limit: None,
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
                    rpm_limit: None,
                    is_enabled: Some(true),
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    transparent_injections: None,
                    expires_at: Some("2000-01-01T00:00:00Z".into()),
                    model_ids: None,
                    spend_limit: None,
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
            vec![RouteConfig {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                image_generation_bound: false,
                id: "model-id".into(),
                model_id: "model".into(),
                display_name: None,
                balance: "traffic_equalization".into(),

                is_enabled: true,
                created_at: "2000-01-01T00:00:00Z".into(),
                supported_thinking_levels: Vec::new(),
                context_window: None,
                supports_image_input: false,
                supports_remote_compaction: false,
                targets: Vec::new(),
                default_thinking_level: None,
                billing: None,
            }],
            Vec::new(),
        ));
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let config = crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        };
        let gateway = crate::Gateway::builder(config)
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

    async fn codex_catalog_models(response: Response) -> Vec<serde_json::Value> {
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("Codex catalog body");
        let catalog: serde_json::Value = serde_json::from_slice(&body).expect("Codex catalog JSON");
        catalog["models"]
            .as_array()
            .expect("Codex catalog models")
            .clone()
    }

    #[tokio::test]
    async fn codex_models_list_projects_the_keys_text_routes_with_provider_model_specs() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let config = crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        };
        let gateway = Gateway::new(config).await.expect("gateway init");
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some("Codex catalog provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some("openai-compatible".into()),
                    base_url: "http://127.0.0.1:9/v1".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "provider-secret".into(),
                },
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await
            .expect("provider");
        for (upstream, context, input, output) in [
            (
                "vision-upstream",
                200_000,
                ["text", "image"].as_slice(),
                "text",
            ),
            ("text-upstream", 64_000, ["text"].as_slice(), "text"),
            ("image-upstream", 4_000, ["text"].as_slice(), "image"),
        ] {
            gateway
                .admin()
                .create_manual_provider_model(
                    &provider.id,
                    upstream,
                    CreateManualProviderModel {
                        metadata: serde_json::json!({
                            "id": upstream,
                            "name": upstream,
                            "limit": {"context": context, "output": 8_000},
                            "modalities": {"input": input, "output": [output]},
                        }),
                        template_id: None,
                    },
                )
                .await
                .expect("Provider Model");
        }
        let mut routes = std::collections::BTreeMap::new();
        for (model_id, display_name, upstream, modality) in [
            (
                "vision-route",
                Some("Vision"),
                "vision-upstream",
                crate::db::models::RouteOutputModality::Text,
            ),
            (
                "text-route",
                None,
                "text-upstream",
                crate::db::models::RouteOutputModality::Text,
            ),
            (
                "image-route",
                None,
                "image-upstream",
                crate::db::models::RouteOutputModality::Image,
            ),
            (
                "unbound-route",
                None,
                "text-upstream",
                crate::db::models::RouteOutputModality::Text,
            ),
        ] {
            let route = gateway
                .admin()
                .create_model(CreateRoute {
                    output_modality: modality,
                    model_id: model_id.into(),
                    display_name: display_name.map(ToOwned::to_owned),
                    balance: Some("traffic_equalization".into()),
                    targets: vec![CreateTarget {
                        provider_id: provider.id.clone(),
                        model: upstream.into(),
                        enabled: true,
                        priority: Some(1),
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    }],
                    default_thinking_level: None,
                    billing: None,
                })
                .await
                .expect("Route");
            routes.insert(model_id, route);
        }
        let create_key = |name: &str, media_understanding: bool| crate::db::models::CreateApiKey {
            key: None,
            name: name.into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: media_understanding,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items(
                media_understanding
                    .then_some(crate::db::models::TransparentInjection::MediaUnderstanding),
            ),
            model_ids: ["vision-route", "text-route", "image-route"]
                .map(|model_id| routes[model_id].id.clone().into())
                .to_vec(),
            spend_limit: None,
        };
        let plain_key = gateway
            .admin()
            .create_api_key(create_key("Codex plain key", false))
            .await
            .expect("plain API key");
        let media_key = gateway
            .admin()
            .create_api_key(create_key("Codex media key", true))
            .await
            .expect("media API key");
        let headers_for = |key: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {key}")).expect("auth header"),
            );
            headers
        };

        assert_eq!(
            codex_models_list(State(gateway.clone()), HeaderMap::new())
                .await
                .status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            codex_models_list(State(gateway.clone()), headers_for("invalid"))
                .await
                .status(),
            axum::http::StatusCode::UNAUTHORIZED
        );

        let models = codex_catalog_models(
            codex_models_list(State(gateway.clone()), headers_for(&plain_key.token)).await,
        )
        .await;
        assert_eq!(
            models
                .iter()
                .map(|model| model["slug"].as_str().expect("slug"))
                .collect::<Vec<_>>(),
            ["vision-route", "text-route"],
            "only the key's text-output Routes, ordered by display name"
        );
        assert_eq!(models[0]["display_name"], "Vision");
        assert_eq!(models[0]["priority"], 0);
        assert_eq!(models[1]["priority"], 1);
        assert_eq!(models[0]["context_window"], 200_000);
        assert_eq!(models[1]["context_window"], 64_000);
        assert_eq!(
            models[0]["input_modalities"],
            serde_json::json!(["text", "image"])
        );
        assert_eq!(models[1]["input_modalities"], serde_json::json!(["text"]));

        let models = codex_catalog_models(
            codex_models_list(State(gateway), headers_for(&media_key.token)).await,
        )
        .await;
        assert_eq!(
            models[1]["input_modalities"],
            serde_json::json!(["text", "image"]),
            "Media Understanding injection lets text-only Routes accept images"
        );
    }
}

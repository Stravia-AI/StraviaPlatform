use async_trait::async_trait;
use axum::{Router, http::StatusCode, routing::get};
use serde_json::json;

use super::*;
use crate::config::GatewayConfig;
use crate::db::models::{CreateProvider, ProviderCredentialInput, ProviderSourceInput};
use crate::provider_models::{
    CreateManualProviderModel, ProviderModelSelectionPolicy, UpdateProviderModelSelection,
};
use crate::thinking::mapping_control;

struct StubModelDiscovery;

#[async_trait]
impl ProviderModelDiscovery for StubModelDiscovery {
    async fn discover(
        &self,
        _admin: &AdminService,
        provider_id: &str,
    ) -> Result<Vec<String>, RouteModelDiscoveryError> {
        Ok(vec![format!("{provider_id}-model")])
    }
}

#[tokio::test]
async fn route_model_discovery_uses_the_injected_adapter() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..GatewayConfig::default()
    })
    .await?;
    let discovery = StubModelDiscovery;
    let admin = gateway.admin();
    let models = RouteModule::with_model_discovery(&admin, &discovery)
        .discover_provider_model_ids("provider")
        .await?;
    assert_eq!(models, ["provider-model"]);
    Ok(())
}

#[tokio::test]
async fn discovery_http_failure_is_typed_and_hides_the_response_body() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/models",
                get(|| async { (StatusCode::BAD_GATEWAY, "secret upstream diagnostic") }),
            ),
        )
        .await
        .expect("serve discovery fixture");
    });

    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..GatewayConfig::default()
    })
    .await?;
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Discovery Error Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: format!("http://{address}"),
                models_source: Some(format!("http://{address}/models")),
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "discovery-test-key".into(),
            },
            use_proxy: false,
        })
        .await?;

    let error = RouteModule::new(&admin)
        .discover_provider_model_ids(&provider.id)
        .await
        .expect_err("discovery should reject the upstream status");
    assert!(
        matches!(
            &error,
            RouteModelDiscoveryError::DiscoveryHttpStatus {
                provider_id,
                status: 502,
            } if provider_id == &provider.id
        ),
        "unexpected Route error: {error:?}"
    );
    assert!(!error.to_string().contains("secret upstream diagnostic"));
    Ok(())
}

async fn route_fixture_with_protocol(
    protocol: &str,
) -> anyhow::Result<(tempfile::TempDir, Gateway, Provider)> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..GatewayConfig::default()
    })
    .await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Route Test Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: protocol.into(),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await?;
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "upstream-model",
            CreateManualProviderModel {
                metadata: json!({
                    "id": "upstream-model",
                    "name": "Upstream Model",
                    "limit": {
                        "context": 200000,
                        "output": 32000
                    },
                    "modalities": {
                        "input": ["text", "image"],
                        "output": ["text"]
                    }
                }),
            },
        )
        .await?;
    Ok((data_dir, gateway, provider))
}

async fn route_fixture() -> anyhow::Result<(tempfile::TempDir, Gateway, Provider)> {
    route_fixture_with_protocol("openai").await
}

#[test]
fn route_wire_inputs_use_model_id_and_reject_legacy_name() {
    let current = serde_json::from_value::<CreateRoute>(json!({
        "model_id": "client-model",
        "display_name": "Friendly model",
        "target_provider": "provider",
        "target_model": "upstream-model"
    }));
    assert!(current.is_ok(), "current Route contract must deserialize");

    let legacy = serde_json::from_value::<CreateRoute>(json!({
        "name": "client-model",
        "target_provider": "provider",
        "target_model": "upstream-model"
    }));
    assert!(legacy.is_err(), "legacy name input must be rejected");
}

#[tokio::test]
async fn one_click_bind_is_idempotent_and_uses_upstream_id_as_route_id() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin);
    let input = RouteBind::OneClick {
        provider_id: provider.id.clone(),
        provider_model_id: "upstream-model".into(),
    };

    let first = routes.bind(input.clone()).await?;
    let detail = admin
        .get_provider_model(&provider.id, "upstream-model")
        .await?;
    admin
        .update_provider_model_selection(
            &provider.id,
            "upstream-model",
            UpdateProviderModelSelection {
                policy: ProviderModelSelectionPolicy::ForceDisabled,
                revision: detail.revision,
            },
        )
        .await?;
    let second = routes.bind(input).await?;

    assert_eq!(first.id, second.id);
    assert_eq!(first.model_id, "upstream-model");
    assert_eq!(second.targets.len(), 1);
    assert_eq!(second.targets[0].provider_id, provider.id);
    assert_eq!(second.targets[0].model, "upstream-model");
    assert_eq!(second.context_window, Some(200_000));
    assert_eq!(second.output_max_tokens, Some(32_000));
    assert!(second.supports_image_input);
    Ok(())
}

#[tokio::test]
async fn route_ids_are_compared_exactly_when_binding() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin);

    for route_id in ["CaseRoute", "caseroute"] {
        routes
            .bind(RouteBind::At {
                route_id: route_id.into(),
                provider_id: provider.id.clone(),
                provider_model_id: "upstream-model".into(),
                weight: 100,
                priority: 1,
            })
            .await?;
    }

    let route_ids = routes
        .list()
        .await?
        .into_iter()
        .map(|route| route.model_id)
        .collect::<Vec<_>>();
    assert_eq!(route_ids.len(), 2);
    assert!(route_ids.iter().any(|route_id| route_id == "CaseRoute"));
    assert!(route_ids.iter().any(|route_id| route_id == "caseroute"));
    Ok(())
}

#[tokio::test]
async fn route_get_uses_exact_route_id_and_never_storage_id() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin);
    let route = routes
        .bind(RouteBind::At {
            route_id: "ExactRoute".into(),
            provider_id: provider.id,
            provider_model_id: "upstream-model".into(),
            weight: 100,
            priority: 1,
        })
        .await?;

    assert_eq!(routes.get("ExactRoute").await?.id, route.id);
    assert!(routes.get("exactroute").await.is_err());
    assert!(routes.get(&route.id).await.is_err());
    let cache = gateway.model_cache.read().await;
    assert!(cache.match_model("ExactRoute").is_some());
    assert!(cache.match_model("exactroute").is_none());
    assert!(cache.match_model(&route.id).is_none());
    Ok(())
}

#[tokio::test]
async fn route_display_name_is_optional_normalized_and_not_an_identity() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let create = |model_id: &str| CreateRoute {
        model_id: model_id.into(),
        display_name: Some("  Shared label  ".into()),
        balance: Some("priority".into()),
        target_provider: provider.id.clone(),
        target_model: "upstream-model".into(),
        targets: Vec::new(),
    };

    let first = admin
        .create_model(create("  CaseSensitive/Model  "))
        .await?;
    let second = admin.create_model(create("other-model")).await?;
    assert_eq!(first.model_id, "CaseSensitive/Model");
    assert_eq!(first.display_name.as_deref(), Some("Shared label"));
    assert_eq!(second.display_name.as_deref(), Some("Shared label"));

    let renamed = admin
        .update_model(
            &first.model_id,
            UpdateRoute {
                display_name: Some("   ".into()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(renamed.id, first.id);
    assert_eq!(renamed.model_id, "CaseSensitive/Model");
    assert!(renamed.display_name.is_none());
    assert_eq!(renamed.effective_display_name(), "CaseSensitive/Model");
    assert!(admin.get_model("casesensitive/model").await.is_err());
    assert_eq!(
        gateway
            .model_cache
            .read()
            .await
            .match_model("CaseSensitive/Model")
            .expect("cached Route")
            .id,
        first.id
    );
    Ok(())
}

#[tokio::test]
async fn unavailable_provider_model_cannot_be_bound_as_a_new_target() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let detail = admin
        .get_provider_model(&provider.id, "upstream-model")
        .await?;
    admin
        .update_provider_model_selection(
            &provider.id,
            "upstream-model",
            UpdateProviderModelSelection {
                policy: ProviderModelSelectionPolicy::ForceDisabled,
                revision: detail.revision,
            },
        )
        .await?;

    let error = RouteModule::new(&admin)
        .bind(RouteBind::OneClick {
            provider_id: provider.id.clone(),
            provider_model_id: "upstream-model".into(),
        })
        .await
        .expect_err("unavailable Provider Model must be rejected");

    assert!(error.to_string().contains("not available"));
    let change_error = admin
        .create_model(CreateRoute {
            model_id: "manual-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "upstream-model".into(),
            targets: vec![CreateTarget {
                provider_id: provider.id,
                model: "upstream-model".into(),
                weight: Some(100),
                priority: Some(1),
                thinking_level_map: Vec::new(),
            }],
        })
        .await
        .expect_err("Route change must enforce Effective Availability");
    assert!(change_error.to_string().contains("not available"));
    assert!(admin.list_models().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn missing_provider_model_cannot_be_added_as_a_new_target() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();

    let error = admin
        .create_model(CreateRoute {
            model_id: "missing-model-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "missing-model".into(),
            targets: vec![],
        })
        .await
        .expect_err("a Target requires a Provider Model snapshot");

    assert!(error.to_string().contains("PROVIDER_MODEL_NOT_FOUND"));
    assert!(admin.list_models().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn unbinding_the_last_target_deletes_the_route() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin);
    routes
        .bind(RouteBind::OneClick {
            provider_id: provider.id.clone(),
            provider_model_id: "upstream-model".into(),
        })
        .await?;

    let remaining = routes
        .unbind(RouteUnbind {
            route_id: "upstream-model".into(),
            provider_id: provider.id,
            provider_model_id: "upstream-model".into(),
        })
        .await?;

    assert!(remaining.is_none());
    assert!(admin.list_models().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn route_generates_seven_rows_seeds_levels_and_resets_one_override() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    admin
        .create_manual_provider_model(
            &provider.id,
            "effort-model",
            CreateManualProviderModel {
                metadata: json!({
                    "id": "effort-model",
                    "reasoning_options": [{
                        "type": "effort",
                        "values": ["none", "low", "high", "max"]
                    }]
                }),
            },
        )
        .await?;
    let route = admin
        .create_model(CreateRoute {
            model_id: "thinking-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "effort-model".into(),
            targets: Vec::new(),
        })
        .await?;

    assert_eq!(route.targets[0].thinking_level_map.len(), 7);
    assert_eq!(
        route.supported_thinking_levels.0,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            ThinkingLevel::Max
        ]
    );
    assert_eq!(
        mapping_control(&route.targets[0].thinking_level_map, ThinkingLevel::Minimal),
        Some(&crate::thinking::TargetThinkingControl::Hidden)
    );

    let mut targets = route_targets_for_update(&route);
    let low = targets[0]
        .thinking_level_map
        .iter_mut()
        .find(|row| row.level == ThinkingLevel::Low)
        .expect("low row");
    low.control = crate::thinking::TargetThinkingControl::Hidden;
    let updated = admin
        .update_model(
            &route.model_id,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await?;
    assert_eq!(
        updated.supported_thinking_levels.0,
        vec![ThinkingLevel::Off, ThinkingLevel::High, ThinkingLevel::Max]
    );
    let low = updated.targets[0]
        .thinking_level_map
        .iter()
        .find(|row| row.level == ThinkingLevel::Low)
        .expect("low row");
    assert_eq!(low.source, ThinkingMappingSource::Overridden);

    let reset = admin
        .reset_target_thinking_mapping(&route.model_id, &updated.targets[0].id, ThinkingLevel::Low)
        .await?;
    let low = reset.targets[0]
        .thinking_level_map
        .iter()
        .find(|row| row.level == ThinkingLevel::Low)
        .expect("low row");
    assert_eq!(low.source, ThinkingMappingSource::Generated);
    assert_eq!(
        low.control,
        crate::thinking::TargetThinkingControl::Effort {
            value: "low".into()
        }
    );
    assert_eq!(
        reset.supported_thinking_levels.0,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            ThinkingLevel::Max
        ]
    );
    Ok(())
}

#[tokio::test]
async fn open_responses_accepts_max_effort_map() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture_with_protocol("open-responses").await?;
    let admin = gateway.admin();
    admin
        .create_manual_provider_model(
            &provider.id,
            "max-effort-model",
            CreateManualProviderModel {
                metadata: json!({
                    "id": "max-effort-model",
                    "reasoning_options": [{
                        "type": "effort",
                        "values": ["none", "max"]
                    }]
                }),
            },
        )
        .await?;

    let route = admin
        .create_model(CreateRoute {
            model_id: "max-effort-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "max-effort-model".into(),
            targets: Vec::new(),
        })
        .await?;

    assert_eq!(
        mapping_control(&route.targets[0].thinking_level_map, ThinkingLevel::Max),
        Some(&crate::thinking::TargetThinkingControl::Effort {
            value: "max".into()
        })
    );
    Ok(())
}

async fn create_openai_compatible_toggle_route(vendor: &str, model: &str) -> anyhow::Result<Route> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..GatewayConfig::default()
    })
    .await?;
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some(format!("{vendor} Route Test Provider")),
            source: ProviderSourceInput::Custom {
                vendor: Some(vendor.into()),
                protocol: "openai-compatible".into(),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await?;
    admin
        .create_manual_provider_model(
            &provider.id,
            model,
            CreateManualProviderModel {
                metadata: json!({
                    "id": model,
                    "reasoning_options": [{"type": "toggle"}]
                }),
            },
        )
        .await?;

    admin
        .create_model(CreateRoute {
            model_id: model.into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: model.into(),
            targets: Vec::new(),
        })
        .await
}

#[tokio::test]
async fn xiaomi_toggle_model_can_be_bound_over_openai_compatible() -> anyhow::Result<()> {
    let route = create_openai_compatible_toggle_route("xiaomi", "mimo-v2.5").await?;

    assert_eq!(
        route.supported_thinking_levels.0,
        vec![ThinkingLevel::Off, ThinkingLevel::Medium]
    );
    Ok(())
}

#[tokio::test]
async fn unknown_compatible_provider_does_not_guess_toggle_wire_shape() {
    let error = create_openai_compatible_toggle_route("openai-compatible", "custom-toggle-model")
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("THINKING_CONTROL_UNREPRESENTABLE")
    );
}

#[tokio::test]
async fn gemini_accepts_generated_effort_maps() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..GatewayConfig::default()
    })
    .await?;
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Gemini Route Test Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "google-gemini".into(),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await?;
    admin
        .create_manual_provider_model(
            &provider.id,
            "gemini-effort-model",
            CreateManualProviderModel {
                metadata: json!({
                    "id": "gemini-effort-model",
                    "reasoning_options": [{
                        "type": "effort",
                        "values": ["low", "high"]
                    }]
                }),
            },
        )
        .await?;

    let route = admin
        .create_model(CreateRoute {
            model_id: "gemini-thinking-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "gemini-effort-model".into(),
            targets: Vec::new(),
        })
        .await?;

    assert_eq!(
        route.supported_thinking_levels.0,
        vec![ThinkingLevel::Low, ThinkingLevel::High]
    );
    Ok(())
}

#[tokio::test]
async fn supported_levels_are_the_intersection_of_all_targets() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for (model, values, context, output, input_modalities) in [
        (
            "wide-effort-model",
            vec!["none", "low", "high", "max"],
            200_000,
            64_000,
            vec!["text", "image"],
        ),
        (
            "narrow-effort-model",
            vec!["low", "high"],
            128_000,
            32_000,
            vec!["text"],
        ),
    ] {
        admin
            .create_manual_provider_model(
                &provider.id,
                model,
                CreateManualProviderModel {
                    metadata: json!({
                        "id": model,
                        "reasoning_options": [{
                            "type": "effort",
                            "values": values
                        }],
                        "limit": {
                            "context": context,
                            "output": output
                        },
                        "modalities": {
                            "input": input_modalities,
                            "output": ["text"]
                        }
                    }),
                },
            )
            .await?;
    }

    let route = admin
        .create_model(CreateRoute {
            model_id: "intersection-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "wide-effort-model".into(),
            targets: vec![
                CreateTarget {
                    provider_id: provider.id.clone(),
                    model: "wide-effort-model".into(),
                    weight: Some(100),
                    priority: Some(1),
                    thinking_level_map: Vec::new(),
                },
                CreateTarget {
                    provider_id: provider.id,
                    model: "narrow-effort-model".into(),
                    weight: Some(100),
                    priority: Some(1),
                    thinking_level_map: Vec::new(),
                },
            ],
        })
        .await?;

    assert_eq!(
        route.supported_thinking_levels.0,
        vec![ThinkingLevel::Low, ThinkingLevel::High]
    );
    assert_eq!(route.context_window, Some(128_000));
    assert_eq!(route.output_max_tokens, Some(32_000));
    assert!(!route.supports_image_input);
    Ok(())
}

#[tokio::test]
async fn regenerate_updates_derived_supported_levels() -> anyhow::Result<()> {
    let (_data_dir, gateway, _provider) = route_fixture().await?;
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Toggle Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "anthropic".into(),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await?;
    admin
        .create_manual_provider_model(
            &provider.id,
            "toggle-model",
            CreateManualProviderModel {
                metadata: json!({
                    "id": "toggle-model",
                    "reasoning_options": [{"type": "toggle"}]
                }),
            },
        )
        .await?;
    let route = admin
        .create_model(CreateRoute {
            model_id: "toggle-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "toggle-model".into(),
            targets: Vec::new(),
        })
        .await?;
    let mut targets = route_targets_for_update(&route);
    let high = targets[0]
        .thinking_level_map
        .iter_mut()
        .find(|row| row.level == ThinkingLevel::High)
        .expect("high row");
    high.control = crate::thinking::TargetThinkingControl::Effort {
        value: "high".into(),
    };
    let updated = admin
        .update_model(
            &route.model_id,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await?;
    assert_eq!(
        updated.supported_thinking_levels.0,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Medium,
            ThinkingLevel::High
        ]
    );

    let regenerated = admin
        .regenerate_target_thinking_map(&route.model_id, &updated.targets[0].id)
        .await?;
    assert_eq!(
        regenerated.supported_thinking_levels.0,
        vec![ThinkingLevel::Off, ThinkingLevel::Medium]
    );
    Ok(())
}

#[tokio::test]
async fn refresh_regenerates_only_generated_rows() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let route = admin
        .create_model(CreateRoute {
            model_id: "refresh-route".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "upstream-model".into(),
            targets: Vec::new(),
        })
        .await?;
    let mut targets = route_targets_for_update(&route);
    let high = targets[0]
        .thinking_level_map
        .iter_mut()
        .find(|row| row.level == ThinkingLevel::High)
        .expect("high row");
    high.control = crate::thinking::TargetThinkingControl::Effort {
        value: "custom-high".into(),
    };
    let route = admin
        .update_model(
            &route.model_id,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await?;
    let refreshed_metadata: crate::provider_models::ProviderModelMetadata =
        serde_json::from_value(json!({
            "id": "upstream-model",
            "reasoning_options": [{
                "type": "effort",
                "values": ["none", "low", "max"]
            }]
        }))?;
    RouteModule::new(&admin)
        .refresh_generated_thinking_maps(&provider.id, "upstream-model", &refreshed_metadata, true)
        .await?;

    let refreshed = admin
        .list_models()
        .await?
        .into_iter()
        .find(|candidate| candidate.id == route.id)
        .expect("refreshed Route");
    let medium = refreshed.targets[0]
        .thinking_level_map
        .iter()
        .find(|row| row.level == ThinkingLevel::Medium)
        .expect("medium row");
    assert_eq!(medium.source, ThinkingMappingSource::Generated);
    assert_eq!(
        medium.control,
        crate::thinking::TargetThinkingControl::Hidden
    );
    let high = refreshed.targets[0]
        .thinking_level_map
        .iter()
        .find(|row| row.level == ThinkingLevel::High)
        .expect("high row");
    assert_eq!(high.source, ThinkingMappingSource::Overridden);
    assert_eq!(
        high.control,
        crate::thinking::TargetThinkingControl::Effort {
            value: "custom-high".into()
        }
    );
    Ok(())
}

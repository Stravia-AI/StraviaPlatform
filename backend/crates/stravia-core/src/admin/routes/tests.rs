use serde_json::json;

use super::*;
use crate::config::GatewayConfig;
use crate::db::models::{CreateProvider, ProviderCredentialInput, ProviderSourceInput};
use crate::provider_models::{
    CreateManualProviderModel, ProviderModelSelectionPolicy, UpdateProviderModelSelection,
};
use crate::thinking::mapping_control;

async fn route_fixture_with_protocol(
    protocol: &str,
) -> anyhow::Result<(tempfile::TempDir, Gateway, Provider)> {
    let data_dir = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        },
        std::sync::Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Route Test Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some(protocol.into()),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "upstream-model",
            CreateManualProviderModel {
                template_id: None,
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
    route_fixture_with_protocol("openai-compatible").await
}

#[test]
fn route_wire_inputs_require_targets_and_reject_legacy_fields() {
    let current = serde_json::from_value::<CreateRoute>(json!({
        "model_id": "client-model",
        "display_name": "Friendly model",
        "targets": [{"provider_id": "provider", "model": "upstream-model"}]
    }));
    assert!(current.is_ok());

    let provider_only = serde_json::from_value::<CreateRoute>(json!({
        "model_id": "research",
        "targets": [{"provider_id": "provider", "model": null}]
    }))
    .expect("Provider-only Route contract");
    assert!(provider_only.targets[0].model.is_none());

    for legacy in ["name", "target_provider", "target_model"] {
        let mut input = json!({"model_id": "client-model", "targets": []});
        input[legacy] = json!("legacy");
        assert!(serde_json::from_value::<CreateRoute>(input).is_err());
        let mut update = json!({});
        update[legacy] = json!("legacy");
        assert!(serde_json::from_value::<UpdateRoute>(update).is_err());
    }
    assert!(
        serde_json::from_value::<UpdateRoute>(json!({
            "targets": [{"id": "caller-id", "provider_id": "provider", "model": "model"}]
        }))
        .is_err()
    );
}

#[test]
fn route_target_wire_input_rejects_removed_weight() {
    let target = serde_json::from_value::<CreateTarget>(json!({
        "provider_id": "provider",
        "model": "upstream-model",
        "weight": 100
    }));
    assert!(
        target.is_err(),
        "Target.weight must not remain in the write API"
    );
    assert!(
        serde_json::from_value::<CreateTarget>(json!({
            "id": "caller-id", "provider_id": "provider", "model": "upstream-model"
        }))
        .is_err()
    );
}

#[test]
fn route_target_wire_input_defaults_enabled_and_accepts_disabled() {
    let enabled = serde_json::from_value::<CreateTarget>(json!({
        "provider_id": "provider",
        "model": "upstream-model"
    }))
    .expect("Target without enabled");
    let disabled = serde_json::from_value::<CreateTarget>(json!({
        "provider_id": "provider",
        "model": "standby-model",
        "enabled": false
    }))
    .expect("disabled Target");

    let provider_only = serde_json::from_value::<CreateTarget>(json!({
        "provider_id": "research-provider",
        "model": null
    }))
    .expect("Provider-only Target");
    let blank = serde_json::from_value::<CreateTarget>(json!({
        "provider_id": "provider",
        "model": "   "
    }))
    .expect("structurally valid blank Target");

    assert!(enabled.enabled);
    assert!(!disabled.enabled);
    assert!(provider_only.model.is_none());
    assert!(ensure_route_targets_valid(&[blank]).is_err());
}

#[test]
fn route_update_default_thinking_level_distinguishes_omitted_null_and_invalid() {
    let omitted = serde_json::from_value::<UpdateRoute>(json!({})).expect("empty update");
    assert!(omitted.default_thinking_level.is_none());

    let cleared = serde_json::from_value::<UpdateRoute>(json!({"default_thinking_level": null}))
        .expect("explicit null clears the default");
    assert_eq!(cleared.default_thinking_level, Some(None));

    let set = serde_json::from_value::<UpdateRoute>(json!({"default_thinking_level": "high"}))
        .expect("valid Thinking Level");
    assert_eq!(set.default_thinking_level, Some(Some(ThinkingLevel::High)));

    let invalid =
        serde_json::from_value::<UpdateRoute>(json!({"default_thinking_level": "ludicrous"}));
    assert!(invalid.is_err(), "unknown Thinking Level must be rejected");

    let omitted: UpdateRoute = serde_json::from_value(json!({})).expect("omitted patch");
    let cleared: UpdateRoute =
        serde_json::from_value(json!({"display_name": null})).expect("clear display name");
    assert!(omitted.display_name.is_none());
    assert_eq!(cleared.display_name, Some(None));
    assert_eq!(
        serde_json::to_value(&omitted).expect("serialize omitted"),
        json!({})
    );
    assert_eq!(
        serde_json::to_value(&cleared).expect("serialize clear"),
        json!({"display_name": null})
    );
    for field in ["model_id", "balance", "is_enabled", "targets"] {
        let mut invalid = json!({});
        invalid[field] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<UpdateRoute>(invalid).is_err(),
            "null {field} must fail"
        );
    }
}

#[tokio::test]
async fn route_default_thinking_level_round_trips_and_updates() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let route = admin
        .create_model(CreateRoute {
            model_id: "default-thinking-route".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("upstream-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: Some(ThinkingLevel::High),
        })
        .await?;
    assert_eq!(route.default_thinking_level.as_deref(), Some("high"));

    // 未提交字段时保留已存值
    let route = admin
        .update_model(
            "default-thinking-route",
            UpdateRoute {
                display_name: Some(Some("Renamed".into())),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(route.default_thinking_level.as_deref(), Some("high"));
    assert_eq!(route.display_name.as_deref(), Some("Renamed"));
    let cleared = admin
        .update_model(
            "default-thinking-route",
            UpdateRoute {
                display_name: Some(None),
                ..Default::default()
            },
        )
        .await?;
    assert!(cleared.display_name.is_none());
    assert_eq!(cleared.targets[0].id, route.targets[0].id);

    // 显式 null 清除
    let route = admin
        .update_model(
            "default-thinking-route",
            UpdateRoute {
                default_thinking_level: Some(None),
                ..Default::default()
            },
        )
        .await?;
    assert!(route.default_thinking_level.is_none());

    // 当前不被 Target 支持的档位仍可保存（保存时仅校验枚举）
    let route = admin
        .update_model(
            "default-thinking-route",
            UpdateRoute {
                default_thinking_level: Some(Some(ThinkingLevel::Xhigh)),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(route.default_thinking_level.as_deref(), Some("xhigh"));
    Ok(())
}

#[tokio::test]
async fn route_configuration_supports_three_targets_priorities_and_failure_defaults()
-> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for model in ["second-model", "third-model"] {
        admin
            .create_manual_provider_model(
                &provider.id,
                model,
                CreateManualProviderModel {
                    template_id: None,
                    metadata: json!({"id": model, "name": model}),
                },
            )
            .await?;
    }
    let create_target = |model: &str, priority: i32| CreateTarget {
        enabled: true,
        provider_id: provider.id.clone(),
        model: Some(model.into()),
        priority: Some(priority),
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };
    let route = admin
        .create_model(CreateRoute {
            model_id: "layered-route".into(),
            display_name: None,
            balance: None,

            targets: vec![
                create_target("upstream-model", 100_000),
                create_target("second-model", 0),
                create_target("third-model", 0),
            ],
            default_thinking_level: None,
        })
        .await?;
    assert_eq!(route.balance, "traffic_equalization");
    assert_eq!(route.targets.len(), 3);
    assert_eq!(route.targets[0].priority, 100_000);
    assert!(route.targets[1..].iter().all(|target| target.priority == 0));
    assert!(route.targets.iter().all(|target| {
        target.first_token_timeout_ms == DEFAULT_FIRST_TOKEN_TIMEOUT_MS
            && target.target_retry_budget == DEFAULT_TARGET_RETRY_BUDGET
            && target.target_cooldown_ms == DEFAULT_TARGET_COOLDOWN_MS
    }));

    for valid in [-1, 100_001, i32::MIN, i32::MAX] {
        let route = admin
            .create_model(CreateRoute {
                model_id: format!("signed-priority-{valid}"),
                display_name: None,
                balance: Some("traffic_equalization".into()),

                targets: vec![create_target("upstream-model", valid)],
                default_thinking_level: None,
            })
            .await?;
        assert_eq!(route.targets[0].priority, valid);
    }
    let invalid_strategy = admin
        .create_model(CreateRoute {
            model_id: "invalid-strategy".into(),
            display_name: None,
            balance: Some("weighted_random".into()),

            targets: vec![create_target("upstream-model", 0)],
            default_thinking_level: None,
        })
        .await
        .expect_err("unknown Scheduling Strategy must be rejected");
    assert!(
        invalid_strategy
            .to_string()
            .contains("Route Scheduling Strategy")
    );
    Ok(())
}

#[tokio::test]
async fn route_configuration_round_trips_disabled_targets_and_requires_one_enabled()
-> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    admin
        .create_manual_provider_model(
            &provider.id,
            "standby-model",
            CreateManualProviderModel {
                template_id: None,
                metadata: json!({"id": "standby-model", "name": "Standby"}),
            },
        )
        .await?;
    let target = |model: &str, enabled: bool| CreateTarget {
        provider_id: provider.id.clone(),
        model: Some(model.into()),
        enabled,
        priority: Some(if enabled { -1 } else { i32::MAX }),
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };

    let route = admin
        .create_model(CreateRoute {
            model_id: "enabled-and-standby".into(),
            display_name: None,
            balance: None,

            targets: vec![
                target("upstream-model", true),
                target("standby-model", false),
            ],
            default_thinking_level: None,
        })
        .await?;
    assert_eq!(route.targets.len(), 2);
    assert!(
        route
            .targets
            .iter()
            .find(|target| target.model().map(|model| model.as_str()) == Some("upstream-model"))
            .is_some_and(|target| target.enabled)
    );
    assert!(
        route
            .targets
            .iter()
            .find(|target| target.model().map(|model| model.as_str()) == Some("standby-model"))
            .is_some_and(|target| !target.enabled)
    );
    assert_eq!(
        route
            .primary_target()
            .and_then(|target| target.model().map(|model| model.as_str())),
        Some("upstream-model")
    );

    let error = admin
        .create_model(CreateRoute {
            model_id: "all-disabled".into(),
            display_name: None,
            balance: None,

            targets: vec![target("upstream-model", false)],
            default_thinking_level: None,
        })
        .await
        .expect_err("Route without an enabled Target must be rejected");
    assert!(error.to_string().contains("enabled Target"));
    Ok(())
}

#[tokio::test]
async fn one_click_bind_is_idempotent_and_uses_upstream_id_as_route_id() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin.gw);
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
    assert_eq!(first.display_name.as_deref(), Some("Upstream Model"));
    assert_eq!(second.targets.len(), 1);
    assert_eq!(
        second.targets[0].provider_id().as_str(),
        provider.id.as_str()
    );
    assert_eq!(
        second.targets[0].model().map(|model| model.as_str()),
        Some("upstream-model")
    );
    assert!(second.targets[0].enabled);
    assert_eq!(second.context_window, Some(200_000));
    assert_eq!(second.output_max_tokens, Some(32_000));
    assert!(second.supports_image_input);
    Ok(())
}

#[tokio::test]
async fn route_ids_are_compared_exactly_when_binding() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin.gw);

    for route_id in ["CaseRoute", "caseroute"] {
        routes
            .bind(RouteBind::At {
                route_id: route_id.into(),
                provider_id: provider.id.clone(),
                provider_model_id: "upstream-model".into(),
                priority: 1,
                first_token_timeout_ms: DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
                target_retry_budget: DEFAULT_TARGET_RETRY_BUDGET,
                target_cooldown_ms: DEFAULT_TARGET_COOLDOWN_MS,
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
async fn target_models_match_inventory_by_segment_and_case() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    admin
        .create_manual_provider_model(
            &provider.id,
            "zhipuai/glm-4.6",
            CreateManualProviderModel {
                template_id: None,
                metadata: json!({
                    "id": "zhipuai/glm-4.6",
                    "name": "GLM-4.6",
                    "limit": { "context": 131072, "output": 16384 },
                    "modalities": { "input": ["text"], "output": ["text"] }
                }),
            },
        )
        .await?;
    let create_target = |model: &str| CreateTarget {
        enabled: true,
        provider_id: provider.id.clone(),
        model: Some(model.into()),
        priority: None,
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };

    // Target model 与清单 ID 仅在最右段与大小写上一致时也能命中
    let route = admin
        .create_model(CreateRoute {
            model_id: "glm-4.6".into(),
            display_name: None,
            balance: None,

            targets: vec![create_target("GLM-4.6")],
            default_thinking_level: None,
        })
        .await?;
    assert_eq!(route.targets.len(), 1);
    assert_eq!(
        route.targets[0].model().map(|model| model.as_str()),
        Some("GLM-4.6")
    );
    // 能力元数据经宽松匹配解析成功
    assert_eq!(route.context_window, Some(131_072));
    assert_eq!(route.output_max_tokens, Some(16_384));
    assert!(!route.supports_image_input);
    Ok(())
}

#[tokio::test]
async fn ambiguous_inventory_segments_keep_target_errors_visible() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for model in ["openai/gpt-4o", "azure/gpt-4o"] {
        admin
            .create_manual_provider_model(
                &provider.id,
                model,
                CreateManualProviderModel {
                    template_id: None,
                    metadata: json!({ "id": model, "name": model }),
                },
            )
            .await?;
    }
    let create_target = |model: &str| CreateTarget {
        enabled: true,
        provider_id: provider.id.clone(),
        model: Some(model.into()),
        priority: None,
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };
    let input = |target: CreateTarget| CreateRoute {
        model_id: "gpt-4o-route".into(),
        display_name: None,
        balance: None,

        targets: vec![target],
        default_thinking_level: None,
    };

    // 两个清单 ID 的最右段相同，宽松匹配无法区分时保持报错而不是任意选择
    let error = admin
        .create_model(input(create_target("gpt-4o")))
        .await
        .expect_err("ambiguous segment must not bind");
    assert!(
        error.to_string().contains("PROVIDER_MODEL_NOT_FOUND"),
        "unexpected error: {error:?}"
    );
    // 带命名空间的 ID 精确命中不受影响
    admin
        .create_model(input(create_target("openai/gpt-4o")))
        .await?;
    Ok(())
}

#[tokio::test]
async fn bind_treats_case_variants_of_one_inventory_model_as_a_single_target() -> anyhow::Result<()>
{
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin.gw);

    for provider_model_id in ["upstream-model", "Upstream-Model"] {
        routes
            .bind(RouteBind::At {
                route_id: "cased-route".into(),
                provider_id: provider.id.clone(),
                provider_model_id: provider_model_id.into(),
                priority: 1,
                first_token_timeout_ms: DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
                target_retry_budget: DEFAULT_TARGET_RETRY_BUDGET,
                target_cooldown_ms: DEFAULT_TARGET_COOLDOWN_MS,
            })
            .await?;
    }

    let route = routes.get("cased-route").await?;
    assert_eq!(route.targets.len(), 1);
    assert_eq!(
        route.targets[0].model().map(|model| model.as_str()),
        Some("upstream-model")
    );
    Ok(())
}

#[tokio::test]
async fn route_get_uses_exact_route_id_and_never_storage_id() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin.gw);
    let route = routes
        .bind(RouteBind::At {
            route_id: "ExactRoute".into(),
            provider_id: provider.id,
            provider_model_id: "upstream-model".into(),
            priority: 1,
            first_token_timeout_ms: DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
            target_retry_budget: DEFAULT_TARGET_RETRY_BUDGET,
            target_cooldown_ms: DEFAULT_TARGET_COOLDOWN_MS,
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
async fn target_statuses_use_exact_route_id_and_never_storage_id() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let route = RouteModule::new(&admin.gw)
        .bind(RouteBind::At {
            route_id: "ExactRoute".into(),
            provider_id: provider.id,
            provider_model_id: "upstream-model".into(),
            priority: 1,
            first_token_timeout_ms: DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
            target_retry_budget: DEFAULT_TARGET_RETRY_BUDGET,
            target_cooldown_ms: DEFAULT_TARGET_COOLDOWN_MS,
        })
        .await?;

    let statuses = admin.get_model_target_statuses("ExactRoute").await?;
    assert_eq!(
        statuses
            .iter()
            .map(|status| status.target_id.as_str())
            .collect::<Vec<_>>(),
        route
            .targets
            .iter()
            .map(|target| target.id.as_str())
            .collect::<Vec<_>>()
    );
    assert!(admin.get_model_target_statuses("exactroute").await.is_err());
    assert!(admin.get_model_target_statuses(&route.id).await.is_err());
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
        targets: vec![CreateTarget {
            provider_id: provider.id.clone(),
            model: Some("upstream-model".into()),
            enabled: true,
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
            thinking_level_map: Vec::new(),
        }],
        default_thinking_level: None,
    };

    let first = admin
        .create_model(create("  CaseSensitive/Model  "))
        .await?;
    let second = admin.create_model(create("other-model")).await?;
    assert_eq!(first.model_id, "CaseSensitive/Model");
    assert_eq!(first.balance, "traffic_equalization");
    assert_eq!(first.display_name.as_deref(), Some("Shared label"));
    assert_eq!(second.display_name.as_deref(), Some("Shared label"));

    let renamed = admin
        .update_model(
            &first.model_id,
            UpdateRoute {
                display_name: Some(Some("   ".into())),
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

    let error = RouteModule::new(&admin.gw)
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

            targets: vec![CreateTarget {
                provider_id: provider.id,
                model: Some("upstream-model".into()),
                enabled: true,
                priority: Some(1),
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
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
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("missing-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
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
    let routes = RouteModule::new(&admin.gw);
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
                template_id: None,
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
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("effort-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;

    assert_eq!(route.targets[0].thinking_level_map.len(), 7);
    assert_eq!(
        route.supported_thinking_levels,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            ThinkingLevel::Max
        ]
    );
    assert_eq!(
        mapping_control(&route.targets[0].thinking_level_map, ThinkingLevel::Minimal),
        Some(&stravia_runtime_contract::thinking::TargetThinkingControl::Hidden)
    );

    let mut targets = route_targets_for_update(&route);
    let low = targets[0]
        .thinking_level_map
        .iter_mut()
        .find(|row| row.level == ThinkingLevel::Low)
        .expect("low row");
    low.control = stravia_runtime_contract::thinking::TargetThinkingControl::Hidden;
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
        updated.supported_thinking_levels,
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
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "low".into()
        }
    );
    assert_eq!(
        reset.supported_thinking_levels,
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
                template_id: None,
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
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("max-effort-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;

    assert_eq!(
        mapping_control(&route.targets[0].thinking_level_map, ThinkingLevel::Max),
        Some(
            &stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: "max".into()
            }
        )
    );
    Ok(())
}

async fn create_toggle_route(
    vendor: &str,
    model: &str,
    protocol: &str,
    capabilities: &[&str],
    reasoning: Option<bool>,
) -> anyhow::Result<RouteConfig> {
    let data_dir = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        },
        std::sync::Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await?;
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some(format!("{vendor} Route Test Provider")),
            source: ProviderSourceInput::Custom {
                vendor: vendor.into(),
                channel: "default".into(),
                protocol: Some(protocol.into()),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "route-fixture-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    admin
        .create_manual_provider_model(
            &provider.id,
            model,
            CreateManualProviderModel {
                template_id: None,
                metadata: json!({
                    "id": model,
                    "reasoning": reasoning,
                    "reasoning_options": [{"type": "toggle"}],
                    "capabilities": capabilities,
                }),
            },
        )
        .await?;

    admin
        .create_model(CreateRoute {
            model_id: model.into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some(model.into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
}

#[tokio::test]
async fn xiaomi_toggle_model_can_be_bound_over_openai_compatible() -> anyhow::Result<()> {
    let route = create_toggle_route("xiaomi", "mimo-v2.5", "openai-compatible", &[], None).await?;

    assert_eq!(
        route.supported_thinking_levels,
        vec![ThinkingLevel::Off, ThinkingLevel::Medium]
    );
    Ok(())
}

#[tokio::test]
async fn unknown_compatible_provider_hides_generated_toggle_controls() -> anyhow::Result<()> {
    let route = create_toggle_route(
        "openai-compatible",
        "custom-toggle-model",
        "openai-compatible",
        &[],
        None,
    )
    .await?;

    assert!(route.supported_thinking_levels.is_empty());
    for row in route.targets[0].thinking_level_map.iter() {
        assert_eq!(
            row.control,
            stravia_runtime_contract::thinking::TargetThinkingControl::Hidden
        );
        assert_eq!(row.source, ThinkingMappingSource::Generated);
    }
    Ok(())
}

#[tokio::test]
async fn unknown_protocol_does_not_inherit_open_responses_controls() -> anyhow::Result<()> {
    let route = create_toggle_route(
        "openai-compatible",
        "unknown-wire-toggle-model",
        "vendor-private-wire",
        &[],
        None,
    )
    .await?;

    assert!(route.supported_thinking_levels.is_empty());
    assert!(
        route.targets[0]
            .thinking_level_map
            .iter()
            .all(|row| row.control.is_hidden())
    );
    Ok(())
}

#[tokio::test]
async fn model_capability_declaration_authorizes_compatible_toggle_controls() -> anyhow::Result<()>
{
    let route = create_toggle_route(
        "openai-compatible",
        "declared-toggle-model",
        "openai-compatible",
        &[stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE],
        None,
    )
    .await?;

    assert_eq!(
        route.supported_thinking_levels,
        vec![ThinkingLevel::Off, ThinkingLevel::Medium]
    );
    Ok(())
}

#[tokio::test]
async fn reasoning_false_rejects_declared_toggle_controls() -> anyhow::Result<()> {
    let route = create_toggle_route(
        "xiaomi",
        "non-reasoning-model",
        "openai-compatible",
        &[stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE],
        Some(false),
    )
    .await?;

    assert!(route.supported_thinking_levels.is_empty());
    assert!(
        route.targets[0]
            .thinking_level_map
            .iter()
            .all(|row| row.control.is_hidden())
    );
    Ok(())
}

#[tokio::test]
async fn unknown_compatible_provider_still_rejects_submitted_toggle_controls() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        },
        std::sync::Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await
    .expect("gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Unknown Compatible Route Test Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: "openai-compatible".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "route-fixture-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("provider");
    admin
        .create_manual_provider_model(
            &provider.id,
            "custom-toggle-model",
            CreateManualProviderModel {
                template_id: None,
                metadata: json!({
                    "id": "custom-toggle-model",
                    "reasoning_options": [{"type": "toggle"}]
                }),
            },
        )
        .await
        .expect("provider model");

    use stravia_runtime_contract::thinking::TargetThinkingControl;
    let error = admin
        .create_model(CreateRoute {
            model_id: "submitted-toggle-route".into(),
            display_name: None,
            balance: None,

            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("custom-toggle-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: vec![
                    crate::thinking::ThinkingLevelMapping {
                        level: ThinkingLevel::Off,
                        control: TargetThinkingControl::Disabled,
                        source: ThinkingMappingSource::Generated,
                    },
                    crate::thinking::ThinkingLevelMapping {
                        level: ThinkingLevel::Medium,
                        control: TargetThinkingControl::Enabled,
                        source: ThinkingMappingSource::Generated,
                    },
                ],
            }],
            default_thinking_level: None,
        })
        .await
        .expect_err("explicitly submitted toggle controls must fail closed");
    let payload: serde_json::Value =
        serde_json::from_str(&error.to_string()).expect("coded thinking-control error");

    assert_eq!(payload["code"], "THINKING_CONTROL_UNREPRESENTABLE");
    assert_eq!(
        payload["params"]["levels"],
        serde_json::json!(["off", "medium"])
    );
    assert_eq!(
        payload["params"]["controls"],
        serde_json::json!(["disabled", "enabled"])
    );
    assert_eq!(
        payload["params"]["supported_controls"],
        serde_json::json!(["effort", "hidden"])
    );
}

#[tokio::test]
async fn gemini_accepts_generated_effort_maps() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        },
        std::sync::Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await?;
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Gemini Route Test Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("google-gemini".into()),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    admin
        .create_manual_provider_model(
            &provider.id,
            "gemini-effort-model",
            CreateManualProviderModel {
                template_id: None,
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
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("gemini-effort-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;

    assert_eq!(
        route.supported_thinking_levels,
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
                    template_id: None,
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

            targets: vec![
                CreateTarget {
                    provider_id: provider.id.clone(),
                    model: Some("wide-effort-model".into()),
                    enabled: true,
                    priority: Some(1),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                },
                CreateTarget {
                    provider_id: provider.id,
                    model: Some("narrow-effort-model".into()),
                    enabled: true,
                    priority: Some(1),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                },
            ],
            default_thinking_level: None,
        })
        .await?;

    assert_eq!(
        route.supported_thinking_levels,
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
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("anthropic-messages".into()),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    admin
        .create_manual_provider_model(
            &provider.id,
            "toggle-model",
            CreateManualProviderModel {
                template_id: None,
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
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("toggle-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;
    let mut targets = route_targets_for_update(&route);
    let high = targets[0]
        .thinking_level_map
        .iter_mut()
        .find(|row| row.level == ThinkingLevel::High)
        .expect("high row");
    high.control = stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
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
        updated.supported_thinking_levels,
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
        regenerated.supported_thinking_levels,
        vec![ThinkingLevel::Off, ThinkingLevel::Medium]
    );
    Ok(())
}

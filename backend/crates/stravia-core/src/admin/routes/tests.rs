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

fn modality_route_input(
    route_id: &str,
    provider_id: &str,
    modality: crate::db::models::RouteOutputModality,
    models: &[(&str, bool)],
) -> CreateRoute {
    serde_json::from_value(json!({
        "model_id": route_id,
        "output_modality": modality,
        "targets": models.iter().map(|(model, enabled)| json!({
            "provider_id": provider_id, "model": model, "enabled": enabled
        })).collect::<Vec<_>>()
    }))
    .expect("valid Route test input")
}

async fn register_output_model(
    admin: &AdminService,
    provider_id: &str,
    model_id: &str,
    metadata: serde_json::Value,
) -> anyhow::Result<()> {
    admin
        .create_manual_provider_model(
            provider_id,
            model_id,
            CreateManualProviderModel {
                template_id: None,
                metadata,
            },
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn route_candidates_and_writes_use_registered_outputs_not_read_fallbacks()
-> anyhow::Result<()> {
    use crate::db::models::RouteOutputModality::{Image, Text};
    let (_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for (id, metadata, expected) in [
        (
            "text",
            json!({"modalities":{"input":["image"],"output":["text"]}}),
            vec![Text],
        ),
        (
            "image",
            json!({"modalities":{"input":["text"],"output":["image"]}}),
            vec![Image],
        ),
        (
            "dual",
            json!({"modalities":{"input":["text"],"output":["text","image"]}}),
            vec![Text, Image],
        ),
        ("missing", json!({}), vec![]),
        ("null-modalities", json!({"modalities":null}), vec![]),
        (
            "missing-output",
            json!({"modalities":{"input":["text"]}}),
            vec![],
        ),
        (
            "empty",
            json!({"modalities":{"input":[],"output":[]}}),
            vec![],
        ),
        (
            "audio",
            json!({"modalities":{"input":["text"],"output":["audio"]}}),
            vec![],
        ),
    ] {
        register_output_model(&admin, &provider.id, id, metadata).await?;
        let detail = admin.get_provider_model(&provider.id, id).await?;
        assert_eq!(detail.route_output_modalities, expected, "{id}");
        let summary = admin
            .list_provider_models(&provider.id)
            .await?
            .models
            .into_iter()
            .find(|model| model.id == id)
            .expect("registered model");
        assert_eq!(summary.route_output_modalities, expected, "{id}");
        if id == "missing" {
            assert_eq!(detail.metadata.effective_modalities().output, vec!["text"]);
        }
        for modality in [Text, Image] {
            let result = admin
                .create_model(modality_route_input(
                    &format!("{id}-{}", modality.as_str()),
                    &provider.id,
                    modality,
                    &[(id, true)],
                ))
                .await;
            if expected.contains(&modality) {
                assert_eq!(result?.output_modality, Some(modality));
            } else {
                assert!(
                    result
                        .expect_err("unregistered output cannot bind")
                        .to_string()
                        .contains("ROUTE_OUTPUT_MODALITY_MISMATCH"),
                    "{id}"
                );
            }
        }
    }
    let prepared = admin
        .prepare_provider_model(&provider.id, "prepared", None)
        .await?;
    register_output_model(
        &admin,
        &provider.id,
        "prepared",
        prepared.metadata.to_value()?,
    )
    .await?;
    admin
        .create_model(modality_route_input(
            "prepared",
            &provider.id,
            Text,
            &[("prepared", true)],
        ))
        .await?;
    let detail = admin.get_provider_model(&provider.id, "missing").await?;
    admin
        .update_provider_model_selection(
            &provider.id,
            "missing",
            UpdateProviderModelSelection {
                policy: ProviderModelSelectionPolicy::ForceEnabled,
                revision: detail.revision,
            },
        )
        .await?;
    assert!(
        admin
            .get_provider_model(&provider.id, "missing")
            .await?
            .route_output_modalities
            .is_empty()
    );
    let error = admin
        .create_model(modality_route_input(
            "forced-unknown",
            &provider.id,
            Text,
            &[("missing", true)],
        ))
        .await
        .expect_err("ForceEnabled cannot invent an output declaration");
    assert!(error.to_string().contains("ROUTE_OUTPUT_MODALITY_MISMATCH"));
    Ok(())
}

#[tokio::test]
async fn route_modality_switch_validates_disabled_targets_and_is_atomic() -> anyhow::Result<()> {
    use crate::db::models::RouteOutputModality::{Image, Text};
    let (_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for (id, outputs) in [
        ("image", json!(["image"])),
        ("dual", json!(["text", "image"])),
    ] {
        register_output_model(
            &admin,
            &provider.id,
            id,
            json!({"modalities":{"output":outputs}}),
        )
        .await?;
    }
    let original = admin
        .create_model(modality_route_input(
            "switch",
            &provider.id,
            Text,
            &[("dual", true), ("upstream-model", false)],
        ))
        .await?;
    let error = admin
        .update_model(
            "switch",
            UpdateRoute {
                output_modality: Some(Image),
                ..Default::default()
            },
        )
        .await
        .expect_err("disabled incompatible Target must block switch");
    assert!(error.to_string().contains("ROUTE_OUTPUT_MODALITY_MISMATCH"));
    let unchanged = admin.get_model("switch").await?;
    assert_eq!(
        serde_json::to_value(&unchanged)?,
        serde_json::to_value(&original)?
    );
    let retained_dual_id = original
        .targets
        .iter()
        .find(|target| target.model().as_str() == "dual")
        .expect("dual Target")
        .id
        .clone();
    let replacement = modality_route_input(
        "switch",
        &provider.id,
        Image,
        &[("dual", true), ("image", false)],
    );
    let switched = admin
        .update_model(
            "switch",
            UpdateRoute {
                output_modality: Some(Image),
                targets: Some(replacement.targets),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(switched.id, original.id);
    assert_eq!(switched.output_modality, Some(Image));
    assert_eq!(
        switched
            .targets
            .iter()
            .find(|target| target.model().as_str() == "dual")
            .expect("retained dual Target")
            .id,
        retained_dual_id
    );
    Ok(())
}

#[tokio::test]
async fn new_target_with_same_model_suffix_must_match_route_output() -> anyhow::Result<()> {
    use crate::db::models::RouteOutputModality::Text;
    let (_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for (model, outputs) in [("a/model", json!(["text"])), ("b/model", json!(["image"]))] {
        register_output_model(
            &admin,
            &provider.id,
            model,
            json!({"modalities":{"output":outputs}}),
        )
        .await?;
    }
    let original = admin
        .create_model(modality_route_input(
            "same-suffix",
            &provider.id,
            Text,
            &[("a/model", true)],
        ))
        .await?;
    let mut proposed = route_targets_for_update(&original);
    proposed
        .extend(modality_route_input("unused", &provider.id, Text, &[("b/model", true)]).targets);
    let error = admin
        .update_model(
            "same-suffix",
            UpdateRoute {
                targets: Some(proposed),
                ..Default::default()
            },
        )
        .await
        .expect_err("a distinct registered model must not inherit a retained Target exemption");
    assert!(error.to_string().contains("ROUTE_OUTPUT_MODALITY_MISMATCH"));
    let persisted = admin.get_model("same-suffix").await?;
    assert_eq!(persisted.output_modality, Some(Text));
    assert_eq!(persisted.targets.len(), 1);
    assert_eq!(persisted.targets[0].id, original.targets[0].id);
    assert_eq!(
        persisted.targets[0].provider_id(),
        original.targets[0].provider_id()
    );
    assert_eq!(persisted.targets[0].model().as_str(), "a/model");
    assert!(persisted.targets[0].enabled);
    Ok(())
}

#[tokio::test]
async fn unchanged_mismatch_is_retained_but_new_disabled_mismatch_is_rejected() -> anyhow::Result<()>
{
    use crate::db::models::RouteOutputModality::Text;
    let (_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let route = admin
        .create_model(modality_route_input(
            "drift",
            &provider.id,
            Text,
            &[("upstream-model", true)],
        ))
        .await?;
    let detail = admin
        .get_provider_model(&provider.id, "upstream-model")
        .await?;
    admin
        .update_provider_model(
            &provider.id,
            "upstream-model",
            crate::provider_models::UpdateProviderModel {
                revision: detail.revision,
                changes: std::collections::BTreeMap::from([
                    ("modalities.input".to_string(), json!(["text"])),
                    ("modalities.output".to_string(), json!(["image"])),
                ]),
            },
        )
        .await?;
    let drifted = admin.get_model("drift").await?;
    assert_eq!(drifted.targets[0].id, route.targets[0].id);
    assert!(drifted.targets[0].output_modality_mismatch);
    let saved = admin
        .update_model(
            "drift",
            UpdateRoute {
                targets: Some(route_targets_for_update(&drifted)),
                ..Default::default()
            },
        )
        .await?;
    assert!(saved.targets[0].output_modality_mismatch);
    register_output_model(
        &admin,
        &provider.id,
        "image",
        json!({"modalities":{"output":["image"]}}),
    )
    .await?;
    let mut proposed = route_targets_for_update(&saved);
    proposed
        .extend(modality_route_input("unused", &provider.id, Text, &[("image", false)]).targets);
    let error = admin
        .update_model(
            "drift",
            UpdateRoute {
                targets: Some(proposed),
                ..Default::default()
            },
        )
        .await
        .expect_err("new disabled Target must still match");
    assert!(error.to_string().contains("ROUTE_OUTPUT_MODALITY_MISMATCH"));
    assert_eq!(admin.get_model("drift").await?.targets.len(), 1);
    let detail = admin
        .get_provider_model(&provider.id, "upstream-model")
        .await?;
    admin
        .update_provider_model(
            &provider.id,
            "upstream-model",
            crate::provider_models::UpdateProviderModel {
                revision: detail.revision,
                changes: std::collections::BTreeMap::from([(
                    "modalities.output".to_string(),
                    json!(["text"]),
                )]),
            },
        )
        .await?;
    let repaired = admin.get_model("drift").await?;
    assert_eq!(repaired.targets[0].id, route.targets[0].id);
    assert!(!repaired.targets[0].output_modality_mismatch);
    Ok(())
}

#[tokio::test]
async fn one_click_modality_selection_and_duplicate_binding_revalidate_outputs()
-> anyhow::Result<()> {
    use crate::db::models::RouteOutputModality::{Image, Text};
    let (_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let missing_route = admin
        .bind_route(BindRouteInput {
            route_id: Some("not-created".into()),
            provider_id: provider.id.clone(),
            provider_model_id: "upstream-model".into(),
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
        })
        .await
        .expect_err("explicit binding cannot implicitly create a Route modality");
    assert!(missing_route.to_string().contains("ROUTE_NOT_FOUND"));
    for (id, outputs, expected) in [
        ("text", json!(["text"]), Some(Text)),
        ("dual", json!(["image", "text"]), Some(Text)),
        ("image", json!(["image"]), Some(Image)),
        ("audio", json!(["audio"]), None),
    ] {
        register_output_model(
            &admin,
            &provider.id,
            id,
            json!({"modalities":{"output":outputs}}),
        )
        .await?;
        let result = admin
            .bind_route(BindRouteInput {
                route_id: None,
                provider_id: provider.id.clone(),
                provider_model_id: id.into(),
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
            })
            .await;
        if let Some(expected) = expected {
            assert_eq!(result?.output_modality, Some(expected));
        } else {
            assert!(
                result
                    .expect_err("unsupported output must not connect")
                    .to_string()
                    .contains("ROUTE_OUTPUT_MODALITY_UNREGISTERED")
            );
        }
    }
    let detail = admin.get_provider_model(&provider.id, "text").await?;
    admin
        .update_provider_model(
            &provider.id,
            "text",
            crate::provider_models::UpdateProviderModel {
                revision: detail.revision,
                changes: std::collections::BTreeMap::from([(
                    "modalities.output".to_string(),
                    json!(["image"]),
                )]),
            },
        )
        .await?;
    let error = admin
        .bind_route(BindRouteInput {
            route_id: None,
            provider_id: provider.id.clone(),
            provider_model_id: "text".into(),
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
        })
        .await
        .expect_err("duplicate binding cannot bypass output drift");
    assert!(error.to_string().contains("ROUTE_OUTPUT_MODALITY_MISMATCH"));
    assert_eq!(admin.get_model("text").await?.output_modality, Some(Text));
    let connected = admin
        .bind_route(BindRouteInput {
            route_id: Some("image".into()),
            provider_id: provider.id.clone(),
            provider_model_id: "dual".into(),
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
        })
        .await?;
    assert_eq!(connected.output_modality, Some(Image));
    let error = admin
        .bind_route(BindRouteInput {
            route_id: Some("image".into()),
            provider_id: provider.id.clone(),
            provider_model_id: "upstream-model".into(),
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
        })
        .await
        .expect_err("explicit bind must obey existing image route modality");
    assert!(error.to_string().contains("ROUTE_OUTPUT_MODALITY_MISMATCH"));
    assert_eq!(admin.get_model("image").await?.targets.len(), 2);
    assert!(
        admin
            .list_models()
            .await?
            .iter()
            .all(|route| route.model_id.as_str() != "audio")
    );
    Ok(())
}

#[tokio::test]
async fn unconfigured_route_rejects_new_same_suffix_target_but_retains_legacy_alias()
-> anyhow::Result<()> {
    use crate::db::models::RouteOutputModality::Text;
    let (_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for model in ["a/model", "b/model"] {
        register_output_model(
            &admin,
            &provider.id,
            model,
            json!({"modalities":{"output":["text"]}}),
        )
        .await?;
    }
    let original = gateway
        .storage
        .routes()
        .put(PutRoute {
            id: None,
            model_id: "unconfigured-same-suffix".into(),
            output_modality: None,
            display_name: None,
            selection_strategy: "traffic_equalization".into(),
            is_enabled: true,
            targets: Some(
                modality_route_input("unused", &provider.id, Text, &[("a/model", true)]).targets,
            ),
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    let saved = admin
        .update_model(
            "unconfigured-same-suffix",
            UpdateRoute {
                display_name: Some(Some("Retained legacy Target".into())),
                targets: Some(route_targets_for_update(&original)),
                ..Default::default()
            },
        )
        .await?;
    assert!(saved.output_modality.is_none());
    assert_eq!(saved.targets[0].id, original.targets[0].id);
    assert_eq!(saved.targets[0].model().as_str(), "a/model");
    let mut proposed = route_targets_for_update(&saved);
    proposed
        .extend(modality_route_input("unused", &provider.id, Text, &[("b/model", true)]).targets);
    let error = admin
        .update_model(
            "unconfigured-same-suffix",
            UpdateRoute {
                targets: Some(proposed),
                ..Default::default()
            },
        )
        .await
        .expect_err("a distinct same-suffix Target requires an explicit Route modality");
    assert!(
        error
            .to_string()
            .contains("ROUTE_OUTPUT_MODALITY_UNCONFIGURED")
    );
    let persisted = admin.get_model("unconfigured-same-suffix").await?;
    assert!(persisted.output_modality.is_none());
    assert_eq!(persisted.targets.len(), 1);
    assert_eq!(persisted.targets[0].id, original.targets[0].id);
    assert_eq!(persisted.targets[0].model().as_str(), "a/model");
    Ok(())
}

#[tokio::test]
async fn historical_route_requires_explicit_modality_and_disabled_image_reference_blocks_text()
-> anyhow::Result<()> {
    use crate::db::models::RouteOutputModality::{Image, Text};
    let (_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    register_output_model(
        &admin,
        &provider.id,
        "dual",
        json!({"modalities":{"output":["text","image"]}}),
    )
    .await?;
    let route = gateway
        .storage
        .routes()
        .put(PutRoute {
            id: None,
            model_id: "historical".into(),
            output_modality: None,
            display_name: Some("Legacy".into()),
            selection_strategy: "traffic_equalization".into(),
            is_enabled: false,
            targets: Some(
                modality_route_input("unused", &provider.id, Text, &[("dual", true)]).targets,
            ),
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    for update in [
        UpdateRoute {
            is_enabled: Some(true),
            ..Default::default()
        },
        UpdateRoute {
            display_name: Some(Some("Still historical".into())),
            ..Default::default()
        },
    ] {
        assert!(
            admin
                .update_model("historical", update)
                .await?
                .output_modality
                .is_none()
        );
    }
    let error = admin
        .bind_route(BindRouteInput {
            route_id: Some("historical".into()),
            provider_id: provider.id.clone(),
            provider_model_id: "dual".into(),
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
        })
        .await
        .expect_err("historical duplicate binding cannot recover route");
    assert!(
        error
            .to_string()
            .contains("ROUTE_OUTPUT_MODALITY_UNCONFIGURED")
    );
    gateway
        .storage
        .settings()
        .set(
            crate::media_generation::config::SETTINGS_KEY,
            &json!({"enabled":false,"image":{"route_id":"historical"}}).to_string(),
        )
        .await?;
    assert!(admin.get_model("historical").await?.image_generation_bound);
    let error = admin
        .update_model(
            "historical",
            UpdateRoute {
                output_modality: Some(Text),
                ..Default::default()
            },
        )
        .await
        .expect_err("disabled image generation still retains its reference");
    assert!(error.to_string().contains("ROUTE_IMAGE_GENERATION_BOUND"));
    let recovered = admin
        .update_model(
            "historical",
            UpdateRoute {
                output_modality: Some(Image),
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(recovered.id, route.id);
    assert!(!recovered.is_enabled);
    assert_eq!(recovered.targets[0].id, route.targets[0].id);
    admin
        .update_media_generation_config(crate::media_generation::MediaGenerationConfig {
            enabled: false,
            image: crate::media_generation::ImageGenerationConfig { route_id: None },
        })
        .await?;
    let switched = admin
        .update_model(
            "historical",
            UpdateRoute {
                output_modality: Some(Text),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(switched.output_modality, Some(Text));
    assert!(!switched.image_generation_bound);
    Ok(())
}

#[tokio::test]
async fn missing_model_modalities_default_to_text_without_overriding_declarations()
-> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let prepared = admin
        .prepare_provider_model(&provider.id, "metadata-free-model", None)
        .await?;
    assert_eq!(
        prepared.snapshot_state,
        crate::provider_models::SnapshotState::Unregistered
    );
    assert_eq!(
        prepared.metadata.to_value()?["modalities"],
        json!({"input": ["text"], "output": ["text"]})
    );
    assert!(prepared.metadata.limit.is_none());
    assert!(prepared.metadata.reasoning_efforts.is_none());

    for (id, metadata, expected) in [
        (
            "metadata-free-model",
            json!({}),
            json!({"input": ["text"], "output": ["text"]}),
        ),
        (
            "null-modalities-model",
            json!({"modalities": null}),
            json!({"input": ["text"], "output": ["text"]}),
        ),
        (
            "audio-model",
            json!({"modalities": {"input": ["audio"], "output": ["audio"]}}),
            json!({"input": ["audio"], "output": ["audio"]}),
        ),
        (
            "empty-modalities-model",
            json!({"modalities": {"input": [], "output": []}}),
            json!({"input": [], "output": []}),
        ),
    ] {
        admin
            .create_manual_provider_model(
                &provider.id,
                id,
                CreateManualProviderModel {
                    template_id: None,
                    metadata,
                },
            )
            .await?;
        let detail = admin.get_provider_model(&provider.id, id).await?;
        assert_eq!(detail.metadata.to_value()?["modalities"], expected);
        let summary = admin
            .list_provider_models(&provider.id)
            .await?
            .models
            .into_iter()
            .find(|model| model.id == id)
            .expect("saved model");
        assert_eq!(
            serde_json::to_value(summary.specification)?["modalities"],
            expected
        );
        let capabilities = admin.get_model_capabilities(&provider.id, id).await?;
        assert_eq!(json!(capabilities.input_modalities), expected["input"]);
        assert_eq!(json!(capabilities.output_modalities), expected["output"]);
    }
    Ok(())
}

#[test]
fn route_creation_requires_an_explicit_supported_output_modality() {
    for modality in [
        None,
        Some(json!(null)),
        Some(json!("")),
        Some(json!("audio")),
    ] {
        let mut input = json!({
            "model_id": "client-model",
            "targets": [{"provider_id": "provider", "model": "upstream-model"}]
        });
        if let Some(modality) = modality {
            input["output_modality"] = modality;
        }
        assert!(serde_json::from_value::<CreateRoute>(input).is_err());
    }
}

#[test]
fn route_output_modality_updates_reject_null_and_unknown_without_defaulting() {
    let omitted = serde_json::from_value::<UpdateRoute>(json!({})).expect("ordinary patch");
    assert!(omitted.output_modality.is_none());
    for value in [json!(null), json!(""), json!("audio")] {
        assert!(serde_json::from_value::<UpdateRoute>(json!({"output_modality":value})).is_err());
    }
    assert_eq!(
        serde_json::from_value::<UpdateRoute>(json!({"output_modality":"image"}))
            .expect("explicit modality")
            .output_modality,
        Some(crate::db::models::RouteOutputModality::Image)
    );
}

#[test]
fn route_wire_inputs_require_targets_and_reject_legacy_fields() {
    let current = serde_json::from_value::<CreateRoute>(json!({
        "model_id": "client-model",
        "output_modality": "text",
        "display_name": "Friendly model",
        "targets": [{"provider_id": "provider", "model": "upstream-model"}]
    }));
    assert!(current.is_ok());

    let missing_model = serde_json::from_value::<CreateRoute>(json!({
        "model_id": "research",
        "targets": [{"provider_id": "provider", "model": null}]
    }));
    assert!(missing_model.is_err());
    assert!(
        serde_json::from_value::<CreateRoute>(json!({
            "model_id": "research",
            "targets": [{"provider_id": "provider"}]
        }))
        .is_err()
    );

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

    let missing_model = serde_json::from_value::<CreateTarget>(json!({
        "provider_id": "research-provider",
        "model": null
    }));
    let blank = serde_json::from_value::<CreateTarget>(json!({
        "provider_id": "provider",
        "model": "   "
    }))
    .expect("structurally valid blank Target");

    assert!(enabled.enabled);
    assert!(!disabled.enabled);
    assert!(missing_model.is_err());
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
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "default-thinking-route".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "upstream-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: Some(ThinkingLevel::High),
            billing: None,
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
                    metadata: json!({"id": model, "name": model, "modalities": {"input": ["text"], "output": ["text"]}}),
                },
            )
            .await?;
    }
    let create_target = |model: &str, priority: i32| CreateTarget {
        enabled: true,
        provider_id: provider.id.clone(),
        model: model.into(),
        priority: Some(priority),
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };
    let route = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "layered-route".into(),
            display_name: None,
            balance: None,

            targets: vec![
                create_target("upstream-model", 100_000),
                create_target("second-model", 0),
                create_target("third-model", 0),
            ],
            default_thinking_level: None,
            billing: None,
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
                output_modality: crate::db::models::RouteOutputModality::Text,
                model_id: format!("signed-priority-{valid}"),
                display_name: None,
                balance: Some("traffic_equalization".into()),

                targets: vec![create_target("upstream-model", valid)],
                default_thinking_level: None,
                billing: None,
            })
            .await?;
        assert_eq!(route.targets[0].priority, valid);
    }
    let invalid_strategy = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "invalid-strategy".into(),
            display_name: None,
            balance: Some("weighted_random".into()),

            targets: vec![create_target("upstream-model", 0)],
            default_thinking_level: None,
            billing: None,
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
                metadata: json!({"id": "standby-model", "name": "Standby", "modalities": {"input": ["text"], "output": ["text"]}}),
            },
        )
        .await?;
    let target = |model: &str, enabled: bool| CreateTarget {
        provider_id: provider.id.clone(),
        model: model.into(),
        enabled,
        priority: Some(if enabled { -1 } else { i32::MAX }),
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };

    let route = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "enabled-and-standby".into(),
            display_name: None,
            balance: None,

            targets: vec![
                target("upstream-model", true),
                target("standby-model", false),
            ],
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    assert_eq!(route.targets.len(), 2);
    assert!(
        route
            .targets
            .iter()
            .find(|target| target.model().as_str() == "upstream-model")
            .is_some_and(|target| target.enabled)
    );
    assert!(
        route
            .targets
            .iter()
            .find(|target| target.model().as_str() == "standby-model")
            .is_some_and(|target| !target.enabled)
    );
    assert_eq!(
        route.primary_target().map(|target| target.model().as_str()),
        Some("upstream-model")
    );

    let error = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "all-disabled".into(),
            display_name: None,
            balance: None,

            targets: vec![target("upstream-model", false)],
            default_thinking_level: None,
            billing: None,
        })
        .await
        .expect_err("Route without an enabled Target must be rejected");
    assert!(error.to_string().contains("enabled Target"));
    Ok(())
}

#[tokio::test]
async fn listed_routes_project_provider_model_names_for_every_target() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    admin
        .create_manual_provider_model(
            &provider.id,
            "standby-model",
            CreateManualProviderModel {
                template_id: None,
                metadata: json!({"id": "standby-model", "name": "Standby", "modalities": {"input": ["text"], "output": ["text"]}}),
            },
        )
        .await?;
    let target = |model: &str, enabled: bool| CreateTarget {
        provider_id: provider.id.clone(),
        model: model.into(),
        enabled,
        priority: None,
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };
    admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "named-targets".into(),
            display_name: None,
            balance: None,
            targets: vec![
                target("upstream-model", true),
                target("standby-model", false),
            ],
            default_thinking_level: None,
            billing: None,
        })
        .await?;

    let routes = serde_json::to_value(admin.list_models().await?)?;
    let route = routes
        .as_array()
        .and_then(|routes| {
            routes
                .iter()
                .find(|route| route["model_id"] == "named-targets")
        })
        .expect("listed Route");
    let name_of = |model: &str| {
        route["targets"]
            .as_array()
            .and_then(|targets| targets.iter().find(|target| target["model"] == model))
            .map(|target| target["model_name"].clone())
    };
    assert_eq!(name_of("upstream-model"), Some(json!("Upstream Model")));
    assert_eq!(name_of("standby-model"), Some(json!("Standby")));
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
    assert_eq!(second.targets[0].model().as_str(), "upstream-model");
    assert!(second.targets[0].enabled);
    assert_eq!(second.context_window, Some(200_000));
    assert!(second.supports_image_input);
    Ok(())
}

#[tokio::test]
async fn route_ids_are_compared_exactly_when_binding() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin.gw);

    for route_id in ["CaseRoute", "caseroute"] {
        admin
            .create_model(modality_route_input(
                route_id,
                &provider.id,
                crate::db::models::RouteOutputModality::Text,
                &[("upstream-model", true)],
            ))
            .await?;
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
        model: model.into(),
        priority: None,
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };

    // Target model 与清单 ID 仅在最右段与大小写上一致时也能命中
    let route = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "glm-4.6".into(),
            display_name: None,
            balance: None,

            targets: vec![create_target("GLM-4.6")],
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    assert_eq!(route.targets.len(), 1);
    assert_eq!(route.targets[0].model().as_str(), "GLM-4.6");
    assert_eq!(
        route.targets[0].route_output_modalities,
        vec![crate::db::models::RouteOutputModality::Text]
    );
    // 能力元数据经宽松匹配解析成功
    assert_eq!(route.context_window, Some(131_072));
    assert!(!route.supports_image_input);

    let mut historical_target = create_target("GLM-4.6");
    historical_target.priority = Some(17);
    historical_target.first_token_timeout_ms = Some(45_000);
    historical_target.target_retry_budget = Some(2);
    historical_target.target_cooldown_ms = Some(91_000);
    let historical = gateway
        .storage
        .routes()
        .put(PutRoute {
            id: None,
            model_id: "historical-glm".into(),
            output_modality: None,
            display_name: None,
            selection_strategy: "traffic_equalization".into(),
            is_enabled: true,
            targets: Some(vec![historical_target]),
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    let projected = admin.get_model("historical-glm").await?;
    assert!(projected.targets[0].output_modality_mismatch);
    assert_eq!(
        projected.targets[0].route_output_modalities,
        vec![crate::db::models::RouteOutputModality::Text]
    );
    let repaired = admin
        .update_model(
            "historical-glm",
            UpdateRoute {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(repaired.targets.len(), 1);
    let retained = &repaired.targets[0];
    assert_eq!(retained.id, historical.targets[0].id);
    assert_eq!(retained.provider_id(), historical.targets[0].provider_id());
    assert_eq!(retained.model().as_str(), "GLM-4.6");
    assert!(retained.enabled);
    assert_eq!(retained.priority, 17);
    assert_eq!(retained.first_token_timeout_ms, 45_000);
    assert_eq!(retained.target_retry_budget, 2);
    assert_eq!(retained.target_cooldown_ms, 91_000);
    assert_eq!(
        retained.thinking_level_map,
        historical.targets[0].thinking_level_map
    );
    assert!(!retained.output_modality_mismatch);
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
                    metadata: json!({ "id": model, "name": model, "modalities": {"input": ["text"], "output": ["text"]} }),
                },
            )
            .await?;
    }
    let create_target = |model: &str| CreateTarget {
        enabled: true,
        provider_id: provider.id.clone(),
        model: model.into(),
        priority: None,
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };
    let input = |target: CreateTarget| CreateRoute {
        output_modality: crate::db::models::RouteOutputModality::Text,
        model_id: "gpt-4o-route".into(),
        display_name: None,
        balance: None,

        targets: vec![target],
        default_thinking_level: None,
        billing: None,
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
    admin
        .create_model(modality_route_input(
            "cased-route",
            &provider.id,
            crate::db::models::RouteOutputModality::Text,
            &[("upstream-model", true)],
        ))
        .await?;

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
    assert_eq!(route.targets[0].model().as_str(), "upstream-model");
    Ok(())
}

#[tokio::test]
async fn route_get_uses_exact_route_id_and_never_storage_id() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    let routes = RouteModule::new(&admin.gw);
    let route = admin
        .create_model(modality_route_input(
            "ExactRoute",
            &provider.id,
            crate::db::models::RouteOutputModality::Text,
            &[("upstream-model", true)],
        ))
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
    let route = admin
        .create_model(modality_route_input(
            "ExactRoute",
            &provider.id,
            crate::db::models::RouteOutputModality::Text,
            &[("upstream-model", true)],
        ))
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
        output_modality: crate::db::models::RouteOutputModality::Text,
        model_id: model_id.into(),
        display_name: Some("  Shared label  ".into()),
        balance: Some("priority".into()),
        targets: vec![CreateTarget {
            provider_id: provider.id.clone(),
            model: "upstream-model".into(),
            enabled: true,
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
            thinking_level_map: Vec::new(),
        }],
        default_thinking_level: None,
        billing: None,
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
    assert!(
        admin
            .get_provider_model(&provider.id, "upstream-model")
            .await?
            .route_output_modalities
            .is_empty()
    );
    assert!(
        admin
            .list_provider_models(&provider.id)
            .await?
            .models
            .into_iter()
            .find(|model| model.id == "upstream-model")
            .expect("registered model")
            .route_output_modalities
            .is_empty()
    );

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
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "manual-route".into(),
            display_name: None,
            balance: None,

            targets: vec![CreateTarget {
                provider_id: provider.id,
                model: "upstream-model".into(),
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
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "missing-model-route".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "missing-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
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
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "reasoning_efforts": ["none", "low", "high", "max"]
                }),
            },
        )
        .await?;
    let route = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "thinking-route".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "effort-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
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
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "reasoning_efforts": ["none", "max"]
                }),
            },
        )
        .await?;

    let route = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "max-effort-route".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "max-effort-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
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
    submit_toggle: bool,
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
                    "capabilities": capabilities,
                    "modalities": {"input": ["text"], "output": ["text"]},
                }),
            },
        )
        .await?;

    admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: model.into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: model.into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: if submit_toggle {
                    vec![
                        crate::thinking::ThinkingLevelMapping {
                            level: ThinkingLevel::Off,
                            control:
                                stravia_runtime_contract::thinking::TargetThinkingControl::Disabled,
                            source: ThinkingMappingSource::Overridden,
                        },
                        crate::thinking::ThinkingLevelMapping {
                            level: ThinkingLevel::Medium,
                            control:
                                stravia_runtime_contract::thinking::TargetThinkingControl::Enabled,
                            source: ThinkingMappingSource::Overridden,
                        },
                    ]
                } else {
                    Vec::new()
                },
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await
}

#[tokio::test]
async fn xiaomi_toggle_model_can_be_bound_over_openai_compatible() -> anyhow::Result<()> {
    let route = create_toggle_route("xiaomi", "mimo-v2.5", "openai-compatible", &[], true).await?;

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
        false,
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
        false,
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
    let generated = create_toggle_route(
        "openai-compatible",
        "declared-toggle-model",
        "openai-compatible",
        &[stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE],
        false,
    )
    .await?;
    assert!(generated.supported_thinking_levels.is_empty());
    assert!(
        generated.targets[0]
            .thinking_level_map
            .iter()
            .all(|row| row.control.is_hidden())
    );
    let route = create_toggle_route(
        "openai-compatible",
        "declared-toggle-model",
        "openai-compatible",
        &[stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE],
        true,
    )
    .await?;

    assert_eq!(
        route.supported_thinking_levels,
        vec![ThinkingLevel::Off, ThinkingLevel::Medium]
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
                    "modalities": {"input": ["text"], "output": ["text"]}
                }),
            },
        )
        .await
        .expect("provider model");

    use stravia_runtime_contract::thinking::TargetThinkingControl;
    let error = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "submitted-toggle-route".into(),
            display_name: None,
            balance: None,

            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "custom-toggle-model".into(),
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
            billing: None,
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
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "reasoning_efforts": ["low", "high"]
                }),
            },
        )
        .await?;

    let route = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "gemini-thinking-route".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "gemini-effort-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await?;

    assert_eq!(
        route.supported_thinking_levels,
        vec![ThinkingLevel::Low, ThinkingLevel::High]
    );
    Ok(())
}

#[tokio::test]
async fn supported_levels_are_the_union_of_enabled_targets() -> anyhow::Result<()> {
    let (_data_dir, gateway, provider) = route_fixture().await?;
    let admin = gateway.admin();
    for (model, values, context, input_modalities) in [
        (
            "wide-effort-model",
            vec!["none", "low", "high", "max"],
            200_000,
            vec!["text", "image"],
        ),
        (
            "narrow-effort-model",
            vec!["low", "high"],
            128_000,
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
                        "reasoning_efforts": values,
                        "limit": {
                            "context": context
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
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "union-route".into(),
            display_name: None,
            balance: None,

            targets: vec![
                CreateTarget {
                    provider_id: provider.id.clone(),
                    model: "wide-effort-model".into(),
                    enabled: true,
                    priority: Some(1),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                },
                CreateTarget {
                    provider_id: provider.id,
                    model: "narrow-effort-model".into(),
                    enabled: true,
                    priority: Some(1),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                },
            ],
            default_thinking_level: None,
            billing: None,
        })
        .await?;

    assert_eq!(
        route.supported_thinking_levels,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ]
    );
    assert_eq!(route.context_window, Some(128_000));
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
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "reasoning_efforts": ["none", "medium"]
                }),
            },
        )
        .await?;
    let route = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "toggle-route".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "toggle-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
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

    assert_eq!(
        updated.targets[0]
            .thinking_level_map
            .iter()
            .find(|row| row.level == ThinkingLevel::High)
            .expect("high row")
            .source,
        ThinkingMappingSource::Overridden
    );
    let regenerated = admin
        .regenerate_target_thinking_map(&route.model_id, &updated.targets[0].id)
        .await?;
    assert_eq!(
        regenerated.supported_thinking_levels,
        vec![ThinkingLevel::Off, ThinkingLevel::Medium]
    );
    assert!(
        regenerated.targets[0]
            .thinking_level_map
            .iter()
            .all(|row| row.source == ThinkingMappingSource::Generated)
    );
    Ok(())
}

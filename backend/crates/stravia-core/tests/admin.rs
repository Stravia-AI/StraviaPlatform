use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use stravia_core::Gateway;
use stravia_core::admin::CopyProviderOptions;
use stravia_core::config::GatewayConfig;
use stravia_core::db::models::*;
use stravia_core::provider_catalog::{
    CatalogError, CatalogSource, CatalogVersion, ProviderCatalog,
};
use stravia_core::provider_models::{
    CreateManualProviderModel, ProviderModelSelectionPolicy, UpdateProviderModel,
    UpdateProviderModelSelection,
};
use stravia_core::storage::Storage as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};

use uuid::Uuid;

const FAR_FUTURE_RFC3339: &str = "2099-01-01T00:00:00Z";
const CODEX_RUNTIME_URL: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_SCOPE: &[u8] = br#"{
  "gpt-5.4": {
    "id": "gpt-5.4",
    "name": "GPT-5.4",
    "description": "Catalog snapshot description",
    "family": "gpt",
    "tool_call": true,
    "temperature": true,
    "modalities": { "input": ["text"], "output": ["text"] },
    "limit": { "context": 272000, "output": 128000 },
    "cost": { "input": 2.5, "output": 15.0 }
  }
}"#;

struct TestCatalogSource;

#[async_trait]
impl CatalogSource for TestCatalogSource {
    async fn fetch_version(&self) -> anyhow::Result<CatalogVersion> {
        Ok(CatalogVersion {
            revision: "bootstrap".to_string(),
            generated_at: "test".to_string(),
        })
    }

    async fn fetch_providers(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("global indexes are not used by this test catalog")
    }

    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("global indexes are not used by this test catalog")
    }

    async fn fetch_provider_scope(&self, provider_id: &str) -> anyhow::Result<Vec<u8>> {
        match provider_id {
            "openai" | "google" | "minimax" => Ok(OPENAI_SCOPE.to_vec()),
            _ => anyhow::bail!("test catalog has no scope for {provider_id}"),
        }
    }

    async fn fetch_logo(&self, _provider_id: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("logos are not used by this test catalog")
    }
}

#[tokio::test]
async fn catalog_provider_creation_resolves_runtime_fields_in_core() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let catalog = gw.provider_catalog.providers().await;
    let openai = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "openai")
        .expect("built-in Catalog must contain OpenAI");
    let channel = openai
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .expect("OpenAI must expose the default channel");

    let provider = gw
        .admin()
        .create_provider(CreateProvider {
            name: None,
            source: ProviderSourceInput::Catalog {
                provider_id: openai.id.clone(),
                channel_id: channel.id.clone(),
                fingerprint: channel.fingerprint.clone(),
                base_url_override: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await?;

    assert_eq!(provider.name, "OpenAI");
    assert_eq!(provider.vendor.as_deref(), Some("openai"));
    assert_eq!(provider.protocol, "open-responses");
    assert_eq!(provider.base_url, "https://api.openai.com/v1");
    assert_eq!(provider.preset_key.as_deref(), Some("openai"));
    assert_eq!(provider.channel.as_deref(), Some("default"));
    assert_eq!(provider.models_source.as_deref(), Some("catalog"));

    let stale = gw
        .admin()
        .create_provider(CreateProvider {
            name: Some("stale".to_string()),
            source: ProviderSourceInput::Catalog {
                provider_id: openai.id.clone(),
                channel_id: channel.id.clone(),
                fingerprint: "stale-fingerprint".to_string(),
                base_url_override: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await
        .unwrap_err();
    assert!(stale.to_string().contains("refresh and select"));

    Ok(())
}

#[tokio::test]
async fn catalog_provider_creation_uses_npm_vendor_and_assembled_azure_url() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let groq = gw
        .admin()
        .create_provider(catalog_provider_input_for(&gw, "Groq catalog", "groq").await?)
        .await?;
    assert_eq!(groq.vendor.as_deref(), Some("groq"));
    assert_eq!(groq.preset_key.as_deref(), Some("groq"));
    assert_eq!(groq.base_url, "https://api.groq.com/openai/v1");

    let catalog = gw.provider_catalog.providers().await;
    let azure = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "azure-cognitive-services")
        .expect("built-in Catalog must contain Azure Cognitive Services");
    let channel = azure
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .expect("Azure must expose its default channel");
    let azure = gw
        .admin()
        .create_provider(CreateProvider {
            name: Some("Azure catalog".to_string()),
            source: ProviderSourceInput::Catalog {
                provider_id: azure.id.clone(),
                channel_id: channel.id.clone(),
                fingerprint: channel.fingerprint.clone(),
                base_url_override: None,
            },
            credential: ProviderCredentialInput::Fields {
                values: std::collections::BTreeMap::from([
                    ("resourceName".to_string(), "MyRes".to_string()),
                    ("apiKey".to_string(), "k".to_string()),
                ]),
            },
            use_proxy: false,
        })
        .await?;
    assert_eq!(azure.vendor.as_deref(), Some("azure"));
    assert_eq!(azure.base_url, "https://myres.openai.azure.com/openai/v1");
    assert_eq!(azure.api_key, "k");
    assert_eq!(
        azure.adapter_credentials,
        r#"{"apiKey":"k","resourceName":"MyRes"}"#
    );
    let azure = gw
        .admin()
        .update_provider(
            &azure.id,
            UpdateProvider {
                base_url: Some(azure.base_url),
                adapter_credentials: Some(std::collections::BTreeMap::from([
                    ("resourceName".to_string(), "next-resource".to_string()),
                    ("apiKey".to_string(), "next-key".to_string()),
                ])),
                ..UpdateProvider::default()
            },
        )
        .await?;
    assert_eq!(
        azure.base_url,
        "https://next-resource.openai.azure.com/openai/v1"
    );

    let azure = gw
        .admin()
        .update_provider(
            &azure.id,
            UpdateProvider {
                base_url: Some("https://azure-proxy.example.test/openai/v1".to_string()),
                adapter_credentials: Some(std::collections::BTreeMap::from([
                    ("resourceName".to_string(), "ignored-resource".to_string()),
                    ("apiKey".to_string(), "latest-key".to_string()),
                ])),
                ..UpdateProvider::default()
            },
        )
        .await?;
    assert_eq!(azure.base_url, "https://azure-proxy.example.test/openai/v1");
    let azure = gw
        .admin()
        .update_provider(
            &azure.id,
            UpdateProvider {
                adapter_credentials: Some(std::collections::BTreeMap::from([
                    ("resourceName".to_string(), "still-ignored".to_string()),
                    ("apiKey".to_string(), "final-key".to_string()),
                ])),
                ..UpdateProvider::default()
            },
        )
        .await?;
    assert_eq!(azure.base_url, "https://azure-proxy.example.test/openai/v1");

    let sap = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "sap-ai-core")
        .expect("built-in Catalog must contain SAP AI Core");
    let channel = sap
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .expect("SAP AI Core must expose its default channel");
    let sap = gw
        .admin()
        .create_provider(CreateProvider {
            name: Some("SAP AI Core catalog".to_string()),
            source: ProviderSourceInput::Catalog {
                provider_id: sap.id.clone(),
                channel_id: channel.id.clone(),
                fingerprint: channel.fingerprint.clone(),
                base_url_override: None,
            },
            credential: ProviderCredentialInput::Fields {
                values: std::collections::BTreeMap::from([
                    (
                        "deploymentUrl".to_string(),
                        "https://deployment.example.test".to_string(),
                    ),
                    (
                        "tokenUrl".to_string(),
                        "https://auth.example.test/oauth/token".to_string(),
                    ),
                    ("clientId".to_string(), "client-id".to_string()),
                    ("clientSecret".to_string(), "client-secret".to_string()),
                    ("resourceGroup".to_string(), "production".to_string()),
                ]),
            },
            use_proxy: false,
        })
        .await?;
    assert_eq!(sap.vendor.as_deref(), Some("sap-ai-core"));
    assert_eq!(sap.base_url, "https://deployment.example.test");
    assert_eq!(sap.api_key, "");
    assert_eq!(
        sap.adapter_credentials,
        r#"{"clientId":"client-id","clientSecret":"client-secret","deploymentUrl":"https://deployment.example.test","resourceGroup":"production","tokenUrl":"https://auth.example.test/oauth/token"}"#
    );
    Ok(())
}

#[tokio::test]
async fn provider_base_url_preview_matches_vendor_assembly() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let preview = gw.admin().preview_provider_base_url(
        "cloudflare-ai-gateway",
        std::collections::BTreeMap::from([
            ("accountId".to_string(), "account_1".to_string()),
            ("gatewayId".to_string(), "gateway_1".to_string()),
        ]),
        None,
    )?;

    assert_eq!(
        preview,
        "https://gateway.ai.cloudflare.com/v1/account_1/gateway_1/compat"
    );
    Ok(())
}

#[tokio::test]
async fn provider_models_persist_direct_edits_and_cost_rules() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let provider = gw
        .admin()
        .create_provider(catalog_provider_input_for(&gw, "snapshot-provider", "minimax").await?)
        .await?;

    let synced = gw.admin().sync_provider_models(&provider.id).await?;
    assert!(synced.added > 0);
    let listed = gw.admin().list_provider_models(&provider.id).await?;
    let model = listed.models.first().expect("persisted catalog model");
    let original = gw
        .admin()
        .get_provider_model(&provider.id, &model.id)
        .await?;
    let mut metadata = serde_json::to_value(&original.metadata)?;
    let object = metadata.as_object_mut().expect("metadata object");
    object.insert(
        "description".to_string(),
        serde_json::Value::String("Locally curated description".to_string()),
    );
    object.insert(
        "cost".to_string(),
        serde_json::from_str(
            r#"{
                "input": 0.123456789012345678,
                "output": 2.5,
                "tiers": [{
                    "tier": {"type": "context", "size": 200000},
                    "input": 0.25,
                    "output": 5
                }]
            }"#,
        )?,
    );
    let updated = gw
        .admin()
        .update_provider_model(
            &provider.id,
            &model.id,
            UpdateProviderModel {
                metadata: metadata.clone(),
                revision: original.revision,
            },
        )
        .await?;
    assert_eq!(
        updated.metadata.description.as_deref(),
        Some("Locally curated description")
    );
    let stored = gw
        .storage
        .provider_models()
        .get(&provider.id, &model.id)
        .await?
        .expect("stored Provider Model");
    assert_eq!(
        stored
            .metadata
            .cost
            .as_ref()
            .and_then(|cost| cost.prices.input)
            .map(|value| value.to_string())
            .as_deref(),
        Some("0.123456789012345678")
    );
    assert_eq!(stored.cost_rules.len(), 1);
    assert_eq!(stored.cost_rules[0].threshold_tokens, 200_000);

    let synced_again = gw.admin().sync_provider_models(&provider.id).await?;
    assert_eq!(synced_again.added, 0);
    let frozen = gw
        .admin()
        .get_provider_model(&provider.id, &model.id)
        .await?;
    assert_eq!(
        frozen.metadata.description.as_deref(),
        Some("Locally curated description")
    );

    let stale = gw
        .admin()
        .update_provider_model(
            &provider.id,
            &model.id,
            UpdateProviderModel {
                metadata,
                revision: original.revision,
            },
        )
        .await
        .unwrap_err();
    assert!(stale.to_string().contains("PROVIDER_MODEL_CONFLICT"));

    let disabled = gw
        .admin()
        .update_provider_model_selection(
            &provider.id,
            &model.id,
            UpdateProviderModelSelection {
                policy: ProviderModelSelectionPolicy::ForceDisabled,
                revision: frozen.revision,
            },
        )
        .await?;
    assert!(!disabled.available);
    let enabled = gw
        .admin()
        .update_provider_model_selection(
            &provider.id,
            &model.id,
            UpdateProviderModelSelection {
                policy: ProviderModelSelectionPolicy::ForceEnabled,
                revision: disabled.revision,
            },
        )
        .await?;
    assert!(enabled.available);

    let reimported = gw
        .admin()
        .reimport_provider_model(&provider.id, &model.id, enabled.revision)
        .await?;
    assert_ne!(
        reimported.metadata.description.as_deref(),
        Some("Locally curated description")
    );
    Ok(())
}

#[tokio::test]
async fn manual_provider_models_are_partial_and_do_not_mutate_routes() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let provider = gw
        .admin()
        .create_provider(catalog_provider_input(&gw, "manual-model-provider").await?)
        .await?;
    let prepared = gw
        .admin()
        .prepare_provider_model(&provider.id, "private/model", None)
        .await?;
    assert_eq!(prepared.id, "private/model");
    assert!(prepared.metadata.description.is_none());

    let prepared_template = gw
        .admin()
        .prepare_provider_model(
            &provider.id,
            "provider-gpt-3.5",
            Some("openai/gpt-3.5-turbo"),
        )
        .await?;
    assert_eq!(prepared_template.id, "provider-gpt-3.5");
    assert_eq!(
        prepared_template.metadata.description.as_deref(),
        Some("Compact GPT model for low-latency assistance and high-volume workloads")
    );
    assert_eq!(prepared_template.metadata.family.as_deref(), Some("gpt"));
    assert_eq!(
        prepared_template.extensions["benchmarks"][0]["name"],
        "Artificial Analysis Coding Index"
    );

    let missing_template = gw
        .admin()
        .prepare_provider_model(
            &provider.id,
            "private/unknown",
            Some("openai/not-in-catalog"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        missing_template.downcast_ref::<CatalogError>(),
        Some(CatalogError::ModelNotFound { id }) if id == "openai/not-in-catalog"
    ));

    let created = gw
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "private/model",
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "private/model",
                    "name": "Private Model",
                    "tool_call": true,
                    "reasoning_options": [{
                        "type": "effort",
                        "values": ["none", "low", "medium", "high", "xhigh"]
                    }],
                    "vendor_extension": {"mode": "private"}
                }),
            },
        )
        .await?;
    assert_eq!(created.metadata.name.as_deref(), Some("Private Model"));
    assert_eq!(created.extensions["vendor_extension"]["mode"], "private");

    let route = gw
        .admin()
        .create_model(CreateRoute {
            name: "private-route".to_string(),
            balance: Some("weighted".to_string()),
            target_provider: provider.id.clone(),
            target_model: "private/model".to_string(),
            targets: vec![],
        })
        .await?;
    gw.admin()
        .delete_manual_provider_model(&provider.id, "private/model")
        .await?;
    assert!(
        gw.admin()
            .list_models()
            .await?
            .iter()
            .any(|model| model.id == route.id)
    );
    Ok(())
}

#[tokio::test]
async fn discovered_models_are_persisted_and_enriched_without_expanding_ids() -> anyhow::Result<()>
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await?;
        let body = serde_json::json!({
            "models": [
                {"slug": "gpt-5.4", "visibility": "list"},
                {"slug": "endpoint-only-model", "visibility": "list"},
                {"slug": "gpt-5.6-sol-wm", "visibility": "hide"}
            ]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await?;
        anyhow::Ok(())
    });

    let gw = build_gateway().await?;
    let provider = gw
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "dynamic-catalog-provider".to_string(),
            vendor: Some("openai".to_string()),
            protocol: "open-responses".to_string(),
            base_url: format!("http://{address}"),
            preset_key: Some("openai".to_string()),
            channel: Some("default".to_string()),
            models_source: Some(format!("http://{address}/models")),
            static_models: None,
            api_key: "sk-test".to_string(),
            adapter_credentials: r#"{"apiKey":"sk-test"}"#.to_string(),
            auth_mode: "apikey".to_string(),
            use_proxy: false,
        })
        .await?;
    let summary = gw.admin().sync_provider_models(&provider.id).await?;
    server.await??;
    assert_eq!(summary.added, 2);
    let models = gw.admin().list_provider_models(&provider.id).await?;
    assert_eq!(models.models.len(), 2);
    let known = gw
        .admin()
        .get_provider_model(&provider.id, "gpt-5.4")
        .await?;
    assert_ne!(known.metadata.name.as_deref(), Some("gpt-5.4"));
    let endpoint_only = gw
        .admin()
        .get_provider_model(&provider.id, "endpoint-only-model")
        .await?;
    assert_eq!(
        endpoint_only.metadata.name.as_deref(),
        Some("endpoint-only-model")
    );
    Ok(())
}

#[tokio::test]
async fn copy_provider_creates_disabled_provider_with_copy_suffix() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("source-provider"))
        .await?;

    let copied = gw.admin().copy_provider(&original.id).await?;

    assert_ne!(copied.id, original.id);
    assert_eq!(copied.name, "source-provider_Copy");
    assert_eq!(copied.vendor, original.vendor);
    assert_eq!(copied.protocol, original.protocol);
    assert_eq!(copied.base_url, original.base_url);
    assert_eq!(copied.preset_key, original.preset_key);
    assert_eq!(copied.channel, original.channel);
    assert_eq!(copied.models_source, original.models_source);
    assert_eq!(copied.static_models, original.static_models);
    assert_eq!(copied.api_key, original.api_key);
    assert_eq!(copied.auth_mode, original.auth_mode);
    assert_eq!(copied.use_proxy, original.use_proxy);
    assert!(original.is_enabled);
    assert!(!copied.is_enabled);

    Ok(())
}

#[tokio::test]
async fn provider_update_keeps_the_selected_option_immutable() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let provider = gw
        .admin()
        .create_provider(catalog_provider_input(&gw, "immutable-provider-option").await?)
        .await?;

    let channel_error = gw
        .admin()
        .update_provider(
            &provider.id,
            UpdateProvider {
                channel: Some("codex".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("an existing Provider cannot switch to another channel");
    assert!(
        channel_error
            .to_string()
            .contains("cannot be changed after creation")
    );

    let protocol_error = gw
        .admin()
        .update_provider(
            &provider.id,
            UpdateProvider {
                protocol: Some("google-gemini".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("a Catalog Provider cannot change its protocol");
    assert!(
        protocol_error
            .to_string()
            .contains("cannot be changed after creation")
    );

    Ok(())
}

#[tokio::test]
async fn copy_provider_uses_numbered_suffix_when_copy_name_exists() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("source-provider"))
        .await?;
    gw.admin().copy_provider(&original.id).await?;

    let second_copy = gw.admin().copy_provider(&original.id).await?;

    assert_eq!(second_copy.name, "source-provider_Copy2");

    Ok(())
}

#[tokio::test]
async fn copy_provider_can_copy_matching_route_targets_to_copied_provider() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("route-source-provider"))
        .await?;
    let fallback = gw
        .admin()
        .create_provider(api_key_provider_input("route-fallback-provider"))
        .await?;
    add_manual_provider_model(&gw, &original.id, "source-upstream-model").await?;
    add_manual_provider_model(&gw, &fallback.id, "fallback-upstream-model").await?;

    let source_route = gw
        .admin()
        .create_model(CreateRoute {
            name: "source-model".to_string(),
            balance: Some("priority".to_string()),
            target_provider: String::new(),
            target_model: String::new(),
            targets: vec![
                CreateTarget {
                    provider_id: original.id.clone(),
                    model: "source-upstream-model".to_string(),
                    weight: Some(80),
                    priority: Some(1),
                    thinking_level_map: Vec::new(),
                },
                CreateTarget {
                    provider_id: fallback.id.clone(),
                    model: "fallback-upstream-model".to_string(),
                    weight: Some(20),
                    priority: Some(2),
                    thinking_level_map: Vec::new(),
                },
            ],
        })
        .await?;

    let copied = gw
        .admin()
        .copy_provider_with_options(
            &original.id,
            CopyProviderOptions {
                append_targets: true,
            },
        )
        .await?;

    assert!(!copied.is_enabled);
    let copied_provider_model = gw
        .admin()
        .get_provider_model(&copied.id, "source-upstream-model")
        .await?;
    assert!(copied_provider_model.available);
    let models = gw.admin().list_models().await?;
    assert_eq!(
        models.len(),
        1,
        "copying route targets must not create new routes"
    );
    assert!(models.iter().all(|model| model.name != "source-model_Copy"));

    let updated_model = models
        .iter()
        .find(|model| model.id == source_route.id)
        .expect("source route should remain");
    assert_eq!(updated_model.name, "source-model");
    assert_eq!(updated_model.balance, "priority");
    assert_eq!(updated_model.target_provider, original.id);
    assert_eq!(updated_model.target_model, "source-upstream-model");
    assert_eq!(updated_model.targets.len(), 3);
    assert!(updated_model.targets.iter().any(|target| {
        target.provider_id == original.id
            && target.model == "source-upstream-model"
            && target.weight == 80
            && target.priority == 1
    }));
    assert!(updated_model.targets.iter().any(|target| {
        target.provider_id == copied.id
            && target.model == "source-upstream-model"
            && target.weight == 80
            && target.priority == 1
    }));
    assert!(updated_model.targets.iter().any(|target| {
        target.provider_id == fallback.id
            && target.model == "fallback-upstream-model"
            && target.weight == 20
            && target.priority == 2
    }));

    Ok(())
}

#[tokio::test]
async fn copy_provider_does_not_append_targets_by_default() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("no-route-copy-provider"))
        .await?;
    add_manual_provider_model(&gw, &original.id, "source-upstream-model").await?;

    gw.admin()
        .create_model(CreateRoute {
            name: "no-route-copy-model".to_string(),
            balance: None,
            target_provider: original.id.clone(),
            target_model: "source-upstream-model".to_string(),
            targets: vec![],
        })
        .await?;

    gw.admin().copy_provider(&original.id).await?;

    let models = gw.admin().list_models().await?;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].targets.len(), 1);
    assert_eq!(models[0].targets[0].provider_id, original.id);

    Ok(())
}

#[tokio::test]
async fn copy_oauth_provider_copies_credential_binding() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .storage
        .providers()
        .create(oauth_provider_record())
        .await?;
    seed_oauth_credential(&gw, &original.id, "copy-access-token", "copy-refresh-token").await?;

    let copied = gw.admin().copy_provider(&original.id).await?;
    let copied_credential = gw.storage.oauth_credentials().get(&copied.id).await?;

    assert_eq!(copied.name, format!("{}_Copy", original.name));
    assert_eq!(copied.auth_mode, "oauth");
    assert!(copied.api_key.is_empty());
    assert_eq!(
        copied_credential
            .as_ref()
            .map(|cred| cred.access_token.as_str()),
        Some("copy-access-token"),
    );

    Ok(())
}

#[tokio::test]
async fn logout_provider_oauth_preserves_oauth_mode_and_disconnects_binding() -> anyhow::Result<()>
{
    let gw = build_gateway().await?;
    let provider = gw
        .storage
        .providers()
        .create(oauth_provider_record())
        .await?;
    seed_oauth_credential(&gw, &provider.id, "test-access-token", "test-refresh-token").await?;

    let status = gw.admin().logout_provider_oauth(&provider.id).await?;
    assert_eq!(status.status, "disconnected");

    let updated = gw.admin().get_provider(&provider.id).await?;
    assert_eq!(updated.effective_auth_mode(), "oauth");
    assert!(updated.api_key.is_empty());
    let oauth_cred = gw.storage.oauth_credentials().get(&provider.id).await?;
    assert!(
        oauth_cred.is_none(),
        "oauth credential should be deleted after logout"
    );

    Ok(())
}

#[tokio::test]
async fn catalog_provider_uses_runtime_discovery_without_expanding_scope() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await?;
        let body = r#"{"data":[{"id":"account-only-model"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await?;
        anyhow::Ok(())
    });

    let gw = build_gateway().await?;
    let provider = gw
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "catalog-openai-account".to_string(),
            vendor: Some("openai".to_string()),
            protocol: "open-responses".to_string(),
            base_url: format!("http://{address}"),
            preset_key: Some("openai".to_string()),
            channel: Some("default".to_string()),
            models_source: Some("catalog".to_string()),
            static_models: None,
            api_key: "sk-test".to_string(),
            adapter_credentials: r#"{"apiKey":"sk-test"}"#.to_string(),
            auth_mode: "apikey".to_string(),
            use_proxy: false,
        })
        .await?;
    let stored = gw.admin().get_provider(&provider.id).await?;
    assert_eq!(stored.preset_key.as_deref(), Some("openai"));
    assert_eq!(stored.channel.as_deref(), Some("default"));

    let summary = gw.admin().sync_provider_models(&provider.id).await?;
    assert_eq!(summary.added, 1);
    let models = gw.admin().list_provider_models(&provider.id).await?;
    assert_eq!(models.models.len(), 1);
    assert_eq!(models.models[0].id, "account-only-model");
    timeout(Duration::from_secs(5), server).await???;
    let model = gw
        .admin()
        .get_provider_model(&provider.id, "account-only-model")
        .await?;
    assert!(model.metadata.description.is_none());
    Ok(())
}

#[tokio::test]
async fn catalog_capabilities_use_the_catalog_scope_not_vendor_channel() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let provider = gw
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "vertex-catalog-capabilities".to_string(),
            vendor: Some("google-vertex".to_string()),
            protocol: "google-gemini".to_string(),
            base_url: "https://aiplatform.googleapis.com".to_string(),
            preset_key: Some("vertexai".to_string()),
            channel: Some("native".to_string()),
            models_source: None,
            static_models: None,
            api_key: "{}".to_string(),
            adapter_credentials: r#"{"credentials":"{}"}"#.to_string(),
            auth_mode: "apikey".to_string(),
            use_proxy: false,
        })
        .await?;

    let capabilities = gw
        .admin()
        .get_model_capabilities(&provider.id, "gpt-5.4")
        .await?;

    assert_eq!(capabilities.provider, "google");
    assert_eq!(capabilities.model_id, "gpt-5.4");
    assert_eq!(capabilities.context_window, 272_000);
    Ok(())
}

async fn build_gateway() -> anyhow::Result<Gateway> {
    let config = GatewayConfig {
        data_dir: test_data_dir(),
        ..Default::default()
    };
    let (mut gw, _log_rx) = Gateway::new(config).await?;
    gw.provider_catalog =
        ProviderCatalog::with_source(&gw.config.data_dir, Arc::new(TestCatalogSource))?;
    Ok(gw)
}

async fn add_manual_provider_model(
    gw: &Gateway,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<()> {
    gw.admin()
        .create_manual_provider_model(
            provider_id,
            model_id,
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": model_id,
                    "name": model_id,
                }),
            },
        )
        .await?;
    Ok(())
}

// ── config epoch tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn config_epoch_starts_at_zero_and_increments_on_model_create() -> anyhow::Result<()> {
    let gw = build_gateway().await?;

    let epoch_before: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let provider = gw
        .admin()
        .create_provider(api_key_provider_input("epoch-test-provider"))
        .await?;
    add_manual_provider_model(&gw, &provider.id, "gpt-4").await?;
    gw.admin()
        .create_model(CreateRoute {
            name: "epoch-test-model".to_string(),
            balance: Some("weighted".to_string()),
            target_provider: provider.id.clone(),
            target_model: "gpt-4".to_string(),
            targets: vec![],
        })
        .await?;

    let epoch_after: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    assert!(
        epoch_after > epoch_before,
        "config_epoch should increment after create_model: before={epoch_before} after={epoch_after}"
    );
    Ok(())
}

#[tokio::test]
async fn config_epoch_increments_on_model_update_and_delete() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let provider = gw
        .admin()
        .create_provider(api_key_provider_input("epoch-update-provider"))
        .await?;
    add_manual_provider_model(&gw, &provider.id, "gpt-4").await?;
    let model = gw
        .admin()
        .create_model(CreateRoute {
            name: "epoch-update-model".to_string(),
            balance: Some("weighted".to_string()),
            target_provider: provider.id.clone(),
            target_model: "gpt-4".to_string(),
            targets: vec![],
        })
        .await?;

    let epoch_before_update: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    gw.admin()
        .update_model(
            &model.name,
            UpdateRoute {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;

    let epoch_after_update: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(
        epoch_after_update > epoch_before_update,
        "epoch should increment on update"
    );

    gw.admin().delete_model(&model.name).await?;

    let epoch_after_delete: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(
        epoch_after_delete > epoch_after_update,
        "epoch should increment on delete"
    );

    Ok(())
}

// ── readyz (StorageBootstrap::health) ─────────────────────────────────────

#[tokio::test]
async fn storage_health_is_reachable_for_sqlite_gateway() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let health = gw.storage.bootstrap().health().await?;
    assert!(
        health.can_connect,
        "SQLite health check should report can_connect"
    );
    assert!(
        health.schema_compatible,
        "SQLite health check should report schema_compatible after migration"
    );
    Ok(())
}

#[tokio::test]
async fn schema_compatible_is_false_when_migrations_skipped() -> anyhow::Result<()> {
    // Create a SQLite pool on a fresh directory without running any migrations.
    // Gateway::new() would fail at RouteCache load, so test directly at storage level.
    let dir = tempfile::tempdir()?;
    let pool = stravia_core::db::init_pool(dir.path()).await?;
    let storage = stravia_core::storage::SqliteStorage::from_pool(pool);

    let health = storage.bootstrap().health().await?;

    assert!(
        health.can_connect,
        "should still connect to SQLite even without schema"
    );
    assert!(
        !health.schema_compatible,
        "schema_compatible must be false when models table has not been created"
    );
    Ok(())
}

fn test_data_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "stravia-admin-integration-tests-{}",
        Uuid::new_v4()
    ))
}

fn oauth_provider_record() -> CreateProviderRecord {
    CreateProviderRecord {
        name: format!("oauth-provider-{}", Uuid::new_v4()),
        vendor: Some("openai".to_string()),
        protocol: "open-responses".to_string(),
        base_url: CODEX_RUNTIME_URL.to_string(),
        preset_key: Some("openai".to_string()),
        channel: Some("codex".to_string()),
        models_source: Some("catalog".to_string()),
        static_models: None,
        api_key: String::new(),
        adapter_credentials: "{}".to_string(),
        auth_mode: "oauth".to_string(),
        use_proxy: false,
    }
}

fn api_key_provider_input(name: &str) -> CreateProvider {
    CreateProvider {
        name: Some(name.to_string()),
        source: ProviderSourceInput::Custom {
            vendor: Some("openai".to_string()),
            protocol: "openai-compatible".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            models_source: Some("https://api.openai.com/v1/models".to_string()),
            static_models: Some("gpt-test\ntext-test".to_string()),
        },
        credential: ProviderCredentialInput::ApiKey {
            value: "sk-test".to_string(),
        },
        use_proxy: true,
    }
}

async fn catalog_provider_input(gw: &Gateway, name: &str) -> anyhow::Result<CreateProvider> {
    catalog_provider_input_for(gw, name, "openai").await
}

async fn catalog_provider_input_for(
    gw: &Gateway,
    name: &str,
    provider_id: &str,
) -> anyhow::Result<CreateProvider> {
    let catalog = gw.provider_catalog.providers().await;
    let provider = catalog
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| anyhow::anyhow!("{provider_id} missing from built-in Catalog"))?;
    let channel = provider
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .ok_or_else(|| anyhow::anyhow!("{provider_id} default channel missing"))?;
    Ok(CreateProvider {
        name: Some(name.to_string()),
        source: ProviderSourceInput::Catalog {
            provider_id: provider.id.clone(),
            channel_id: channel.id.clone(),
            fingerprint: channel.fingerprint.clone(),
            base_url_override: None,
        },
        credential: ProviderCredentialInput::ApiKey {
            value: "sk-test".to_string(),
        },
        use_proxy: true,
    })
}

async fn seed_oauth_credential(
    gw: &Gateway,
    provider_id: &str,
    access_token: &str,
    refresh_token: &str,
) -> anyhow::Result<()> {
    gw.storage
        .oauth_credentials()
        .upsert(
            provider_id,
            UpsertOAuthCredential {
                driver_key: "codex".to_string(),
                scheme: "oauth_auth_code_pkce".to_string(),
                access_token: access_token.to_string(),
                refresh_token: Some(refresh_token.to_string()),
                expires_at: Some(FAR_FUTURE_RFC3339.to_string()),
                resource_url: Some(CODEX_RUNTIME_URL.to_string()),
                subject_id: Some("acct_test".to_string()),
                scopes: Some("[\"openid\",\"offline_access\"]".to_string()),
                meta: Some(format!(r#"{{"access_token":"{access_token}"}}"#)),
            },
        )
        .await?;
    Ok(())
}

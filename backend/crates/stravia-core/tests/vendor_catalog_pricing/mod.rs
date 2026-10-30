use super::*;
use rust_decimal::Decimal;

async fn pricing_catalog() -> anyhow::Result<FakeCatalog> {
    let catalog = FakeCatalog::start().await?;
    catalog
        .set_index(
            "pricing-1",
            json!({
                "google": catalog_entry("google", "Google", "@ai-sdk/google", None),
                "anthropic": catalog_entry("anthropic", "Anthropic", "@ai-sdk/anthropic", None),
                "openai": catalog_entry("openai", "OpenAI", "@ai-sdk/openai", None)
            }),
        )
        .await;
    catalog.state.write().await.canonical = json!({
        "google/gemini-3.8-flash": {
            "id": "google/gemini-3.8-flash", "name": "Gemini 3.8 Flash",
            "modalities": {"input": ["text"], "output": ["text"]}
        }
    });
    catalog
        .set_scope(
            "google",
            200,
            json!({
                "gemini-3.8-flash": {
                    "id": "gemini-3.8-flash", "name": "Gemini 3.8 Flash",
                    "canonical_id": "google/gemini-3.8-flash",
                    "limit": {"context": 1048576},
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "cost": {"input": "0.75", "output": "3.75", "cache_read": "0.075"}
                }
            }),
        )
        .await;
    Ok(catalog)
}

fn discovered_provider(catalog: &FakeCatalog) -> CreateProvider {
    CreateProvider {
        name: Some("subscription channel".into()),
        source: ProviderSourceInput::Custom {
            vendor: "custom".into(),
            channel: "default".into(),
            protocol: Some("openai-compatible".into()),
            base_url: format!("{}/v1", catalog.base_url()),
            models_source: Some(format!("{}/v1/models", catalog.base_url())),
            static_models: None,
        },
        credential: ProviderCredentialInput::None,
        vendor_options: Default::default(),
        use_proxy: false,
    }
}

#[tokio::test]
async fn missing_channel_price_uses_official_cost_without_replacing_channel_specifications()
-> anyhow::Result<()> {
    let catalog = pricing_catalog().await?;
    catalog.state.write().await.inventory = json!({"data": [{
        "id": "gemini-3.8-flash", "display_name": "Channel Flash",
        "context_window": 131072,
        "modalities": {"input": ["text"], "output": ["text"]}
    }]});
    let directory = tempfile::tempdir()?;
    let gateway = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gateway.catalog_sync.refresh().await?;
    let provider = gateway
        .admin()
        .create_provider(discovered_provider(&catalog))
        .await?;
    gateway.admin().sync_provider_models(&provider.id).await?;
    let model = gateway
        .admin()
        .get_provider_model(&provider.id, "gemini-3.8-flash")
        .await?;

    assert_eq!(model.metadata.name.as_deref(), Some("Channel Flash"));
    assert_eq!(
        model
            .metadata
            .limit
            .as_ref()
            .and_then(|limit| limit.context),
        Some(131072)
    );
    assert_eq!(model.metadata.modalities.as_ref().unwrap().input, ["text"]);
    let cost = model
        .metadata
        .cost
        .expect("missing channel price must inherit the official API price");
    assert_eq!(cost.prices.input, Some(Decimal::new(75, 2)));
    assert_eq!(cost.prices.output, Some(Decimal::new(375, 2)));
    assert_eq!(cost.prices.cache_read, Some(Decimal::new(75, 3)));
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unavailable_official_prices_keep_last_good_snapshot_and_pinned_values()
-> anyhow::Result<()> {
    use stravia_core::provider_models::{RestoreProviderModelSync, UpdateProviderModel};

    let catalog = pricing_catalog().await?;
    catalog.state.write().await.inventory = json!({"data": [{"id": "gemini-3.8-flash"}]});
    let directory = tempfile::tempdir()?;
    let gateway = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gateway.catalog_sync.refresh().await?;
    let provider = gateway
        .admin()
        .create_provider(discovered_provider(&catalog))
        .await?;
    gateway.admin().sync_provider_models(&provider.id).await?;
    let initial = gateway
        .admin()
        .get_provider_model(&provider.id, "gemini-3.8-flash")
        .await?;
    gateway
        .admin()
        .update_provider_model(
            &provider.id,
            &initial.id,
            UpdateProviderModel {
                revision: initial.revision,
                changes: std::collections::BTreeMap::from([(
                    "cost.output".to_owned(),
                    json!({"amount": "9", "currency": "usd"}),
                )]),
            },
        )
        .await?;

    let index = catalog.state.read().await.index.clone();
    catalog.state.write().await.canonical["google/gemini-3.8-flash"]["cost"] = json!({});
    catalog
        .set_index("pricing-unavailable", index.clone())
        .await;
    catalog
        .set_scope("google", 503, json!({"error": "fixture unavailable"}))
        .await;
    gateway.catalog_sync.refresh().await?;
    gateway.admin().sync_provider_models(&provider.id).await?;
    let stale = gateway
        .admin()
        .get_provider_model(&provider.id, &initial.id)
        .await?;
    let stale_cost = stale.metadata.cost.unwrap();
    assert_eq!(stale_cost.prices.input, Some(Decimal::new(75, 2)));
    assert_eq!(stale_cost.prices.output, Some(Decimal::new(9, 0)));

    catalog.set_index("pricing-recovered", index.clone()).await;
    catalog
        .set_scope(
            "google",
            200,
            json!({
                "gemini-3.8-flash": {"id": "gemini-3.8-flash", "name": "Gemini 3.8 Flash",
                    "cost": {"input": "1", "output": "4", "cache_read": "0.1",
                        "context_over_200k": {"input": "2", "output": "8"}}}
            }),
        )
        .await;
    gateway.catalog_sync.refresh().await?;
    gateway.admin().sync_provider_models(&provider.id).await?;
    let recovered = gateway
        .admin()
        .get_provider_model(&provider.id, &initial.id)
        .await?;
    let recovered_cost = recovered.metadata.cost.as_ref().unwrap();
    assert_eq!(recovered_cost.prices.input, Some(Decimal::ONE));
    assert_eq!(recovered_cost.prices.output, Some(Decimal::new(9, 0)));
    assert_eq!(
        recovered_cost.context_over_200k.as_ref().unwrap().output,
        Some(Decimal::new(8, 0))
    );
    let restored = gateway
        .admin()
        .restore_provider_model_sync(
            &provider.id,
            &initial.id,
            RestoreProviderModelSync {
                revision: recovered.revision,
                fields: vec!["cost.output".to_owned()],
            },
        )
        .await?;
    assert_eq!(
        restored.metadata.cost.unwrap().prices.output,
        Some(Decimal::new(4, 0))
    );

    // 成功刷新后的权威缺价与临时故障不同，不能永久保留过期参考价。
    catalog.set_index("pricing-removed", index).await;
    catalog.set_scope("google", 200, json!({})).await;
    gateway.catalog_sync.refresh().await?;
    gateway.admin().sync_provider_models(&provider.id).await?;
    let removed = gateway
        .admin()
        .get_provider_model(&provider.id, &initial.id)
        .await?
        .metadata
        .cost
        .unwrap_or_default();
    assert_eq!(removed.prices.input, None);
    assert_eq!(removed.prices.output, None);
    assert_eq!(removed.prices.cache_read, None);
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn official_prices_match_names_and_versions_but_preserve_variants_and_channel_zeroes()
-> anyhow::Result<()> {
    let catalog = pricing_catalog().await?;
    {
        let mut state = catalog.state.write().await;
        state.canonical.as_object_mut().unwrap().extend(json!({
            "anthropic/claude-opus-4-6": {"id": "anthropic/claude-opus-4-6", "name": "Claude Opus 4.6"},
            "openai/gpt-oss-120b": {"id": "openai/gpt-oss-120b", "name": "GPT OSS 120B", "open_weights": true},
            "openai/gpt-6-sol": {"id": "openai/gpt-6-sol", "name": "GPT 6 Sol"},
            "google/gemini-flash-20261015": {"id": "google/gemini-flash-20261015", "name": "Gemini Flash Dated"},
            "google/ambiguous": {"id": "google/ambiguous", "name": "Shared Model"},
            "anthropic/ambiguous": {"id": "anthropic/ambiguous", "name": "Shared Model"}
        }).as_object().unwrap().clone());
        state.inventory = json!({"data": [
            {"id": "GEMINI-3.8-FLASH"},
            {"id": "gemini-3-8-flash"},
            {"id": "opaque-flash", "display_name": " Gemini_3.8 Flash "},
            {"id": "claude-opus-4-6-thinking", "display_name": "Claude Opus 4.6 (Thinking)"},
            {"id": "gpt-oss-120b"},
            {"id": "gemini-3.8-pro"},
            {"id": "gemini-38-flash"},
            {"id": "gemini-flash-20261015"},
            {"id": "unknown-model", "display_name": "Shared Model"},
            {"id": "declared-oss", "canonical_id": "openai/gpt-oss-120b", "display_name": "Gemini 3.8 Flash"},
            {"id": "gpt-6-sol", "cost": {"input": "0", "output": "0"}},
            {"id": "openai/gpt-6-sol", "cost": {"input": "7"}}
        ]});
        state.scopes.get_mut("google").unwrap().1["gemini-flash-20261030"] = json!({
            "id": "gemini-flash-20261030", "name": "Gemini Flash Dated",
            "cost": {"input": "1", "output": "2"}
        });
    }
    catalog.set_scope("anthropic", 200, json!({
        "claude-opus-4-6": {
            "id": "claude-opus-4-6", "name": "Claude Opus 4.6",
            "canonical_id": "anthropic/claude-opus-4-6",
            "cost": {"input": "5", "output": "25", "cache_read": "0.5", "cache_write": "6.25"}
        }
    })).await;
    catalog.set_scope("openai", 200, json!({
        "gpt-6-sol": {"id": "gpt-6-sol", "name": "GPT 6 Sol", "cost": {"input": "2", "output": "10"}}
    })).await;
    let directory = tempfile::tempdir()?;
    let gateway = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gateway.catalog_sync.refresh().await?;
    let provider = gateway
        .admin()
        .create_provider(discovered_provider(&catalog))
        .await?;
    gateway.admin().sync_provider_models(&provider.id).await?;
    for id in ["GEMINI-3.8-FLASH", "gemini-3-8-flash", "opaque-flash"] {
        let model = gateway.admin().get_provider_model(&provider.id, id).await?;
        assert_eq!(model.id, id);
        assert_eq!(
            model.metadata.cost.unwrap().prices.input,
            Some(Decimal::new(75, 2)),
            "{id}"
        );
    }
    let thinking = gateway
        .admin()
        .get_provider_model(&provider.id, "claude-opus-4-6-thinking")
        .await?;
    let cost = thinking.metadata.cost.unwrap();
    assert_eq!(cost.prices.output, Some(Decimal::new(25, 0)));
    assert_eq!(cost.prices.cache_write, Some(Decimal::new(625, 2)));
    for id in [
        "gpt-oss-120b",
        "gemini-3.8-pro",
        "gemini-38-flash",
        "gemini-flash-20261015",
        "unknown-model",
        "declared-oss",
    ] {
        assert!(
            gateway
                .admin()
                .get_provider_model(&provider.id, id)
                .await?
                .metadata
                .cost
                .is_none(),
            "{id} must remain unpriced"
        );
    }
    let zero = gateway
        .admin()
        .get_provider_model(&provider.id, "gpt-6-sol")
        .await?;
    assert_eq!(
        zero.metadata.cost.unwrap().prices.input,
        Some(Decimal::ZERO)
    );
    let partial = gateway
        .admin()
        .get_provider_model(&provider.id, "openai/gpt-6-sol")
        .await?;
    let partial = partial.metadata.cost.unwrap();
    assert_eq!(partial.prices.input, Some(Decimal::new(7, 0)));
    assert_eq!(partial.prices.output, None);
    gateway.shutdown().await;
    Ok(())
}

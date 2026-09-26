//! Provider Catalog parity integration: the real base Wasm component fetches
//! the catalog from an in-process fixture over the production `sync-catalog`
//! call boundary. No test reaches the production network — the catalog origin
//! is injected through `GatewayConfig::catalog_base_url`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use stravia_core::Gateway;
use stravia_core::admin::ProviderConfigurationPreviewInput;
use stravia_core::config::GatewayConfig;
use stravia_core::db::models::{
    CreateProvider, CreateProviderRecord, ProviderCredentialInput, ProviderSourceInput,
    UpdateProvider,
};
use stravia_core::provider_catalog::CatalogError;
use stravia_core::provider_models::{
    NewProviderModelRecord, ProviderModelMetadata, ProviderModelPresence,
    ProviderModelSelectionPolicy, ProviderModelSourceKind, SnapshotState, SourceStamp,
};
use stravia_runtime_contract::thinking::ThinkingLevel;
use tokio::sync::RwLock;

#[derive(Clone)]
struct FakeCatalog {
    state: Arc<RwLock<FakeCatalogState>>,
    base_url: String,
}

struct FakeCatalogState {
    revision: String,
    generated_at: String,
    index: Value,
    index_status: u16,
    canonical: Value,
    canonical_status: u16,
    scopes: HashMap<String, (u16, Value)>,
}

impl FakeCatalog {
    async fn start() -> anyhow::Result<Self> {
        let state = Arc::new(RwLock::new(FakeCatalogState {
            revision: "rev-1".to_owned(),
            generated_at: "2025-01-01T00:00:00Z".to_owned(),
            index: json!({}),
            index_status: 200,
            canonical: json!({
                "acme/m-1": { "id": "acme/m-1", "name": "Acme Model One" }
            }),
            canonical_status: 200,
            scopes: HashMap::new(),
        }));
        let app = Router::new()
            .route("/version.json", get(version))
            .route("/providers.json", get(index))
            .route("/models.json", get(canonical))
            .route("/providers/{id}/models.json", get(scope))
            .route(
                "/v1/models",
                get(|| async { Json(json!({"data": [{"id": "glm-5.3"}]})) }),
            )
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { state, base_url })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn set_index(&self, revision: &str, index: Value) {
        let mut state = self.state.write().await;
        state.revision = revision.to_owned();
        state.index = index;
        state.index_status = 200;
    }

    async fn fail_index(&self) {
        self.state.write().await.index_status = 500;
    }

    async fn fail_canonical(&self) {
        self.state.write().await.canonical_status = 500;
    }

    async fn set_scope(&self, provider_id: &str, status: u16, body: Value) {
        self.state
            .write()
            .await
            .scopes
            .insert(provider_id.to_owned(), (status, body));
    }
}

async fn version(State(state): State<Arc<RwLock<FakeCatalogState>>>) -> impl IntoResponse {
    let state = state.read().await;
    Json(json!({
        "revision": state.revision,
        "generated_at": state.generated_at,
    }))
}

async fn index(State(state): State<Arc<RwLock<FakeCatalogState>>>) -> impl IntoResponse {
    let state = state.read().await;
    if state.index_status != 200 {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "index error").into_response();
    }
    Json(state.index.clone()).into_response()
}

async fn canonical(State(state): State<Arc<RwLock<FakeCatalogState>>>) -> impl IntoResponse {
    let state = state.read().await;
    if state.canonical_status != 200 {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "canonical error",
        )
            .into_response();
    }
    Json(state.canonical.clone()).into_response()
}

async fn scope(
    State(state): State<Arc<RwLock<FakeCatalogState>>>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let state = state.read().await;
    match state.scopes.get(&provider_id) {
        Some((200, body)) => Json(body.clone()).into_response(),
        Some((status, _)) => axum::http::StatusCode::from_u16(*status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

fn catalog_entry(id: &str, name: &str, npm: &str, api: Option<&str>) -> Value {
    json!({
        "id": id,
        "name": name,
        "npm": npm,
        "api": api,
    })
}

fn scope_body() -> Value {
    json!({
        "zen-1": {
            "id": "zen-1",
            "name": "Zen One",
            "tool_call": true,
            "temperature": true,
            "modalities": { "input": ["text"], "output": ["text"] },
            "limit": { "context": 200000, "output": 8192 },
            "cost": { "input": 1.0, "output": 2.0 }
        }
    })
}

async fn fixture_gateway(catalog_url: &str, data_dir: &std::path::Path) -> anyhow::Result<Gateway> {
    Gateway::new(GatewayConfig {
        data_dir: data_dir.to_path_buf(),
        catalog_base_url: Some(catalog_url.to_owned()),
        ..Default::default()
    })
    .await
}

fn create_catalog_provider(
    provider_id: &str,
    channel_id: &str,
    fingerprint: &str,
) -> CreateProvider {
    CreateProvider {
        name: None,
        source: ProviderSourceInput::Catalog {
            provider_id: provider_id.to_owned(),
            channel_id: channel_id.to_owned(),
            fingerprint: fingerprint.to_owned(),
            base_url_override: None,
        },
        credential: ProviderCredentialInput::None,
        vendor_options: Default::default(),
        use_proxy: false,
    }
}

async fn reasoning_catalog() -> anyhow::Result<FakeCatalog> {
    let catalog = FakeCatalog::start().await?;
    catalog
        .set_index(
            "rev-1",
            json!({
                "zhipuai-coding-plan": catalog_entry(
                    "zhipuai-coding-plan", "Zhipu AI Coding Plan",
                    "@ai-sdk/openai-compatible", Some(&format!("{}/v1", catalog.base_url()))
                ),
                "openai": catalog_entry(
                    "openai", "OpenAI", "@ai-sdk/openai",
                    Some(&format!("{}/v1", catalog.base_url()))
                )
            }),
        )
        .await;
    catalog.state.write().await.canonical = json!({
        "zhipuai/glm-5.3": {
            "id": "zhipuai/glm-5.3", "name": "GLM-5.3", "reasoning": true
        }
    });
    let models = json!({
        "glm-5.3": {
            "id": "glm-5.3", "name": "GLM-5.3", "reasoning": true,
            "reasoning_options": [{"type": "effort", "values": ["low", "high", "max"]}]
        },
        "glm-4.6v": {
            "id": "glm-4.6v", "name": "GLM-4.6V", "reasoning": true,
            "reasoning_options": [{"type": "toggle"}]
        }
    });
    catalog
        .set_scope("zhipuai-coding-plan", 200, models.clone())
        .await;
    catalog.set_scope("openai", 200, models).await;
    Ok(catalog)
}

fn editor_provider(catalog: &FakeCatalog, vendor: &str) -> CreateProvider {
    CreateProvider {
        name: Some(vendor.to_owned()),
        source: ProviderSourceInput::Custom {
            vendor: vendor.to_owned(),
            channel: "default".to_owned(),
            protocol: None,
            base_url: format!("{}/v1", catalog.base_url()),
            models_source: None,
            static_models: None,
        },
        credential: ProviderCredentialInput::None,
        vendor_options: Default::default(),
        use_proxy: false,
    }
}

#[tokio::test]
async fn editor_created_provider_imports_scoped_reasoning_levels() -> anyhow::Result<()> {
    let catalog = reasoning_catalog().await?;
    let directory = tempfile::tempdir()?;
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gw.catalog_sync.refresh().await?;
    let provider = gw
        .admin()
        .create_provider(editor_provider(&catalog, "zhipuai-coding-plan"))
        .await?;
    gw.admin().sync_provider_models(&provider.id).await?;
    let model = gw
        .admin()
        .get_provider_model(&provider.id, "glm-5.3")
        .await?;
    assert_eq!(
        stravia_core::thinking::visible_levels(&model.thinking_level_map),
        [ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max]
    );
    let toggle = gw
        .admin()
        .get_provider_model(&provider.id, "glm-4.6v")
        .await?;
    assert_eq!(
        stravia_core::thinking::visible_levels(&toggle.thinking_level_map),
        [ThinkingLevel::Off, ThinkingLevel::Medium]
    );
    gw.shutdown().await;
    drop(gw);
    let reopened = fixture_gateway(catalog.base_url(), directory.path()).await?;
    let persisted = reopened
        .admin()
        .get_provider_model(&provider.id, "glm-5.3")
        .await?;
    assert_eq!(
        persisted.metadata.reasoning_options,
        model.metadata.reasoning_options
    );
    reopened.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unlinked_provider_recovers_catalog_metadata_without_overwriting_edits()
-> anyhow::Result<()> {
    let catalog = reasoning_catalog().await?;
    let directory = tempfile::tempdir()?;
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gw.catalog_sync.refresh().await?;
    // 重现旧版编辑器已保存的连接，而不是通过修复后的创建入口造数据。
    let provider = gw
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "unlinked".to_owned(),
            vendor: Some("zhipuai-coding-plan".to_owned()),
            protocol: "openai-compatible".to_owned(),
            base_url: format!("{}/v1", catalog.base_url()),
            preset_key: None,
            channel: Some("default".to_owned()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".to_owned(),
            vendor_options: "{}".to_owned(),
            auth_mode: "apikey".to_owned(),
            use_proxy: false,
        })
        .await?;
    for (model_id, edited) in [("glm-5.3", false), ("glm-4.6v", true)] {
        let source = SourceStamp::Canonical {
            model_id: format!("zhipuai/{model_id}"),
        };
        let mut metadata = json!({"id": model_id, "reasoning": true});
        if edited {
            metadata["reasoning_options"] = json!([{"type": "effort", "values": ["high"]}]);
        }
        gw.storage
            .provider_models()
            .create(NewProviderModelRecord {
                provider_id: provider.id.clone(),
                model_id: model_id.to_owned(),
                source_kind: ProviderModelSourceKind::Discovered,
                snapshot_state: if edited {
                    SnapshotState::Edited {
                        source: Some(source),
                    }
                } else {
                    SnapshotState::Imported { source }
                },
                metadata_source_provider_id: None,
                presence: ProviderModelPresence::Present,
                selection_policy: ProviderModelSelectionPolicy::Auto,
                metadata: ProviderModelMetadata::from_value(model_id, metadata)?,
            })
            .await?;
    }
    gw.admin().sync_provider_models(&provider.id).await?;
    let model = gw
        .admin()
        .get_provider_model(&provider.id, "glm-5.3")
        .await?;
    assert_eq!(
        stravia_core::thinking::visible_levels(&model.thinking_level_map),
        [ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max]
    );
    assert!(model.can_reimport);
    assert_eq!(
        model.snapshot_state,
        SnapshotState::Imported {
            source: SourceStamp::ProviderCatalog {
                provider_id: "zhipuai-coding-plan".to_owned()
            }
        }
    );
    let edited = gw
        .admin()
        .get_provider_model(&provider.id, "glm-4.6v")
        .await?;
    assert_eq!(
        stravia_core::thinking::visible_levels(&edited.thinking_level_map),
        [ThinkingLevel::High]
    );
    gw.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn profile_catalog_preserves_explicit_and_account_model_inventories() -> anyhow::Result<()> {
    let catalog = reasoning_catalog().await?;
    let directory = tempfile::tempdir()?;
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gw.catalog_sync.refresh().await?;
    for (name, vendor, source, models) in [
        (
            "explicit",
            "zhipuai-coding-plan",
            Some(format!("{}/v1/models", catalog.base_url())),
            None,
        ),
        (
            "static",
            "zhipuai-coding-plan",
            None,
            Some("glm-5.3".to_owned()),
        ),
        ("account", "openai", None, None),
    ] {
        let mut input = editor_provider(&catalog, vendor);
        input.name = Some(name.to_owned());
        if let ProviderSourceInput::Custom {
            models_source,
            static_models,
            ..
        } = &mut input.source
        {
            *models_source = source;
            *static_models = models;
        }
        let provider = gw.admin().create_provider(input).await?;
        gw.admin().sync_provider_models(&provider.id).await?;
        let inventory = gw.admin().list_provider_models(&provider.id).await?;
        assert_eq!(
            inventory
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["glm-5.3"]
        );
        let model = gw
            .admin()
            .get_provider_model(&provider.id, "glm-5.3")
            .await?;
        assert_eq!(
            stravia_core::thinking::visible_levels(&model.thinking_level_map),
            [ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max]
        );
    }
    gw.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn catalog_entries_register_create_sync_retire_and_survive_restart() -> anyhow::Result<()> {
    let catalog = FakeCatalog::start().await?;
    let upstream = catalog.base_url().to_owned();
    catalog
        .set_index(
            "rev-1",
            json!({
                "fake-zen": catalog_entry("fake-zen", "Fake Zen", "@ai-sdk/openai-compatible", Some(&format!("{upstream}/v1"))),
                "impossible": catalog_entry("impossible", "Impossible", "@totally/unknown-sdk", None),
            }),
        )
        .await;
    catalog.set_scope("fake-zen", 200, scope_body()).await;

    let directory = tempfile::tempdir()?;
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;

    // A remote refresh dynamically registers the compatible entry; the
    // unmappable npm is filtered instead of half-registered.
    let summary = gw.catalog_sync.refresh().await?;
    assert_eq!(summary.revision, "rev-1");
    let choices = gw.admin().catalog_choices().await;
    let zen = choices
        .providers
        .iter()
        .find(|provider| provider.id == "fake-zen")
        .expect("compatible catalog entry registers at runtime");
    assert!(
        choices
            .providers
            .iter()
            .all(|provider| provider.id != "impossible"),
        "unmapped npm entries must not register"
    );

    // Creation through the catalog seam picks up the derived channel.
    let channel = zen
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .expect("default channel");
    let provider = gw
        .admin()
        .create_provider(create_catalog_provider(
            "fake-zen",
            &channel.id,
            &channel.fingerprint,
        ))
        .await?;
    assert_eq!(provider.vendor.as_deref(), Some("fake-zen"));
    assert_eq!(provider.models_source.as_deref(), Some("catalog"));

    // Model sync consumes the guest-fetched scope, not a native fetch.
    gw.admin().sync_provider_models(&provider.id).await?;
    let models = gw.admin().list_provider_models(&provider.id).await?;
    assert!(
        models.models.iter().any(|model| model.id == "zen-1"),
        "scope models should persist through catalog discovery"
    );

    // Removal: the confirmed snapshot drops the entry. Creation is refused
    // while the saved connection keeps its descriptor admission.
    catalog
        .set_index(
            "rev-2",
            json!({
                "other-zen": catalog_entry("other-zen", "Other Zen", "@ai-sdk/openai-compatible", None),
            }),
        )
        .await;
    let summary = gw.catalog_sync.refresh().await?;
    assert_eq!(summary.revision, "rev-2");
    let choices = gw.admin().catalog_choices().await;
    assert!(
        choices
            .providers
            .iter()
            .all(|provider| provider.id != "fake-zen"),
        "removed entries leave the advertised set"
    );
    assert!(
        gw.admin()
            .create_provider(create_catalog_provider(
                "fake-zen",
                "default",
                "fingerprint"
            ))
            .await
            .is_err(),
        "removed entries must not create new providers"
    );
    assert!(
        gw.admin()
            .create_provider(CreateProvider {
                name: None,
                source: ProviderSourceInput::Custom {
                    vendor: "fake-zen".to_owned(),
                    channel: "default".to_owned(),
                    protocol: Some("openai-compatible".to_owned()),
                    base_url: format!("{upstream}/v1"),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await
            .is_err(),
        "retired profiles must not be creatable through the custom seam"
    );

    // The saved connection still resolves its profile: update and existing
    // configuration previews keep working — inference admission is retained.
    let updated = gw
        .admin()
        .update_provider(
            &provider.id,
            UpdateProvider {
                name: Some("Zen".to_owned()),
                ..UpdateProvider::default()
            },
        )
        .await?;
    assert_eq!(updated.name, "Zen");
    gw.admin()
        .preview_provider_configuration(ProviderConfigurationPreviewInput {
            provider_id: Some(provider.id.clone()),
            vendor_id: "fake-zen".to_owned(),
            channel: "default".to_owned(),
            base_url: updated.base_url.clone(),
            options: Default::default(),
            credentials: Default::default(),
        })
        .await?;

    // Catalog sync on the removed provider reports the missing scope
    // honestly — the model list is not silently empty.
    assert!(
        gw.admin().sync_provider_models(&provider.id).await.is_err(),
        "removed catalog scope must fail instead of returning empty success"
    );

    // Restart: the persisted retired set keeps the connection admitted while
    // still refusing new creations.
    drop(gw);
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gw.admin()
        .preview_provider_configuration(ProviderConfigurationPreviewInput {
            provider_id: Some(provider.id.clone()),
            vendor_id: "fake-zen".to_owned(),
            channel: "default".to_owned(),
            base_url: format!("{upstream}/v1"),
            options: Default::default(),
            credentials: Default::default(),
        })
        .await?;
    assert!(
        gw.admin()
            .create_provider(CreateProvider {
                name: None,
                source: ProviderSourceInput::Custom {
                    vendor: "fake-zen".to_owned(),
                    channel: "default".to_owned(),
                    protocol: Some("openai-compatible".to_owned()),
                    base_url: format!("{upstream}/v1"),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await
            .is_err(),
        "retired profiles stay uncreatable after restart"
    );
    Ok(())
}

#[tokio::test]
async fn catalog_scope_failures_stay_distinct() -> anyhow::Result<()> {
    let catalog = FakeCatalog::start().await?;
    catalog
        .set_index(
            "rev-1",
            json!({
                "fake-zen": catalog_entry("fake-zen", "Fake Zen", "@ai-sdk/openai-compatible", None),
            }),
        )
        .await;
    catalog
        .set_scope("fake-zen", 500, json!({"error": "upstream"}))
        .await;

    let directory = tempfile::tempdir()?;
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gw.catalog_sync.refresh().await?;

    let error = gw
        .catalog_sync
        .catalog_models("fake-zen", "default")
        .await
        .expect_err("scope HTTP failure must surface");
    assert!(
        matches!(
            error.downcast_ref::<CatalogError>(),
            Some(CatalogError::ScopeRefresh { .. })
        ),
        "scope HTTP failure must stay ScopeRefresh, got {error:?}"
    );

    let error = gw
        .catalog_sync
        .catalog_models("ghost", "default")
        .await
        .expect_err("missing provider must surface");
    assert!(
        matches!(
            error.downcast_ref::<CatalogError>(),
            Some(CatalogError::ProviderNotFound { .. })
        ),
        "absent provider must stay ProviderNotFound, got {error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn failed_index_refresh_preserves_last_good_snapshot() -> anyhow::Result<()> {
    let catalog = FakeCatalog::start().await?;
    catalog
        .set_index(
            "rev-1",
            json!({
                "fake-zen": catalog_entry("fake-zen", "Fake Zen", "@ai-sdk/openai-compatible", None),
            }),
        )
        .await;

    let directory = tempfile::tempdir()?;
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gw.catalog_sync.refresh().await?;

    catalog.fail_index().await;
    assert!(
        gw.catalog_sync.refresh().await.is_err(),
        "a failed refresh must report failure"
    );
    let choices = gw.admin().catalog_choices().await;
    assert!(
        choices
            .providers
            .iter()
            .any(|provider| provider.id == "fake-zen"),
        "last-good snapshot must survive a failed refresh"
    );
    Ok(())
}

#[tokio::test]
async fn failed_canonical_refresh_reports_failure_while_the_index_advances() -> anyhow::Result<()> {
    let catalog = FakeCatalog::start().await?;
    catalog
        .set_index(
            "rev-1",
            json!({
                "fake-zen": catalog_entry("fake-zen", "Fake Zen", "@ai-sdk/openai-compatible", None),
            }),
        )
        .await;

    let directory = tempfile::tempdir()?;
    let gw = fixture_gateway(catalog.base_url(), directory.path()).await?;
    gw.catalog_sync.refresh().await?;

    // The two documents advance independently: a canonical outage must fail
    // the reported refresh without rolling back the confirmed provider index.
    catalog.fail_canonical().await;
    catalog
        .set_index(
            "rev-2",
            json!({
                "other-zen": catalog_entry("other-zen", "Other Zen", "@ai-sdk/openai-compatible", None),
            }),
        )
        .await;
    assert!(
        gw.catalog_sync.refresh().await.is_err(),
        "a canonical failure must make the refresh report failure"
    );
    let choices = gw.admin().catalog_choices().await;
    assert!(
        choices
            .providers
            .iter()
            .any(|provider| provider.id == "other-zen"),
        "the confirmed provider index still advances on a canonical failure"
    );
    Ok(())
}

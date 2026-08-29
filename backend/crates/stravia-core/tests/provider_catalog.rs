use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify};

use stravia_core::provider_catalog::{
    CatalogError, CatalogSource, CatalogVersion, ProviderCatalog,
};

#[derive(Clone)]
struct ScriptedSource {
    state: Arc<Mutex<SourceState>>,
}

struct SourceState {
    version: CatalogVersion,
    scripted_versions: VecDeque<CatalogVersion>,
    providers: Vec<u8>,
    canonical_models: Vec<u8>,
    scopes: BTreeMap<String, Vec<u8>>,
    scope_errors: BTreeMap<String, String>,
    scope_fetch_gate: Option<ScopeFetchGate>,
    provider_fetches: u32,
    canonical_model_fetches: u32,
    scope_fetches: BTreeMap<String, u32>,
}

#[derive(Clone)]
struct ScopeFetchGate {
    started: Arc<Notify>,
    resume: Arc<Notify>,
}

#[async_trait]
impl CatalogSource for ScriptedSource {
    async fn fetch_version(&self) -> anyhow::Result<CatalogVersion> {
        let mut state = self.state.lock().await;
        Ok(state
            .scripted_versions
            .pop_front()
            .unwrap_or_else(|| state.version.clone()))
    }

    async fn fetch_providers(&self) -> anyhow::Result<Vec<u8>> {
        let mut state = self.state.lock().await;
        state.provider_fetches += 1;
        Ok(state.providers.clone())
    }

    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>> {
        let mut state = self.state.lock().await;
        state.canonical_model_fetches += 1;
        Ok(state.canonical_models.clone())
    }

    async fn fetch_provider_scope(&self, provider_id: &str) -> anyhow::Result<Vec<u8>> {
        let (scope, gate) = {
            let mut state = self.state.lock().await;
            *state
                .scope_fetches
                .entry(provider_id.to_string())
                .or_default() += 1;
            if let Some(message) = state.scope_errors.get(provider_id) {
                anyhow::bail!(message.clone());
            }
            (
                state.scopes.get(provider_id).cloned(),
                state.scope_fetch_gate.take(),
            )
        };
        if let Some(gate) = gate {
            gate.started.notify_one();
            gate.resume.notified().await;
        }
        scope.ok_or_else(|| anyhow::anyhow!("missing scope for {provider_id}"))
    }

    async fn fetch_logo(&self, _provider_id: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("logo is not used by this test")
    }
}

impl ScriptedSource {
    async fn set_version(&self, version: CatalogVersion) {
        self.state.lock().await.version = version;
    }

    async fn script_versions(&self, versions: impl IntoIterator<Item = CatalogVersion>) {
        self.state.lock().await.scripted_versions = versions.into_iter().collect();
    }

    async fn set_canonical_models(&self, canonical_models: Vec<u8>) {
        self.state.lock().await.canonical_models = canonical_models;
    }

    async fn fail_scope(&self, provider_id: &str, message: &str) {
        self.state
            .lock()
            .await
            .scope_errors
            .insert(provider_id.to_string(), message.to_string());
    }

    async fn pause_next_scope_fetch(&self) -> ScopeFetchGate {
        let gate = ScopeFetchGate {
            started: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        };
        self.state.lock().await.scope_fetch_gate = Some(gate.clone());
        gate
    }

    async fn global_fetches(&self) -> (u32, u32) {
        let state = self.state.lock().await;
        (state.provider_fetches, state.canonical_model_fetches)
    }

    async fn scope_fetches(&self, provider_id: &str) -> u32 {
        self.state
            .lock()
            .await
            .scope_fetches
            .get(provider_id)
            .copied()
            .unwrap_or_default()
    }
}

fn version(revision: &str) -> CatalogVersion {
    CatalogVersion {
        revision: revision.to_string(),
        generated_at: "2026-08-20T14:01:40Z".to_string(),
    }
}

fn source() -> ScriptedSource {
    ScriptedSource {
        state: Arc::new(Mutex::new(SourceState {
            version: version("revision-1"),
            scripted_versions: VecDeque::new(),
            providers: br#"{
              "demo": {
                "id": "demo",
                "name": "Demo AI",
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://demo.invalid/v1",
                "doc": "https://demo.invalid/docs"
              }
            }"#
            .to_vec(),
            canonical_models: br#"{
              "demo/chat": {
                "id": "demo/chat",
                "name": "Demo Chat",
                "modalities": { "input": ["text"], "output": ["text"] }
              }
            }"#
            .to_vec(),
            scopes: BTreeMap::from([(
                "demo".to_string(),
                br#"{
                  "chat": {
                    "canonical_id": "demo/chat",
                    "id": "chat",
                    "name": "Demo Chat",
                    "modalities": { "input": ["text"], "output": ["text"] }
                  }
                }"#
                .to_vec(),
            )]),
            scope_errors: BTreeMap::new(),
            scope_fetch_gate: None,
            provider_fetches: 0,
            canonical_model_fetches: 0,
            scope_fetches: BTreeMap::new(),
        })),
    }
}

#[tokio::test]
async fn catalog_npm_maps_to_vendor_id() -> anyhow::Result<()> {
    let mappings = [
        ("openai", "@ai-sdk/openai", "openai", "open-responses"),
        (
            "openai-compatible",
            "@ai-sdk/openai-compatible",
            "openai-compatible",
            "openai-compatible",
        ),
        (
            "anthropic",
            "@ai-sdk/anthropic",
            "anthropic",
            "anthropic-messages",
        ),
        ("google", "@ai-sdk/google", "google", "google-gemini"),
        ("xai", "@ai-sdk/xai", "xai", "openai-compatible"),
        (
            "azure-cognitive-services",
            "@ai-sdk/azure",
            "azure",
            "openai-compatible",
        ),
        ("groq", "@ai-sdk/groq", "groq", "openai-compatible"),
        (
            "cerebras",
            "@ai-sdk/cerebras",
            "cerebras",
            "openai-compatible",
        ),
        (
            "togetherai",
            "@ai-sdk/togetherai",
            "togetherai",
            "openai-compatible",
        ),
        ("mistral", "@ai-sdk/mistral", "mistral", "openai-compatible"),
        (
            "deepinfra",
            "@ai-sdk/deepinfra",
            "deepinfra",
            "openai-compatible",
        ),
        (
            "perplexity",
            "@ai-sdk/perplexity",
            "perplexity",
            "openai-compatible",
        ),
        (
            "gateway",
            "@ai-sdk/gateway",
            "gateway",
            "gateway-language-model",
        ),
        ("vercel", "@ai-sdk/vercel", "vercel", "openai-compatible"),
        (
            "vertexai",
            "@ai-sdk/google-vertex",
            "google-vertex",
            "google-gemini",
        ),
        (
            "vertex-anthropic",
            "@ai-sdk/google-vertex/anthropic",
            "google-vertex-anthropic",
            "anthropic-messages",
        ),
        (
            "amazon-bedrock",
            "@ai-sdk/amazon-bedrock",
            "amazon-bedrock",
            "bedrock-converse",
        ),
        ("cohere", "@ai-sdk/cohere", "cohere", "cohere-chat"),
        (
            "openrouter",
            "@openrouter/ai-sdk-provider",
            "openrouter",
            "openai-compatible",
        ),
        (
            "watsonx",
            "watsonx-ai-provider",
            "watsonx",
            "watsonx-text-chat",
        ),
        (
            "venice",
            "venice-ai-sdk-provider",
            "venice",
            "openai-compatible",
        ),
        (
            "aihubmix",
            "@aihubmix/ai-sdk-provider",
            "aihubmix",
            "openai-compatible",
        ),
        (
            "sap-ai-core",
            "@jerome-benoit/sap-ai-provider-v2",
            "sap-ai-core",
            "openai-compatible",
        ),
        ("qvac", "@qvac/ai-sdk-provider", "qvac", "openai-compatible"),
        (
            "salad-cloud",
            "@saladtechnologies-oss/ai-sdk-provider",
            "salad-cloud",
            "openai-compatible",
        ),
        (
            "cloudflare-ai-gateway",
            "ai-gateway-provider",
            "cloudflare-ai-gateway",
            "openai-compatible",
        ),
        (
            "gitlab",
            "gitlab-ai-provider",
            "gitlab",
            "openai-compatible",
        ),
        (
            "merge-gateway",
            "merge-gateway-ai-sdk-provider",
            "merge-gateway",
            "openai-compatible",
        ),
    ];
    let source = source();
    let providers = mappings
        .iter()
        .map(|(id, npm, _, _)| {
            (
                (*id).to_string(),
                json!({ "id": id, "name": id, "npm": npm, "api": "" }),
            )
        })
        .collect::<Map<_, _>>();
    source.state.lock().await.providers = serde_json::to_vec(&Value::Object(providers))?;

    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;
    catalog.refresh().await?;
    let providers = catalog.providers().await;

    for (id, npm, vendor_id, protocol) in mappings {
        let provider = providers
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap_or_else(|| panic!("catalog Provider {id}"));
        assert_eq!(provider.npm, npm);
        assert_eq!(provider.vendor_id, vendor_id);
        assert_eq!(provider.protocol, protocol);
    }
    Ok(())
}

#[tokio::test]
async fn catalog_commits_global_indexes_for_one_revision() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;

    let refreshed = catalog.refresh().await?;
    let providers = catalog.providers().await;
    let models = catalog.canonical_models().await;

    assert!(refreshed.changed);
    assert_eq!(refreshed.revision, "revision-1");
    assert_eq!(refreshed.generated_at, "2026-08-20T14:01:40Z");
    assert_eq!(refreshed.provider_count, 1);
    assert_eq!(refreshed.model_count, 1);
    assert_eq!(providers.revision, models.revision);
    assert_eq!(providers.generated_at, models.generated_at);
    assert_eq!(providers.providers[0].id, "demo");
    assert_eq!(models.models[0].id, "demo/chat");
    assert_eq!(source.global_fetches().await, (1, 1));

    let unchanged = catalog.refresh().await?;
    assert!(!unchanged.changed);
    assert_eq!(source.global_fetches().await, (1, 1));
    Ok(())
}

#[tokio::test]
async fn failed_global_update_keeps_the_last_known_good_generation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    catalog.refresh().await?;

    source.set_version(version("revision-2")).await;
    source.set_canonical_models(b"[]".to_vec()).await;

    assert!(catalog.refresh().await.is_err());
    assert_eq!(catalog.providers().await.revision, "revision-1");
    assert_eq!(catalog.canonical_models().await.models[0].id, "demo/chat");
    Ok(())
}

#[tokio::test]
async fn new_global_revision_replaces_the_active_generation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    catalog.refresh().await?;

    source.set_version(version("revision-2")).await;
    let refreshed = catalog.refresh().await?;
    let restarted = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;

    assert!(refreshed.changed);
    assert_eq!(catalog.providers().await.revision, "revision-2");
    assert_eq!(restarted.canonical_models().await.revision, "revision-2");
    Ok(())
}

#[tokio::test]
async fn revision_change_during_download_does_not_publish_a_mixed_generation() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;

    source
        .script_versions([version("revision-1"), version("revision-2")])
        .await;

    assert!(catalog.refresh().await.is_err());
    assert_eq!(catalog.providers().await.revision, "bootstrap");
    assert!(!data_dir.path().join("catalog/active.json").exists());
    Ok(())
}

#[tokio::test]
async fn unsafe_remote_revision_is_rejected_before_persisting_cache_paths() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;

    source.set_version(version("../../outside-cache")).await;

    assert!(catalog.refresh().await.is_err());
    assert_eq!(catalog.providers().await.revision, "bootstrap");
    assert!(!data_dir.path().join("catalog/active.json").exists());
    Ok(())
}

#[tokio::test]
async fn restart_loads_the_complete_last_known_good_generation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    catalog.refresh().await?;

    let restarted = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;

    assert_eq!(restarted.providers().await.revision, "revision-1");
    assert_eq!(restarted.canonical_models().await.models[0].id, "demo/chat");
    Ok(())
}

#[tokio::test]
async fn provider_scope_recovers_from_corruption_and_never_uses_a_prior_revision()
-> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    catalog.refresh().await?;
    catalog.provider_scope("demo").await?;
    assert_eq!(source.scope_fetches("demo").await, 1);

    std::fs::write(
        data_dir.path().join("catalog/scopes/revision-1/demo.json"),
        b"not json",
    )?;
    catalog.provider_scope("demo").await?;
    assert_eq!(source.scope_fetches("demo").await, 2);

    source.set_version(version("revision-2")).await;
    catalog.refresh().await?;
    source.fail_scope("demo", "offline").await;

    let error = catalog.provider_scope("demo").await.unwrap_err();
    assert!(error
        .downcast_ref::<CatalogError>()
        .is_some_and(|error| matches!(error, CatalogError::ScopeRefresh { provider_id, .. } if provider_id == "demo")));
    assert_eq!(source.scope_fetches("demo").await, 3);
    Ok(())
}

#[tokio::test]
async fn scope_download_rejects_a_revision_changed_by_concurrent_global_refresh()
-> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = Arc::new(ProviderCatalog::with_source(
        data_dir.path(),
        Arc::new(source.clone()),
    )?);
    catalog.refresh().await?;

    let gate = source.pause_next_scope_fetch().await;
    let scope_task = {
        let catalog = Arc::clone(&catalog);
        tokio::spawn(async move { catalog.provider_scope("demo").await })
    };
    gate.started.notified().await;

    source.set_version(version("revision-2")).await;
    let refreshed = catalog.refresh().await?;
    assert!(refreshed.changed);
    assert_eq!(refreshed.revision, "revision-2");

    gate.resume.notify_one();
    let error = scope_task
        .await?
        .expect_err("scope must reject its stale revision");
    assert!(matches!(
        error.downcast_ref::<CatalogError>(),
        Some(CatalogError::ScopeRefresh { .. })
    ));
    assert!(
        !data_dir
            .path()
            .join("catalog/scopes/revision-1/demo.json")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn model_source_requires_an_exact_provider_catalog_entry() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    catalog.refresh().await?;

    let error = catalog
        .model_source("demo", "chat-preview")
        .await
        .unwrap_err();

    assert!(error.downcast_ref::<CatalogError>().is_some_and(|error| {
        matches!(
            error,
            CatalogError::EntryNotFound {
                provider_id,
                model_id
            } if provider_id == "demo" && model_id == "chat-preview"
        )
    }));
    Ok(())
}

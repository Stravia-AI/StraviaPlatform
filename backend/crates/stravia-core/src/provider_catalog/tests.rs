use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use super::*;

#[derive(Clone)]
struct ScriptedSource {
    state: Arc<Mutex<SourceState>>,
}

struct SourceState {
    version: CatalogVersion,
    scripted_versions: VecDeque<CatalogVersion>,
    canonical_models: Vec<u8>,
    canonical_model_fetches: u32,
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

    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>> {
        let mut state = self.state.lock().await;
        state.canonical_model_fetches += 1;
        Ok(state.canonical_models.clone())
    }

    async fn fetch_logo(&self, _provider_id: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("logo is not used by this test")
    }

    async fn fetch_favicon(&self, _origin: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("favicon is not used by this test")
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

    async fn canonical_fetches(&self) -> u32 {
        self.state.lock().await.canonical_model_fetches
    }
}

const GENERATED_AT: &str = "2026-08-20T14:01:40Z";

fn version(revision: &str) -> CatalogVersion {
    CatalogVersion {
        revision: revision.to_string(),
        generated_at: GENERATED_AT.to_string(),
    }
}

fn descriptor(provider_id: &str) -> stravia_vendor_sdk::ProviderDescriptor {
    descriptor_with_catalog(
        provider_id,
        Some(provider_id),
        "default",
        "openai-compatible",
        None,
    )
}

fn descriptor_with_catalog(
    provider_id: &str,
    catalog_id: Option<&str>,
    channel_id: &str,
    protocol: &str,
    default_base_url: Option<&str>,
) -> stravia_vendor_sdk::ProviderDescriptor {
    serde_json::from_value(json!({
        "provider_id": provider_id,
        "catalog_id": catalog_id,
        "display_name": provider_id,
        "description": null,
        "channels": [{
            "id": channel_id,
            "name": {"en-US": channel_id},
            "description": null,
            "auth": null,
            "protocol": protocol,
            "default_base_url": default_base_url,
            "capabilities": ["infer"],
            "search_model_required": false
        }],
        "capabilities": ["infer"],
        "config_groups": [],
        "config_fields": [],
        "network": {
            "base_url_field": null,
            "extra_origins": [],
            "field_origins": []
        },
        "data_compat": {
            "config_fields_format": 1,
            "private_state_format": 1,
            "credentials_format": 1,
            "model_metadata_format": 1
        }
    }))
    .expect("test descriptor")
}

fn test_descriptors() -> Vec<stravia_vendor_sdk::ProviderDescriptor> {
    vec![descriptor("demo")]
}

fn demo_providers_body() -> String {
    json!({
      "demo": {
        "id": "demo",
        "name": "Demo AI",
        "npm": "@ai-sdk/openai-compatible",
        "api": "https://demo.invalid/v1",
        "doc": "https://demo.invalid/docs"
      }
    })
    .to_string()
}

fn demo_scope_body() -> Vec<u8> {
    br#"{
      "chat": {
        "canonical_id": "demo/chat",
        "id": "chat",
        "name": "Demo Chat",
        "modalities": { "input": ["text"], "output": ["text"] }
      }
    }"#
    .to_vec()
}

fn source() -> ScriptedSource {
    ScriptedSource {
        state: Arc::new(Mutex::new(SourceState {
            version: version("revision-1"),
            scripted_versions: VecDeque::new(),
            canonical_models: br#"{
              "demo/chat": {
                "id": "demo/chat",
                "name": "Demo Chat",
                "modalities": { "input": ["text"], "output": ["text"] }
              }
            }"#
            .to_vec(),
            canonical_model_fetches: 0,
        })),
    }
}

/// The provider index now arrives through the base guest's `sync-catalog`
/// export; the host only validates and persists the returned body.
async fn install_index(catalog: &ProviderCatalog, revision: &str) -> anyhow::Result<()> {
    install_index_body(catalog, &demo_providers_body(), revision).await
}

async fn install_index_body(
    catalog: &ProviderCatalog,
    body: &str,
    revision: &str,
) -> anyhow::Result<()> {
    catalog
        .install_provider_index(body, revision.to_string(), GENERATED_AT.to_string())
        .await?;
    Ok(())
}

#[tokio::test]
async fn catalog_profiles_bind_to_explicit_catalog_ids() -> anyhow::Result<()> {
    let mappings = [
        ("openai", "@ai-sdk/openai", "open-responses"),
        (
            "openai-compatible",
            "@ai-sdk/openai-compatible",
            "openai-compatible",
        ),
        ("anthropic", "@ai-sdk/anthropic", "anthropic-messages"),
        ("google", "@ai-sdk/google", "google-gemini"),
        ("xai", "@ai-sdk/xai", "openai-compatible"),
        (
            "azure-cognitive-services",
            "@ai-sdk/azure",
            "openai-compatible",
        ),
        ("groq", "@ai-sdk/groq", "openai-compatible"),
        ("cerebras", "@ai-sdk/cerebras", "openai-compatible"),
        ("togetherai", "@ai-sdk/togetherai", "openai-compatible"),
        ("mistral", "@ai-sdk/mistral", "openai-compatible"),
        ("deepinfra", "@ai-sdk/deepinfra", "openai-compatible"),
        ("perplexity", "@ai-sdk/perplexity", "openai-compatible"),
        ("gateway", "@ai-sdk/gateway", "gateway-language-model"),
        ("vercel", "@ai-sdk/vercel", "openai-compatible"),
        ("vertexai", "@ai-sdk/google-vertex", "google-gemini"),
        (
            "vertex-anthropic",
            "@ai-sdk/google-vertex/anthropic",
            "anthropic-messages",
        ),
        (
            "amazon-bedrock",
            "@ai-sdk/amazon-bedrock",
            "bedrock-converse",
        ),
        ("cohere", "@ai-sdk/cohere", "cohere-chat"),
        (
            "openrouter",
            "@openrouter/ai-sdk-provider",
            "openai-compatible",
        ),
        ("watsonx", "watsonx-ai-provider", "watsonx-text-chat"),
        ("venice", "venice-ai-sdk-provider", "openai-compatible"),
        ("aihubmix", "@aihubmix/ai-sdk-provider", "openai-compatible"),
        (
            "sap-ai-core",
            "@jerome-benoit/sap-ai-provider-v2",
            "openai-compatible",
        ),
        ("qvac", "@qvac/ai-sdk-provider", "openai-compatible"),
        (
            "salad-cloud",
            "@saladtechnologies-oss/ai-sdk-provider",
            "openai-compatible",
        ),
        (
            "cloudflare-ai-gateway",
            "ai-gateway-provider",
            "openai-compatible",
        ),
        ("gitlab", "gitlab-ai-provider", "openai-compatible"),
        (
            "merge-gateway",
            "merge-gateway-ai-sdk-provider",
            "openai-compatible",
        ),
    ];
    let providers = mappings
        .iter()
        .map(|(id, npm, _)| {
            (
                (*id).to_string(),
                json!({ "id": id, "name": id, "npm": npm, "api": "" }),
            )
        })
        .collect::<Map<_, _>>();

    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    install_index_body(
        &catalog,
        &Value::Object(providers).to_string(),
        "revision-1",
    )
    .await?;
    let descriptors = mappings
        .iter()
        .map(|(catalog_id, _, protocol)| {
            descriptor_with_catalog(catalog_id, Some(catalog_id), "default", protocol, None)
        })
        .collect::<Vec<_>>();
    let providers = catalog.providers(&descriptors).await;

    for (id, npm, protocol) in mappings {
        let provider = providers
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap_or_else(|| panic!("catalog Provider {id}"));
        assert_eq!(provider.catalog_id.as_deref(), Some(id));
        assert_eq!(provider.npm, npm);
        assert_eq!(provider.protocol, protocol);
    }
    Ok(())
}

#[tokio::test]
async fn dedicated_profiles_reuse_catalog_brands_without_merging_channels() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    install_index_body(
        &catalog,
        &json!({
            "openai": {
                "id": "openai",
                "name": "OpenAI",
                "npm": "@ai-sdk/openai",
                "api": "https://api.openai.com/v1",
                "doc": "https://platform.openai.com/docs"
            },
            "xai": {
                "id": "xai",
                "name": "xAI",
                "npm": "@ai-sdk/xai",
                "api": "https://api.x.ai/v1"
            }
        })
        .to_string(),
        "revision-1",
    )
    .await?;

    let providers = catalog
        .providers(&[
            descriptor_with_catalog("openai", Some("openai"), "default", "open-responses", None),
            descriptor_with_catalog(
                "openai-codex",
                Some("openai"),
                "codex",
                "open-responses",
                Some("https://chatgpt.com/backend-api/codex"),
            ),
            descriptor_with_catalog("xai", Some("xai"), "default", "openai-compatible", None),
            descriptor_with_catalog(
                "xai-grok",
                Some("xai"),
                "grok",
                "open-responses",
                Some("https://cli-chat-proxy.grok.com/v1"),
            ),
        ])
        .await;

    assert_eq!(providers.providers.len(), 4);
    for (provider_id, catalog_id, channel_id) in [
        ("openai", "openai", "default"),
        ("openai-codex", "openai", "codex"),
        ("xai", "xai", "default"),
        ("xai-grok", "xai", "grok"),
    ] {
        let matches = providers
            .providers
            .iter()
            .filter(|provider| provider.id == provider_id)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "profile {provider_id} must not duplicate");
        assert_eq!(matches[0].catalog_id.as_deref(), Some(catalog_id));
        assert_eq!(matches[0].channels.len(), 1);
        assert_eq!(matches[0].channels[0].id, channel_id);
    }
    let codex = providers
        .providers
        .iter()
        .find(|provider| provider.id == "openai-codex")
        .expect("Codex profile");
    assert_eq!(codex.npm, "@ai-sdk/openai");
    assert_eq!(
        codex.documentation_url.as_deref(),
        Some("https://platform.openai.com/docs")
    );

    let scope = catalog
        .install_provider_scope(
            "openai",
            br#"{
              "gpt-5": {
                "id": "gpt-5",
                "name": "GPT-5",
                "modalities": { "input": ["text"], "output": ["text"] }
              }
            }"#,
        )
        .await?;
    let models = catalog.models("openai", "codex", scope).await?;
    assert_eq!(models.models.len(), 1);
    assert_eq!(models.models[0].id, "gpt-5");
    Ok(())
}

#[tokio::test]
async fn global_indexes_commit_independently_for_one_revision() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;

    install_index(&catalog, "revision-1").await?;
    let refreshed = catalog.refresh_canonical().await?;
    let providers = catalog.providers(&test_descriptors()).await;
    let models = catalog.canonical_models().await;

    assert!(refreshed.changed);
    assert_eq!(refreshed.revision, "revision-1");
    assert_eq!(refreshed.generated_at, GENERATED_AT);
    assert_eq!(refreshed.provider_count, 1);
    assert_eq!(refreshed.model_count, 1);
    assert_eq!(providers.revision, models.revision);
    assert_eq!(providers.generated_at, models.generated_at);
    assert_eq!(providers.providers[0].id, "demo");
    assert_eq!(providers.providers[0].catalog_id.as_deref(), Some("demo"));
    assert_eq!(models.models[0].id, "demo/chat");
    assert_eq!(
        catalog
            .canonical_model_matching_upstream_id("chat")
            .await
            .expect("unique model segment")["id"],
        "demo/chat"
    );
    assert_eq!(
        catalog
            .canonical_model_matching_upstream_id("demo/chat")
            .await
            .expect("canonical id")["name"],
        "Demo Chat"
    );
    assert!(
        catalog
            .canonical_model_matching_upstream_id("missing")
            .await
            .is_none()
    );
    assert_eq!(source.canonical_fetches().await, 1);

    let unchanged = catalog.refresh_canonical().await?;
    assert!(!unchanged.changed);
    assert_eq!(source.canonical_fetches().await, 1);
    Ok(())
}

#[tokio::test]
async fn canonical_upstream_id_match_ignores_namespace_and_case() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    source
        .set_canonical_models(
            br#"{
              "minimax/MiniMax-M2.7": { "id": "minimax/MiniMax-M2.7", "name": "MiniMax M2.7" },
              "moonshotai/kimi-k2.5": { "id": "moonshotai/kimi-k2.5", "name": "Kimi K2.5" }
            }"#
            .to_vec(),
        )
        .await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;
    catalog.refresh_canonical().await?;

    // 上游清单的命名空间与 Canonical lab 不必一致,匹配只看最右段。
    assert_eq!(
        catalog
            .canonical_model_matching_upstream_id("MiniMaxAI/MiniMax-M2.7")
            .await
            .expect("mismatched namespace prefix")["name"],
        "MiniMax M2.7"
    );
    // 大小写差异同样归一。
    assert_eq!(
        catalog
            .canonical_model_matching_upstream_id("moonshotai/Kimi-K2.5")
            .await
            .expect("case-insensitive segment")["name"],
        "Kimi K2.5"
    );
    assert!(
        catalog
            .canonical_model_matching_upstream_id("minimax-m2.7")
            .await
            .is_some_and(|template| template["name"] == "MiniMax M2.7")
    );
    Ok(())
}

#[tokio::test]
async fn canonical_upstream_id_match_skips_ambiguous_model_segments() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    source
        .set_canonical_models(
            br#"{
              "demo/chat": { "id": "demo/chat", "name": "Demo Chat" },
              "other/Chat": { "id": "other/Chat", "name": "Other Chat" }
            }"#
            .to_vec(),
        )
        .await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;
    catalog.refresh_canonical().await?;
    // 歧义判定同样走归一键:大小写不同的同名段仍视为多个候选。
    assert!(
        catalog
            .canonical_model_matching_upstream_id("chat")
            .await
            .is_none()
    );
    assert_eq!(
        catalog
            .canonical_model_matching_upstream_id("demo/chat")
            .await
            .expect("exact canonical id")["name"],
        "Demo Chat"
    );
    Ok(())
}

#[tokio::test]
async fn failed_canonical_update_keeps_the_last_known_good_generation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    install_index(&catalog, "revision-1").await?;
    catalog.refresh_canonical().await?;

    source.set_version(version("revision-2")).await;
    source.set_canonical_models(b"[]".to_vec()).await;

    assert!(catalog.refresh_canonical().await.is_err());
    assert_eq!(
        catalog.providers(&test_descriptors()).await.revision,
        "revision-1"
    );
    assert_eq!(catalog.canonical_models().await.models[0].id, "demo/chat");
    Ok(())
}

#[tokio::test]
async fn new_revision_replaces_the_active_generation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    install_index(&catalog, "revision-1").await?;
    catalog.refresh_canonical().await?;

    source.set_version(version("revision-2")).await;
    install_index(&catalog, "revision-2").await?;
    let refreshed = catalog.refresh_canonical().await?;
    let restarted = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;

    assert!(refreshed.changed);
    assert_eq!(
        catalog.providers(&test_descriptors()).await.revision,
        "revision-2"
    );
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

    assert!(catalog.refresh_canonical().await.is_err());
    assert_eq!(
        catalog.providers(&test_descriptors()).await.revision,
        "bootstrap"
    );
    assert!(!data_dir.path().join("catalog/active.json").exists());
    Ok(())
}

#[tokio::test]
async fn unsafe_remote_revision_is_rejected_before_persisting_cache_paths() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;

    source.set_version(version("../../outside-cache")).await;

    assert!(catalog.refresh_canonical().await.is_err());
    assert!(
        catalog
            .install_provider_index(
                &demo_providers_body(),
                "../../outside-cache".to_string(),
                GENERATED_AT.to_string(),
            )
            .await
            .is_err()
    );
    assert!(!data_dir.path().join("catalog/active.json").exists());
    assert_eq!(
        catalog.providers(&test_descriptors()).await.revision,
        "bootstrap"
    );
    Ok(())
}

#[tokio::test]
async fn restart_loads_the_complete_last_known_good_generation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    install_index(&catalog, "revision-1").await?;
    catalog.refresh_canonical().await?;

    let restarted = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;

    assert_eq!(
        restarted.providers(&test_descriptors()).await.revision,
        "revision-1"
    );
    assert_eq!(restarted.canonical_models().await.models[0].id, "demo/chat");
    Ok(())
}

#[tokio::test]
async fn installed_scope_is_cached_under_the_active_revision() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = source();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    install_index(&catalog, "revision-1").await?;

    assert!(catalog.cached_provider_scope("demo").await?.is_none());

    let scope = catalog
        .install_provider_scope("demo", &demo_scope_body())
        .await?;
    assert_eq!(scope.revision, "revision-1");
    assert_eq!(scope.provider_id, "demo");
    assert_eq!(scope.models[0].metadata["id"], "chat");
    assert!(catalog.cached_provider_scope("demo").await?.is_some());

    let restarted = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;
    assert!(restarted.cached_provider_scope("demo").await?.is_some());
    Ok(())
}

#[tokio::test]
async fn installed_scope_is_stamped_with_the_live_revision() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    install_index(&catalog, "revision-1").await?;
    install_index(&catalog, "revision-2").await?;

    let scope = catalog
        .install_provider_scope("demo", &demo_scope_body())
        .await?;
    assert_eq!(scope.revision, "revision-2");
    Ok(())
}

#[tokio::test]
async fn scope_cache_is_discarded_on_corruption_and_scoped_to_the_revision() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    install_index(&catalog, "revision-1").await?;
    catalog
        .install_provider_scope("demo", &demo_scope_body())
        .await?;

    std::fs::write(
        data_dir.path().join("catalog/scopes/revision-1/demo.json"),
        b"not json",
    )?;
    assert!(catalog.cached_provider_scope("demo").await?.is_none());

    catalog
        .install_provider_scope("demo", &demo_scope_body())
        .await?;
    install_index(&catalog, "revision-2").await?;
    // 上一 revision 的 scope 不会在新 revision 下复活。
    assert!(catalog.cached_provider_scope("demo").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn scope_requires_catalog_membership() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    install_index(&catalog, "revision-1").await?;

    let cached = catalog
        .cached_provider_scope("gone-vendor")
        .await
        .unwrap_err();
    assert!(cached.downcast_ref::<CatalogError>().is_some_and(|error| {
        matches!(error, CatalogError::ProviderNotFound { provider_id } if provider_id == "gone-vendor")
    }));
    // 已从索引移除的 provider 不能通过 scope 写入获得成功的刷新结果。
    let installed = catalog
        .install_provider_scope("gone-vendor", &demo_scope_body())
        .await
        .unwrap_err();
    assert!(
        installed
            .downcast_ref::<CatalogError>()
            .is_some_and(|error| matches!(error, CatalogError::ProviderNotFound { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn invalid_scope_body_is_rejected_before_persisting() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    install_index(&catalog, "revision-1").await?;

    assert!(
        catalog
            .install_provider_scope("demo", b"not json")
            .await
            .is_err()
    );
    assert!(catalog.cached_provider_scope("demo").await?.is_none());
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
    install_index(&catalog, "revision-1").await?;
    let scope = catalog
        .install_provider_scope("demo", &demo_scope_body())
        .await?;

    let error = catalog
        .model_source("demo", "chat-preview", scope)
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

// 目录 revision 刷新可能移除已选服务或更换 channel 指纹;创建 Provider 时
// 必须能区分这三类过期,管理面才能给出可恢复的提示而不是裸错误串。
#[tokio::test]
async fn resolve_channel_reports_typed_errors_for_stale_selections() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source()))?;
    install_index(&catalog, "revision-1").await?;

    let missing_provider = catalog
        .resolve_channel("gone-vendor", "default", "fingerprint", &test_descriptors())
        .await
        .unwrap_err();
    assert!(missing_provider.downcast_ref::<CatalogError>().is_some_and(
        |error| matches!(error, CatalogError::ProviderNotFound { provider_id } if provider_id == "gone-vendor")
    ));

    let providers = catalog.providers(&test_descriptors()).await;
    let demo = providers
        .providers
        .iter()
        .find(|provider| provider.id == "demo")
        .expect("demo provider must exist");
    let demo_channel = demo
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .expect("demo default channel must exist");

    let missing_channel = catalog
        .resolve_channel(
            "demo",
            "oauth",
            &demo_channel.fingerprint,
            &test_descriptors(),
        )
        .await
        .unwrap_err();
    assert!(missing_channel.downcast_ref::<CatalogError>().is_some_and(
        |error| matches!(error, CatalogError::ChannelNotFound { provider_id, channel_id }
            if provider_id == "demo" && channel_id == "oauth")
    ));

    let changed = catalog
        .resolve_channel("demo", "default", "stale-fingerprint", &test_descriptors())
        .await
        .unwrap_err();
    assert!(changed.downcast_ref::<CatalogError>().is_some_and(|error| {
        matches!(error, CatalogError::ChannelChanged { provider_id, channel_id }
            if provider_id == "demo" && channel_id == "default")
    }));
    Ok(())
}

// ── Provider icons: logo + website favicon cache semantics ─────────────────

const TEST_SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"></svg>";
const TEST_PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

#[derive(Clone, Default)]
struct IconSource {
    state: Arc<Mutex<IconSourceState>>,
}

#[derive(Default)]
struct IconSourceState {
    logo: Option<Result<Vec<u8>, String>>,
    favicon: Option<Result<Vec<u8>, String>>,
    logo_fetches: u32,
    favicon_fetches: u32,
}

#[async_trait]
impl CatalogSource for IconSource {
    async fn fetch_version(&self) -> anyhow::Result<CatalogVersion> {
        anyhow::bail!("version is not used by icon tests")
    }

    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("canonical models are not used by icon tests")
    }

    async fn fetch_logo(&self, _provider_id: &str) -> anyhow::Result<Vec<u8>> {
        let mut state = self.state.lock().await;
        state.logo_fetches += 1;
        match state.logo.take() {
            Some(Ok(body)) => Ok(body),
            Some(Err(message)) => anyhow::bail!(message),
            None => anyhow::bail!("logo fetch is not scripted"),
        }
    }

    async fn fetch_favicon(&self, _origin: &str) -> anyhow::Result<Vec<u8>> {
        let mut state = self.state.lock().await;
        state.favicon_fetches += 1;
        match state.favicon.take() {
            Some(Ok(body)) => Ok(body),
            Some(Err(message)) => anyhow::bail!(message),
            None => anyhow::bail!("favicon fetch is not scripted"),
        }
    }
}

impl IconSource {
    async fn script_logo(&self, result: Result<Vec<u8>, &str>) {
        self.state.lock().await.logo = Some(result.map_err(|message| message.to_owned()));
    }

    async fn script_favicon(&self, result: Result<Vec<u8>, &str>) {
        self.state.lock().await.favicon = Some(result.map_err(|message| message.to_owned()));
    }

    async fn logo_fetches(&self) -> u32 {
        self.state.lock().await.logo_fetches
    }

    async fn favicon_fetches(&self) -> u32 {
        self.state.lock().await.favicon_fetches
    }
}

fn seed_fresh(path: &std::path::Path, body: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("cache path has a parent"))?;
    std::fs::write(path, body)?;
    Ok(())
}

fn seed_stale(path: &std::path::Path, body: &[u8]) -> anyhow::Result<()> {
    seed_fresh(path, body)?;
    let file = std::fs::File::options().write(true).open(path)?;
    file.set_modified(SystemTime::now() - LOGO_TTL - Duration::from_secs(60))?;
    Ok(())
}

#[tokio::test]
async fn logo_fresh_cache_serves_without_fetching() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    seed_fresh(&logo_path(data_dir.path(), "demo"), TEST_SVG)?;

    let body = catalog.logo("demo").await?;
    assert_eq!(body, TEST_SVG);
    assert_eq!(source.logo_fetches().await, 0);
    Ok(())
}

#[tokio::test]
async fn logo_stale_cache_survives_fetch_failure() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    source.script_logo(Err("logo endpoint is down")).await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    seed_stale(&logo_path(data_dir.path(), "demo"), TEST_SVG)?;

    let body = catalog.logo("demo").await?;
    assert_eq!(body, TEST_SVG);
    assert_eq!(source.logo_fetches().await, 1);
    Ok(())
}

#[tokio::test]
async fn logo_stale_cache_survives_validation_failure() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    source.script_logo(Ok(b"not an image".to_vec())).await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    seed_stale(&logo_path(data_dir.path(), "demo"), TEST_SVG)?;

    let body = catalog.logo("demo").await?;
    assert_eq!(body, TEST_SVG);
    assert_eq!(source.logo_fetches().await, 1);
    Ok(())
}

#[tokio::test]
async fn logo_fetch_failure_without_cache_propagates() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    source.script_logo(Err("logo endpoint is down")).await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;

    assert!(catalog.logo("demo").await.is_err());
    Ok(())
}

#[tokio::test]
async fn logo_fetch_rejects_non_svg_bodies() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    source.script_logo(Ok(b"not an image".to_vec())).await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;

    assert!(catalog.logo("demo").await.is_err());
    assert!(!logo_path(data_dir.path(), "demo").exists());
    Ok(())
}

#[tokio::test]
async fn favicon_fresh_cache_serves_without_fetching() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    let origin = "https://chat.example.com";
    seed_fresh(&favicon_path(data_dir.path(), origin)?, TEST_PNG)?;

    let body = catalog.favicon(origin).await?;
    assert_eq!(body, TEST_PNG);
    assert_eq!(source.favicon_fetches().await, 0);
    Ok(())
}

#[tokio::test]
async fn favicon_stale_cache_survives_fetch_failure() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    source.script_favicon(Err("website is down")).await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    let origin = "https://chat.example.com";
    seed_stale(&favicon_path(data_dir.path(), origin)?, TEST_PNG)?;

    let body = catalog.favicon(origin).await?;
    assert_eq!(body, TEST_PNG);
    assert_eq!(source.favicon_fetches().await, 1);
    Ok(())
}

#[tokio::test]
async fn favicon_fetch_caches_then_serves_from_disk() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    source.script_favicon(Ok(TEST_PNG.to_vec())).await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source.clone()))?;
    let origin = "https://chat.example.com";

    let body = catalog.favicon(origin).await?;
    assert_eq!(body, TEST_PNG);
    assert_eq!(source.favicon_fetches().await, 1);

    let body = catalog.favicon(origin).await?;
    assert_eq!(body, TEST_PNG);
    assert_eq!(source.favicon_fetches().await, 1);
    Ok(())
}

#[tokio::test]
async fn favicon_fetch_failure_without_cache_propagates() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let source = IconSource::default();
    source.script_favicon(Err("website is down")).await;
    let catalog = ProviderCatalog::with_source(data_dir.path(), Arc::new(source))?;

    assert!(catalog.favicon("https://chat.example.com").await.is_err());
    Ok(())
}

#[test]
fn favicon_cache_key_sanitizes_the_origin_and_bounds_length() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let path = favicon_path(data_dir.path(), "https://Chat.Example.COM:8443")?;
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("https---chat.example.com-8443")
    );
    assert!(favicon_path(data_dir.path(), &"x".repeat(200)).is_err());
    Ok(())
}

#[test]
fn icon_content_type_sniffs_supported_images() {
    assert_eq!(
        icon_content_type(&[0x00, 0x00, 0x01, 0x00, 0x10]),
        Some("image/x-icon")
    );
    assert_eq!(icon_content_type(TEST_PNG), Some("image/png"));
    assert_eq!(icon_content_type(b"GIF89a..."), Some("image/gif"));
    assert_eq!(icon_content_type(&[0xff, 0xd8, 0xff]), Some("image/jpeg"));
    assert_eq!(
        icon_content_type(b"RIFF\x04\x00\x00\x00WEBP"),
        Some("image/webp")
    );
    assert_eq!(icon_content_type(TEST_SVG), Some("image/svg+xml"));
    assert_eq!(
        icon_content_type(b"  <?xml version=\"1.0\"?><svg/>"),
        Some("image/svg+xml")
    );
    assert_eq!(icon_content_type(b"plain text"), None);
    assert_eq!(icon_content_type(b""), None);
}

// ── HttpCatalogSource::fetch_favicon against a local server ────────────────

/// Serve one canned HTTP response and report the request line back.
async fn serve_once(
    response: Vec<u8>,
) -> anyhow::Result<(String, tokio::sync::oneshot::Receiver<String>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        let request_line = String::from_utf8_lossy(&request)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        let _ = sender.send(request_line);
        let _ = socket.write_all(&response).await;
    });
    Ok((format!("http://127.0.0.1:{port}"), receiver))
}

fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[tokio::test]
async fn http_favicon_fetches_only_the_origin_favicon_path() -> anyhow::Result<()> {
    let (origin, request) = serve_once(http_response("200 OK", TEST_PNG)).await?;
    let source = HttpCatalogSource::new(None)?;

    // Paths, queries, and trailing segments must normalize to `{origin}/favicon.ico`.
    let body = source
        .fetch_favicon(&format!("{origin}/some/deep/path?query=1"))
        .await?;

    assert_eq!(body, TEST_PNG);
    let request_line = request.await.expect("server observed a request");
    assert!(
        request_line.starts_with("GET /favicon.ico "),
        "{request_line}"
    );
    Ok(())
}

#[tokio::test]
async fn http_favicon_rejects_redirects() -> anyhow::Result<()> {
    let (origin, _request) = serve_once(http_response("302 Found", b"redirect")).await?;
    let source = HttpCatalogSource::new(None)?;
    assert!(source.fetch_favicon(&origin).await.is_err());
    Ok(())
}

#[tokio::test]
async fn http_favicon_rejects_non_image_bodies() -> anyhow::Result<()> {
    let (origin, _request) =
        serve_once(http_response("200 OK", b"<html>not an icon</html>")).await?;
    let source = HttpCatalogSource::new(None)?;
    assert!(source.fetch_favicon(&origin).await.is_err());
    Ok(())
}

#[tokio::test]
async fn http_favicon_rejects_oversized_bodies() -> anyhow::Result<()> {
    let oversized = vec![0x89u8; MAX_LOGO_BYTES + 1];
    let (origin, _request) = serve_once(http_response("200 OK", &oversized)).await?;
    let source = HttpCatalogSource::new(None)?;
    assert!(source.fetch_favicon(&origin).await.is_err());
    Ok(())
}

#[tokio::test]
async fn http_favicon_rejects_non_http_origins() -> anyhow::Result<()> {
    let source = HttpCatalogSource::new(None)?;
    assert!(source.fetch_favicon("ftp://example.com").await.is_err());
    assert!(source.fetch_favicon("not a url").await.is_err());
    Ok(())
}

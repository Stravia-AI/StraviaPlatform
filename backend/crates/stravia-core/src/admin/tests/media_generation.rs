use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::{
    CreateProviderRecord, CreateRoute, CreateTarget, Provider, Route, UpdateRoute,
    UpsertOAuthCredential, UpsertTarget,
};
use crate::media_generation::{ImageGenerationConfig, MediaGenerationConfig};
use crate::provider_models::CreateManualProviderModel;
use serde_json::json;

const CODEX_MODEL: &str = "gpt-5.4";

struct GenerationRouteFixture {
    gateway: Gateway,
    compatible_provider: Provider,
    incompatible_provider: Provider,
    upstream_calls: Arc<AtomicUsize>,
    upstream_task: tokio::task::JoinHandle<()>,
    _data_dir: tempfile::TempDir,
}

impl Drop for GenerationRouteFixture {
    fn drop(&mut self) {
        self.upstream_task.abort();
    }
}

impl GenerationRouteFixture {
    async fn new() -> anyhow::Result<Self> {
        let data_dir = tempfile::tempdir()?;
        let gateway = Gateway::from_storage(
            GatewayConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..GatewayConfig::default()
            },
            Arc::new(crate::storage::MemoryStorage::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        )
        .await?;

        let upstream_calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = upstream_calls.clone();
        let app = axum::Router::new().fallback(move || {
            let observed_calls = observed_calls.clone();
            async move {
                observed_calls.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::NO_CONTENT
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let upstream_url = format!("http://{}", listener.local_addr()?);
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve local generation observer");
        });

        let compatible_provider = gateway
            .storage
            .providers()
            .create(CreateProviderRecord {
                name: "Codex image Provider".into(),
                vendor: Some("openai-codex".into()),
                protocol: "open-responses".into(),
                base_url: upstream_url.clone(),
                preset_key: Some("openai".into()),
                channel: Some("codex".into()),
                models_source: None,
                static_models: None,
                api_key: String::new(),
                adapter_credentials: "{}".into(),
                vendor_options: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await?;
        let incompatible_provider = gateway
            .storage
            .providers()
            .create(CreateProviderRecord {
                name: "Ordinary OpenAI-compatible Provider".into(),
                vendor: Some("protocol-openai-chat-completions".into()),
                protocol: "openai-compatible".into(),
                base_url: upstream_url.clone(),
                preset_key: None,
                channel: Some("default".into()),
                models_source: None,
                static_models: None,
                api_key: "local-test-key".into(),
                adapter_credentials: "{}".into(),
                vendor_options: "{}".into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
            })
            .await?;

        gateway
            .storage
            .oauth_credentials()
            .upsert(
                &compatible_provider.id,
                UpsertOAuthCredential {
                    driver_key: "openai-codex".into(),
                    scheme: "oauth_auth_code_pkce".into(),
                    access_token: "local-test-access-token".into(),
                    resource_url: Some(upstream_url),
                    ..Default::default()
                },
            )
            .await?;

        for provider in [&compatible_provider, &incompatible_provider] {
            gateway
                .admin()
                .create_manual_provider_model(
                    &provider.id,
                    CODEX_MODEL,
                    CreateManualProviderModel {
                        metadata: json!({
                            "id": CODEX_MODEL,
                            "name": "Local test model",
                            "attachment": true,
                            "tool_call": true,
                            "capabilities": ["media_image"],
                            "modalities": {"input": ["text", "image"], "output": ["text"]}
                        }),
                    },
                )
                .await?;
        }

        Ok(Self {
            gateway,
            compatible_provider,
            incompatible_provider,
            upstream_calls,
            upstream_task,
            _data_dir: data_dir,
        })
    }

    async fn create_route(
        &self,
        model_id: &str,
        incompatible_enabled: bool,
    ) -> anyhow::Result<Route> {
        self.gateway
            .admin()
            .create_model(CreateRoute {
                model_id: model_id.into(),
                display_name: Some("Image Route".into()),
                balance: Some("latency_preference".into()),
                target_provider: String::new(),
                target_model: None,
                targets: vec![
                    CreateTarget {
                        provider_id: self.compatible_provider.id.clone(),
                        model: Some(CODEX_MODEL.into()),
                        enabled: true,
                        priority: Some(17),
                        first_token_timeout_ms: Some(1_234),
                        target_retry_budget: Some(2),
                        target_cooldown_ms: Some(5_678),
                        thinking_level_map: Vec::new(),
                    },
                    CreateTarget {
                        provider_id: self.incompatible_provider.id.clone(),
                        model: Some(CODEX_MODEL.into()),
                        enabled: incompatible_enabled,
                        priority: Some(-3),
                        first_token_timeout_ms: Some(4_321),
                        target_retry_budget: Some(1),
                        target_cooldown_ms: Some(8_765),
                        thinking_level_map: Vec::new(),
                    },
                ],
                default_thinking_level: None,
            })
            .await
    }

    fn config(route: &Route, enabled: bool) -> MediaGenerationConfig {
        MediaGenerationConfig {
            enabled,
            image: ImageGenerationConfig {
                route_id: Some(route.model_id.clone()),
            },
        }
    }

    fn targets_with_incompatible_enabled(&self, route: &Route) -> Vec<UpsertTarget> {
        route
            .targets
            .iter()
            .map(|target| UpsertTarget {
                id: Some(target.id.clone()),
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                enabled: target.enabled || target.provider_id == self.incompatible_provider.id,
                priority: Some(target.priority),
                first_token_timeout_ms: Some(target.first_token_timeout_ms),
                target_retry_budget: Some(target.target_retry_budget),
                target_cooldown_ms: Some(target.target_cooldown_ms),
                thinking_level_map: target.thinking_level_map.0.clone(),
            })
            .collect()
    }
}

#[tokio::test]
async fn route_qualification_is_atomic_and_a_stale_binding_can_be_disabled() -> anyhow::Result<()> {
    let fixture = GenerationRouteFixture::new().await?;
    let admin = fixture.gateway.admin();

    let eligible_route = fixture.create_route("public-image-model", false).await?;
    let saved = admin
        .update_media_generation_config(GenerationRouteFixture::config(&eligible_route, true))
        .await?;
    assert!(saved.validation.valid);
    assert_eq!(
        saved.config.image.route_id.as_deref(),
        Some("public-image-model")
    );

    let mixed_route = fixture.create_route("mixed-image-model", true).await?;
    let eligible = admin.list_eligible_media_generation_routes().await?;
    let listed = eligible
        .iter()
        .find(|route| route.id == eligible_route.model_id)
        .expect("eligible Route is listed by its client-facing model ID");
    assert!(
        eligible
            .iter()
            .all(|route| route.id != mixed_route.model_id),
        "a Route with an enabled incompatible Target must not be eligible"
    );
    assert_ne!(listed.id, eligible_route.id, "storage ID must not escape");
    assert_eq!(listed.name.as_deref(), Some("Image Route"));

    let persisted_route = admin.get_model(&eligible_route.model_id).await?;
    assert_eq!(persisted_route.balance, "latency_preference");
    let primary = persisted_route
        .targets
        .iter()
        .find(|target| target.provider_id == fixture.compatible_provider.id)
        .expect("image-capable Target");
    assert_eq!(
        (
            primary.priority,
            primary.first_token_timeout_ms,
            primary.target_retry_budget,
            primary.target_cooldown_ms,
        ),
        (17, 1_234, 2, 5_678)
    );
    let standby = persisted_route
        .targets
        .iter()
        .find(|target| target.provider_id == fixture.incompatible_provider.id)
        .expect("disabled incompatible Target");
    assert!(!standby.enabled);
    assert_eq!(
        (
            standby.priority,
            standby.first_token_timeout_ms,
            standby.target_retry_budget,
            standby.target_cooldown_ms,
        ),
        (-3, 4_321, 1, 8_765)
    );
    assert_eq!(fixture.upstream_calls.load(Ordering::SeqCst), 0);

    let error = admin
        .update_media_generation_config(GenerationRouteFixture::config(&mixed_route, true))
        .await
        .expect_err("an enabled incompatible Target must reject the whole binding");
    assert_eq!(error.code, "media_generation_target_incompatible");
    let after_rejection = admin.get_media_generation_config().await?;
    assert!(after_rejection.config.enabled);
    assert_eq!(
        after_rejection.config.image.route_id.as_deref(),
        Some("public-image-model"),
        "a rejected binding must not overwrite the active configuration"
    );
    assert!(after_rejection.validation.valid);

    admin
        .update_model(
            &eligible_route.model_id,
            UpdateRoute {
                targets: Some(fixture.targets_with_incompatible_enabled(&persisted_route)),
                ..Default::default()
            },
        )
        .await?;
    let stale = admin.get_media_generation_config().await?;
    assert!(!stale.validation.valid);
    assert_eq!(
        stale.validation.code,
        Some("media_generation_target_incompatible")
    );

    let disabled = admin
        .update_media_generation_config(GenerationRouteFixture::config(&eligible_route, false))
        .await?;
    assert!(!disabled.config.enabled);
    assert_eq!(
        disabled.config.image.route_id.as_deref(),
        Some("public-image-model")
    );
    let persisted_disabled = admin.get_media_generation_config().await?;
    assert!(!persisted_disabled.config.enabled);
    assert_eq!(fixture.upstream_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

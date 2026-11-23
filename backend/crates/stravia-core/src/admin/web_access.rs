use std::collections::HashSet;

use chrono::Utc;
use stravia_web_access::WebProviderKindDescriptor;

use super::*;
use crate::storage::traits::{ProviderTestResult, WebProviderStore};

impl AdminService {
    /// 返回当前构建中通过 `GatewayBuilder::web_provider_factory` 注册的全部 Web Provider 类型描述。
    pub fn list_web_provider_kinds(&self) -> Vec<WebProviderKindDescriptor> {
        self.gw.web_provider_registry().descriptors()
    }

    pub async fn list_web_providers(&self) -> anyhow::Result<Vec<WebProvider>> {
        self.web_provider_store()?.list().await
    }

    pub async fn list_web_provider_views(&self) -> anyhow::Result<Vec<WebProviderView>> {
        let providers = self.list_web_providers().await?;
        let mut views = Vec::with_capacity(providers.len());
        for provider in providers {
            views.push(self.web_provider_view(provider).await);
        }
        Ok(views)
    }

    pub async fn get_web_provider(&self, id: &str) -> anyhow::Result<WebProvider> {
        self.web_provider_store()?
            .get(id)
            .await?
            .context("Web Provider not found")
    }

    pub async fn get_web_provider_view(&self, id: &str) -> anyhow::Result<WebProviderView> {
        let provider = self.get_web_provider(id).await?;
        Ok(self.web_provider_view(provider).await)
    }

    pub async fn web_provider_view(&self, provider: WebProvider) -> WebProviderView {
        let factory = self.gw.web_provider_registry().get(&provider.kind);
        let api_key_configured = provider
            .api_key
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        let (registered, available, unavailable_reason, capabilities, allow_delete, config) =
            if let Some(factory) = factory {
                let descriptor = factory.descriptor();
                let available = factory.is_available().await;
                let unavailable_reason = if available {
                    None
                } else {
                    Some(factory.unavailable_reason().1)
                };
                let config = factory.public_config_view(provider.config_json.as_deref());
                (
                    true,
                    available,
                    unavailable_reason,
                    Some(descriptor.capabilities),
                    descriptor.allow_delete,
                    config,
                )
            } else {
                (
                    false,
                    false,
                    Some(format!(
                        "Web Provider kind '{}' is not registered in this build",
                        provider.kind
                    )),
                    None,
                    true,
                    None,
                )
            };
        WebProviderView {
            id: provider.id,
            name: provider.name,
            kind: provider.kind,
            api_key_configured,
            use_proxy: provider.use_proxy,
            config,
            capabilities,
            registered,
            available,
            unavailable_reason,
            allow_delete,
            last_test_success: provider.last_test_success,
            last_test_at: provider.last_test_at,
            created_at: provider.created_at,
            updated_at: provider.updated_at,
        }
    }

    pub async fn create_web_provider(
        &self,
        mut input: CreateWebProvider,
    ) -> anyhow::Result<WebProvider> {
        let api_key_supplied = input.api_key.is_some();
        input.name = normalize_name(&input.name, "Web Provider name")?;
        input.kind = input.kind.trim().to_ascii_lowercase();
        let factory = self
            .gw
            .web_provider_registry()
            .get(&input.kind)
            .with_context(|| format!("unsupported Web Provider kind: {}", input.kind))?;
        let descriptor = factory.descriptor();
        input.api_key = normalize_optional_text(input.api_key);
        if descriptor.singleton
            && self
                .web_provider_store()?
                .list()
                .await?
                .iter()
                .any(|provider| provider.kind == input.kind)
        {
            return Err(coded_error(
                "WEB_PROVIDER_SINGLETON_CONFLICT",
                &format!("Web Provider of kind '{}' already exists", input.kind),
                serde_json::json!({ "kind": input.kind }),
            ));
        }
        let config_supplied = input.config.is_some();
        self.ensure_web_provider_name_unique(None, &input.name)
            .await?;
        if input.config.is_none() {
            input.config = factory.default_config();
        }
        validate_config_object(input.config.as_ref())?;
        factory
            .validate_config(
                input.api_key.as_deref(),
                api_key_supplied,
                config_supplied,
                input.config.as_mut(),
            )
            .map_err(anyhow::Error::msg)?;
        validate_config_object(input.config.as_ref())?;
        let provider = self.web_provider_store()?.create(input).await?;
        crate::storage::bump_config_epoch(self.gw.storage.settings()).await?;
        Ok(provider)
    }

    pub async fn update_web_provider(
        &self,
        id: &str,
        mut input: UpdateWebProvider,
    ) -> anyhow::Result<WebProvider> {
        let current = self.get_web_provider(id).await?;
        let factory = self
            .gw
            .web_provider_registry()
            .get(&current.kind)
            .with_context(|| format!("unsupported Web Provider kind: {}", current.kind))?;
        let api_key_supplied = input.api_key.is_some();
        let config_supplied = input.config.is_some();
        let name = normalize_name(
            input.name.as_deref().unwrap_or(&current.name),
            "Web Provider name",
        )?;
        input.api_key = input.api_key.map(normalize_optional_text);
        self.ensure_web_provider_name_unique(Some(id), &name)
            .await?;
        let api_key = match input.api_key.as_ref() {
            Some(value) => value.as_deref(),
            None => current.api_key.as_deref(),
        };
        let mut merged_config = match input.config.take() {
            Some(Some(incoming)) => {
                validate_config_object(Some(&incoming))?;
                Some(factory.merge_config(current.config_json.as_deref(), incoming))
            }
            Some(None) => None,
            None => current.config_json.as_ref().map(|value| value.0.clone()),
        };
        validate_config_object(merged_config.as_ref())?;
        factory
            .validate_config(
                api_key,
                api_key_supplied,
                config_supplied,
                merged_config.as_mut(),
            )
            .map_err(anyhow::Error::msg)?;
        validate_config_object(merged_config.as_ref())?;
        input.name = Some(name);
        input.config = config_supplied.then_some(merged_config);
        let provider = self.web_provider_store()?.update(id, input).await?;
        crate::storage::bump_config_epoch(self.gw.storage.settings()).await?;
        Ok(provider)
    }

    pub async fn delete_web_provider(&self, id: &str) -> anyhow::Result<()> {
        let provider = self.get_web_provider(id).await?;
        if let Some(factory) = self.gw.web_provider_registry().get(&provider.kind)
            && !factory.descriptor().allow_delete
        {
            return Err(coded_error(
                "WEB_PROVIDER_DELETE_FORBIDDEN",
                &format!(
                    "Remove '{}' from the Search or Fetch list instead of deleting it",
                    provider.name
                ),
                serde_json::json!({ "id": id, "kind": provider.kind }),
            ));
        }
        self.web_provider_store()?.delete(id).await?;
        crate::storage::bump_config_epoch(self.gw.storage.settings()).await?;
        Ok(())
    }

    pub async fn test_web_provider(&self, id: &str) -> anyhow::Result<TestResult> {
        let provider = self.get_web_provider(id).await?;
        let tested_at = Utc::now().to_rfc3339();
        let started = std::time::Instant::now();
        let outcome = self.gw.web_access().test_provider(provider).await;
        self.web_provider_store()?
            .record_test_result(
                id,
                ProviderTestResult {
                    success: outcome.is_ok(),
                    tested_at,
                },
            )
            .await?;
        let latency_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(()) => Ok(TestResult {
                success: true,
                latency_ms,
                model: None,
                error: None,
            }),
            Err(error) => Ok(TestResult {
                success: false,
                latency_ms,
                model: None,
                error: Some(error.to_string()),
            }),
        }
    }

    pub async fn get_web_access_settings(&self) -> anyhow::Result<WebAccessSettings> {
        self.gw.web_access().settings().await
    }

    pub async fn update_web_access_settings(
        &self,
        settings: WebAccessSettings,
    ) -> anyhow::Result<WebAccessSettings> {
        ensure_unique_ids(&settings.search_provider_ids, "Search")?;
        ensure_unique_ids(&settings.fetch_provider_ids, "Fetch")?;
        let store = self.web_provider_store()?;
        let current = store.load_settings().await?;
        let providers = store.list().await?;
        self.validate_selected_providers(
            &providers,
            &settings.search_provider_ids,
            &current.search_provider_ids,
            true,
        )
        .await?;
        self.validate_selected_providers(
            &providers,
            &settings.fetch_provider_ids,
            &current.fetch_provider_ids,
            false,
        )
        .await?;
        store.save_settings(&settings).await?;
        crate::storage::bump_config_epoch(self.gw.storage.settings()).await?;
        Ok(settings)
    }

    async fn validate_selected_providers(
        &self,
        providers: &[WebProvider],
        next_ids: &[String],
        current_ids: &[String],
        search: bool,
    ) -> anyhow::Result<()> {
        let registry = self.gw.web_provider_registry();
        for id in next_ids {
            let provider = providers
                .iter()
                .find(|provider| provider.id == *id)
                .with_context(|| format!("Web Provider not found: {id}"))?;
            let factory = registry
                .get(&provider.kind)
                .with_context(|| format!("unsupported Web Provider kind: {}", provider.kind))?;
            let capabilities = factory.descriptor().capabilities;
            if (search && !capabilities.search) || (!search && !capabilities.fetch) {
                anyhow::bail!(
                    "Web Provider {} does not support {}",
                    provider.name,
                    if search { "Search" } else { "Fetch" }
                );
            }
            if !current_ids.contains(id) && !factory.is_available().await {
                let (code, reason) = factory.unavailable_reason();
                return Err(coded_error(
                    code,
                    &reason,
                    serde_json::json!({ "id": provider.id, "kind": provider.kind }),
                ));
            }
        }
        Ok(())
    }

    fn web_provider_store(&self) -> anyhow::Result<&dyn WebProviderStore> {
        self.gw
            .storage
            .web_providers()
            .context("Web Provider storage is unavailable")
    }

    async fn ensure_web_provider_name_unique(
        &self,
        exclude_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()> {
        if self
            .web_provider_store()?
            .exists_by_name(name, exclude_id)
            .await?
        {
            return Err(coded_error(
                "WEB_PROVIDER_NAME_CONFLICT",
                &format!("Web Provider name already exists: {name}"),
                serde_json::json!({ "name": name }),
            ));
        }
        Ok(())
    }
}

fn validate_config_object(config: Option<&Value>) -> anyhow::Result<()> {
    if let Some(value) = config
        && !value.is_object()
    {
        anyhow::bail!("Web Provider configuration must be a JSON object");
    }
    Ok(())
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn ensure_unique_ids(ids: &[String], capability: &str) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    if let Some(id) = ids.iter().find(|id| !seen.insert(id.as_str())) {
        anyhow::bail!("duplicate {capability} Web Provider ID: {id}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;
    use crate::test_support::StubWebProviderFactory;

    #[tokio::test]
    async fn default_gateway_has_no_builtin_web_provider_kinds_and_rejects_unregistered_create() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let gateway = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");

        let admin = gateway.admin();
        assert!(admin.list_web_provider_kinds().is_empty());
        assert!(admin.list_web_providers().await.unwrap().is_empty());
        assert_eq!(
            admin.get_web_access_settings().await.unwrap(),
            WebAccessSettings::default()
        );

        let err = admin
            .create_web_provider(CreateWebProvider {
                name: "Unregistered".into(),
                kind: "local".into(),
                api_key: None,
                use_proxy: false,
                config: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unsupported Web Provider kind"));
    }

    #[tokio::test]
    async fn registered_factory_supports_crud_availability_and_singleton_rules() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let available = Arc::new(AtomicBool::new(false));
        let worker_factory = Arc::new(
            StubWebProviderFactory::new("worker-web", true, true)
                .with_requires_api_key(false)
                .with_singleton(true)
                .with_allow_delete(false)
                .with_availability(available.clone(), "Worker is offline"),
        );
        let external_factory = Arc::new(StubWebProviderFactory::new("remote-search", true, true));

        let gateway = crate::GatewayBuilder::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .web_provider_factory(worker_factory)
        .web_provider_factory(external_factory)
        .build()
        .await
        .expect("gateway");

        let admin = gateway.admin();
        let kinds = admin.list_web_provider_kinds();
        assert_eq!(kinds.len(), 2);

        let worker = admin
            .create_web_provider(CreateWebProvider {
                name: "Worker".into(),
                kind: "worker-web".into(),
                api_key: None,
                use_proxy: false,
                config: Some(serde_json::json!({"endpoint": "http://127.0.0.1:9000"})),
            })
            .await
            .expect("create singleton worker provider");

        // Singleton conflict rejects second instance of the same kind.
        let dup_err = admin
            .create_web_provider(CreateWebProvider {
                name: "Worker 2".into(),
                kind: "worker-web".into(),
                api_key: None,
                use_proxy: false,
                config: None,
            })
            .await
            .unwrap_err();
        assert!(
            dup_err
                .to_string()
                .contains("WEB_PROVIDER_SINGLETON_CONFLICT")
        );

        // Non-deletable kind cannot be deleted.
        let del_err = admin.delete_web_provider(&worker.id).await.unwrap_err();
        assert!(
            del_err
                .to_string()
                .contains("WEB_PROVIDER_DELETE_FORBIDDEN")
        );

        // Unavailable provider cannot be newly added to Web Access settings.
        let unavail_err = admin
            .update_web_access_settings(WebAccessSettings {
                search_provider_ids: vec![worker.id.clone()],
                fetch_provider_ids: vec![],
            })
            .await
            .unwrap_err();
        assert!(unavail_err.to_string().contains("WEB_PROVIDER_UNAVAILABLE"));

        // Once available, selection succeeds and view reflects availability.
        available.store(true, Ordering::SeqCst);
        let view = admin.get_web_provider_view(&worker.id).await.unwrap();
        assert!(view.registered);
        assert!(view.available);
        assert!(!view.allow_delete);
        assert_eq!(
            view.capabilities,
            Some(WebProviderCapabilities {
                search: true,
                fetch: true,
            })
        );
        admin
            .update_web_access_settings(WebAccessSettings {
                search_provider_ids: vec![worker.id.clone()],
                fetch_provider_ids: vec![worker.id.clone()],
            })
            .await
            .unwrap();

        // When worker later goes offline, removing it from settings still works cleanly.
        available.store(false, Ordering::SeqCst);
        admin
            .update_web_access_settings(WebAccessSettings::default())
            .await
            .unwrap();

        // Standard API-key provider enforces key presence and supports deletion.
        let missing_key = admin
            .create_web_provider(CreateWebProvider {
                name: "Remote".into(),
                kind: "remote-search".into(),
                api_key: None,
                use_proxy: false,
                config: None,
            })
            .await
            .unwrap_err();
        assert!(missing_key.to_string().contains("API key is required"));

        let remote = admin
            .create_web_provider(CreateWebProvider {
                name: "Remote".into(),
                kind: "remote-search".into(),
                api_key: Some("secret".into()),
                use_proxy: false,
                config: None,
            })
            .await
            .unwrap();
        admin.delete_web_provider(&remote.id).await.unwrap();
    }

    #[tokio::test]
    async fn unregistered_legacy_rows_surface_as_unregistered_and_remain_deletable() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let gateway = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");
        let store = gateway.storage.web_providers().unwrap();
        let legacy = store
            .create(CreateWebProvider {
                name: "Legacy Exa".into(),
                kind: "exa".into(),
                api_key: Some("legacy-secret".into()),
                use_proxy: false,
                config: None,
            })
            .await
            .unwrap();

        let admin = gateway.admin();
        let views = admin.list_web_provider_views().await.unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, legacy.id);
        assert!(!views[0].registered);
        assert!(!views[0].available);
        assert!(views[0].allow_delete);
        assert!(views[0].api_key_configured);
        assert!(views[0].unavailable_reason.is_some());

        // Legacy unregistered provider can be cleanly deleted.
        admin.delete_web_provider(&legacy.id).await.unwrap();
        assert!(admin.list_web_providers().await.unwrap().is_empty());
    }
}

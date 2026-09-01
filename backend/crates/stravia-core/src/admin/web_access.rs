use std::collections::HashSet;

use chrono::Utc;

use super::*;
use crate::storage::traits::{ProviderTestResult, WebProviderStore};

impl AdminService {
    pub async fn list_web_providers(&self) -> anyhow::Result<Vec<WebProvider>> {
        self.web_provider_store()?.list().await
    }

    pub async fn get_web_provider(&self, id: &str) -> anyhow::Result<WebProvider> {
        self.web_provider_store()?
            .get(id)
            .await?
            .context("Web Provider not found")
    }

    pub async fn create_web_provider(
        &self,
        mut input: CreateWebProvider,
    ) -> anyhow::Result<WebProvider> {
        let api_key_supplied = input.api_key.is_some();
        input.name = normalize_name(&input.name, "Web Provider name")?;
        input.kind = normalize_web_provider_kind(&input.kind)?;
        input.api_key = normalize_optional_text(input.api_key);
        if input.kind == "local"
            && self
                .web_provider_store()?
                .list()
                .await?
                .iter()
                .any(|provider| provider.kind == "local")
        {
            return Err(coded_error(
                "WEB_PROVIDER_LOCAL_SINGLETON",
                "Local Web Provider already exists",
                serde_json::json!({}),
            ));
        }
        self.ensure_web_provider_name_unique(None, &input.name)
            .await?;
        if input.kind == "local" && input.local_engines.is_none() {
            input.local_engines = Some(default_local_search_engines());
        }
        Self::validate_web_provider_input(
            &input.kind,
            input.api_key.as_deref(),
            api_key_supplied,
            input.local_engines.is_some(),
            input.local_engines.as_mut(),
        )?;
        let provider = self.web_provider_store()?.create(input).await?;
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn update_web_provider(
        &self,
        id: &str,
        mut input: UpdateWebProvider,
    ) -> anyhow::Result<WebProvider> {
        let current = self.get_web_provider(id).await?;
        let api_key_supplied = input.api_key.is_some();
        let local_engines_supplied = input.local_engines.is_some();
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
        let mut local_engines = match input.local_engines.take() {
            Some(Some(update)) => Some(merge_local_engines(
                current.local_engines.as_deref(),
                update,
            )),
            Some(None) => None,
            None => current
                .local_engines
                .as_ref()
                .map(|engines| engines.0.clone()),
        };
        Self::validate_web_provider_input(
            &current.kind,
            api_key,
            api_key_supplied,
            local_engines_supplied,
            local_engines.as_mut(),
        )?;
        input.name = Some(name);
        input.local_engines = local_engines_supplied.then_some(local_engines);
        let provider = self.web_provider_store()?.update(id, input).await?;
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn delete_web_provider(&self, id: &str) -> anyhow::Result<()> {
        let provider = self.get_web_provider(id).await?;
        if provider.kind == "local" {
            return Err(coded_error(
                "WEB_PROVIDER_LOCAL_DELETE_FORBIDDEN",
                "Remove Local from the Search or Fetch list instead of deleting it",
                serde_json::json!({ "id": id }),
            ));
        }
        self.web_provider_store()?.delete(id).await?;
        self.bump_config_epoch().await?;
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
        self.web_provider_store()?.save_settings(&settings).await?;
        self.bump_config_epoch().await?;
        Ok(settings)
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

    fn validate_web_provider_input(
        kind: &str,
        api_key: Option<&str>,
        api_key_supplied: bool,
        local_engines_supplied: bool,
        local_engines: Option<&mut LocalSearchEngineConfigs>,
    ) -> anyhow::Result<()> {
        match kind {
            "local" => {
                if api_key_supplied {
                    anyhow::bail!("Local Web Provider does not accept an API key");
                }
                validate_local_engines(
                    local_engines.context("Local Search Engine configuration is required")?,
                )?;
            }
            "exa" | "zhipu" => {
                if api_key
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_none()
                {
                    anyhow::bail!("API key is required for {kind}");
                }
                if local_engines_supplied {
                    anyhow::bail!("{kind} does not accept Local Search Engine configuration");
                }
            }
            _ => anyhow::bail!("unsupported Web Provider kind: {kind}"),
        }
        Ok(())
    }
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_web_provider_kind(kind: &str) -> anyhow::Result<String> {
    let kind = kind.trim().to_ascii_lowercase();
    if matches!(kind.as_str(), "local" | "exa" | "zhipu") {
        Ok(kind)
    } else {
        anyhow::bail!("unsupported Web Provider kind: {kind}")
    }
}

fn merge_local_engines(
    current: Option<&LocalSearchEngineConfigs>,
    updates: LocalSearchEngineConfigs,
) -> LocalSearchEngineConfigs {
    let mut merged = current
        .cloned()
        .unwrap_or_else(default_local_search_engines);
    for (id, mut update) in updates {
        if update.private_settings.is_none() {
            update.private_settings = merged
                .get(&id)
                .and_then(|engine| engine.private_settings.clone());
        }
        merged.insert(id, update);
    }
    merged
}

fn validate_local_engines(engines: &mut LocalSearchEngineConfigs) -> anyhow::Result<()> {
    const ENGINE_IDS: [&str; 7] = [
        "google",
        "bing",
        "brave",
        "baidu",
        "360",
        "sogou_weixin",
        "google_scholar",
    ];
    for (id, config) in engines.iter_mut() {
        if !ENGINE_IDS.contains(&id.as_str()) {
            anyhow::bail!("unknown Local Search Engine: {id}");
        }
        let private_settings = config.private_settings.get_or_insert_default();
        for key in private_settings.keys() {
            if key != "cookies" {
                anyhow::bail!("unknown private setting for {id}: {key}");
            }
        }
    }
    if !engines.values().any(|engine| engine.enabled) {
        anyhow::bail!("at least one Local Search Engine must be enabled");
    }
    Ok(())
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
    use super::*;

    #[tokio::test]
    async fn creates_zhipu_provider_with_search_and_fetch_capabilities() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");

        let provider = gateway
            .admin()
            .create_web_provider(CreateWebProvider {
                name: "Zhipu".into(),
                kind: "zhipu".into(),
                api_key: Some("secret".into()),
                use_proxy: false,
                local_engines: None,
            })
            .await
            .expect("Zhipu Web Provider");

        assert_eq!(provider.kind, "zhipu");
        assert_eq!(
            provider.capabilities(),
            Some(WebProviderCapabilities {
                search: true,
                fetch: true,
            })
        );
    }

    #[tokio::test]
    async fn seeded_local_provider_is_a_non_deletable_singleton() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");
        let admin = gateway.admin();

        let local = admin
            .list_web_providers()
            .await
            .expect("Web Providers")
            .into_iter()
            .find(|provider| provider.kind == "local")
            .expect("seeded Local Web Provider");
        assert_eq!(
            local.capabilities(),
            Some(WebProviderCapabilities {
                search: true,
                fetch: true,
            })
        );

        let duplicate_error = admin
            .create_web_provider(CreateWebProvider {
                name: "Another Local".into(),
                kind: "local".into(),
                api_key: None,
                use_proxy: false,
                local_engines: None,
            })
            .await
            .expect_err("second Local Web Provider must be rejected");
        assert!(
            duplicate_error
                .to_string()
                .contains("WEB_PROVIDER_LOCAL_SINGLETON")
        );

        let delete_error = admin
            .delete_web_provider(&local.id)
            .await
            .expect_err("Local Web Provider deletion must be rejected");
        assert!(
            delete_error
                .to_string()
                .contains("WEB_PROVIDER_LOCAL_DELETE_FORBIDDEN")
        );
    }

    #[tokio::test]
    async fn validates_local_engines_proxy_and_secret_non_echo() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");
        let admin = gateway.admin();
        let local = admin
            .list_web_providers()
            .await
            .expect("Web Providers")
            .into_iter()
            .find(|provider| provider.kind == "local")
            .expect("Local Web Provider");

        assert!(!local.use_proxy);
        let engines = local.local_engines.as_deref().expect("Local engines");
        for id in ["google", "bing", "brave", "baidu"] {
            assert!(engines[id].enabled, "{id}");
        }
        for id in ["360", "sogou_weixin", "google_scholar"] {
            assert!(!engines[id].enabled, "{id}");
        }

        let update = serde_json::from_value::<UpdateWebProvider>(serde_json::json!({
            "name": "On-device Web",
            "use_proxy": true,
            "local_engines": {
                "google": {
                    "enabled": false,
                    "private_settings": {
                        "cookies": "SID=private-session"
                    }
                }
            }
        }))
        .expect("Local update");
        let updated = admin
            .update_web_provider(&local.id, update)
            .await
            .expect("updated Local Web Provider");
        assert_eq!(updated.name, "On-device Web");
        assert!(updated.use_proxy);
        assert!(!updated.local_engines.as_deref().unwrap()["google"].enabled);
        assert_eq!(
            updated.local_engines.as_deref().unwrap()["google"]
                .private_settings
                .as_ref()
                .and_then(|settings| settings.get("cookies"))
                .map(String::as_str),
            Some("SID=private-session")
        );
        let serialized = serde_json::to_string(&updated).expect("serialized Web Provider");
        assert!(!serialized.contains("SID=private-session"));
        assert!(!serialized.contains("private_settings"));
        let test_result = admin
            .test_web_provider(&local.id)
            .await
            .expect("Local connectivity test result");
        assert!(!test_result.success);
        assert!(
            test_result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("proxy_url"))
        );

        for invalid in [
            serde_json::json!({
                "local_engines": {
                    "unknown": { "enabled": true }
                }
            }),
            serde_json::json!({
                "local_engines": {
                    "google": {
                        "enabled": true,
                        "private_settings": { "typo": "secret" }
                    }
                }
            }),
            serde_json::json!({
                "local_engines": {
                    "google": { "enabled": false },
                    "bing": { "enabled": false },
                    "brave": { "enabled": false },
                    "baidu": { "enabled": false },
                    "360": { "enabled": false },
                    "sogou_weixin": { "enabled": false },
                    "google_scholar": { "enabled": false }
                }
            }),
            serde_json::json!({ "api_key": "not-allowed" }),
        ] {
            let input =
                serde_json::from_value::<UpdateWebProvider>(invalid).expect("invalid update shape");
            assert!(
                admin.update_web_provider(&local.id, input).await.is_err(),
                "invalid Local update must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn accepts_only_configured_exa_and_zhipu_remote_providers() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");
        let admin = gateway.admin();

        for kind in ["exa", "zhipu"] {
            let provider = admin
                .create_web_provider(CreateWebProvider {
                    name: kind.to_ascii_uppercase(),
                    kind: kind.into(),
                    api_key: Some("secret".into()),
                    use_proxy: true,
                    local_engines: None,
                })
                .await
                .expect("remote Web Provider");
            assert!(provider.use_proxy);
            assert!(provider.local_engines.is_none());
        }

        for kind in ["brave", "tavily"] {
            let error = admin
                .create_web_provider(CreateWebProvider {
                    name: format!("Removed {kind}"),
                    kind: kind.into(),
                    api_key: Some("secret".into()),
                    use_proxy: false,
                    local_engines: None,
                })
                .await
                .expect_err("removed Web Provider kind");
            assert!(error.to_string().contains("unsupported Web Provider kind"));
        }

        let missing_key = admin
            .create_web_provider(CreateWebProvider {
                name: "Empty Exa".into(),
                kind: "exa".into(),
                api_key: None,
                use_proxy: false,
                local_engines: None,
            })
            .await
            .expect_err("remote API key is required");
        assert!(missing_key.to_string().contains("API key is required"));

        let remote_engines = admin
            .create_web_provider(CreateWebProvider {
                name: "Configured Exa".into(),
                kind: "exa".into(),
                api_key: Some("secret".into()),
                use_proxy: false,
                local_engines: Some(default_local_search_engines()),
            })
            .await
            .expect_err("remote Local Search Engines must be rejected");
        assert!(
            remote_engines
                .to_string()
                .contains("does not accept Local Search Engine")
        );
    }
}

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
        input.name = normalize_name(&input.name, "Web Provider name")?;
        input.kind = normalize_web_provider_kind(&input.kind)?;
        input.api_key = normalize_optional_text(input.api_key);
        self.ensure_web_provider_name_unique(None, &input.name)
            .await?;
        Self::validate_web_provider_input(&input.kind, input.api_key.as_deref())?;
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
        Self::validate_web_provider_input(&current.kind, api_key)?;
        input.name = Some(name);
        let provider = self.web_provider_store()?.update(id, input).await?;
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn delete_web_provider(&self, id: &str) -> anyhow::Result<()> {
        self.get_web_provider(id).await?;
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

    fn validate_web_provider_input(kind: &str, api_key: Option<&str>) -> anyhow::Result<()> {
        match kind {
            "exa" | "brave" | "tavily" | "zhipu" => {
                if api_key
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_none()
                {
                    anyhow::bail!("API key is required for {kind}");
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
    if matches!(kind.as_str(), "exa" | "brave" | "tavily" | "zhipu") {
        Ok(kind)
    } else {
        anyhow::bail!("unsupported Web Provider kind: {kind}")
    }
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
}

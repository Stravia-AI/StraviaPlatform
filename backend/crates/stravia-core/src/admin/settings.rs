use super::*;

fn reject_removed_proxy_setting(key: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !matches!(
            key,
            "proxy_enabled"
                | "proxy_url"
                | "proxy_bypass"
                | "proxy_force_http1"
                | "update_use_proxy"
        ),
        "removed proxy setting; use outbound_proxy or update_proxy_mode instead"
    );
    Ok(())
}

fn reject_managed_or_reserved_setting(key: &str) -> anyhow::Result<()> {
    if key == "artifact_upload_signing_key" {
        anyhow::bail!("reserved internal setting");
    }
    if key == crate::storage::CUSTOM_CURRENCY_SETTING_KEY {
        anyhow::bail!("custom_currency is managed through the currency API");
    }
    if key == crate::platform_llm::SETTINGS_KEY {
        anyhow::bail!("platform_llm_config is managed through the platform LLM API");
    }
    if key == crate::admin::updates::UPDATE_CHANNEL_KEY {
        anyhow::bail!("product_update_channel is managed through the updates API");
    }
    reject_removed_proxy_setting(key)?;
    if key == stravia_credential_protection::policy::DISABLED_RULES_KEY {
        anyhow::bail!("{key} is managed through the credential rules API");
    }
    Ok(())
}

impl AdminService {
    // ── Settings ──

    pub async fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        reject_managed_or_reserved_setting(key)?;
        if key == crate::outbound_proxy::SETTINGS_KEY {
            let config =
                crate::outbound_proxy::OutboundProxySettings::load(self.gw.storage.settings())
                    .await?;
            return Ok(Some(serde_json::to_string(&config)?));
        }
        if key == crate::outbound_proxy::UPDATE_PROXY_MODE_KEY {
            return Ok(Some(
                crate::outbound_proxy::update_proxy_mode(self.gw.storage.settings())
                    .await?
                    .as_str()
                    .to_owned(),
            ));
        }
        let value = self.gw.storage.settings().get(key).await?;
        if key == crate::rpm::SETTINGS_KEY {
            let config = crate::rpm::RpmConfig::from_setting(value.as_deref())?;
            return Ok(Some(serde_json::to_string(&config)?));
        }
        if key == "artifact_settings" {
            return value
                .map(|value| {
                    let settings = crate::Gateway::parse_artifact_settings(Some(&value))?;
                    Ok(serde_json::to_string(&settings)?)
                })
                .transpose();
        }
        Ok(value)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        reject_managed_or_reserved_setting(key)?;
        if key == crate::outbound_proxy::SETTINGS_KEY {
            let mut config =
                crate::outbound_proxy::OutboundProxySettings::from_setting(Some(value))?;
            config.normalize();
            config.validate()?;

            let previous =
                crate::outbound_proxy::OutboundProxySettings::load(self.gw.storage.settings())
                    .await?;
            let next_ids: std::collections::HashSet<&str> = config
                .proxies
                .iter()
                .map(|entry| entry.id.as_str())
                .collect();
            let removed: Vec<&crate::outbound_proxy::OutboundProxyEntry> = previous
                .proxies
                .iter()
                .filter(|entry| !next_ids.contains(entry.id.as_str()))
                .collect();
            if !removed.is_empty() {
                let providers = self.gw.storage.providers().list().await?;
                let web_providers = match self.gw.storage.web_providers() {
                    Some(store) => store.list().await?,
                    None => Vec::new(),
                };
                let update_mode =
                    crate::outbound_proxy::update_proxy_mode(self.gw.storage.settings()).await?;
                for entry in removed {
                    let mut consumers = Vec::new();
                    for provider in &providers {
                        if provider.proxy_mode == entry.id {
                            consumers.push(provider.name.clone());
                        }
                    }
                    for wp in &web_providers {
                        if wp.proxy_mode == entry.id {
                            consumers.push(wp.name.clone());
                        }
                    }
                    if update_mode.proxy_id() == Some(entry.id.as_str()) {
                        consumers.push("Product Updates".to_owned());
                    }
                    if !consumers.is_empty() {
                        anyhow::bail!(
                            "Cannot delete proxy \"{}\": currently used by {}",
                            entry.display_name(),
                            consumers.join(", ")
                        );
                    }
                }
            }

            return self
                .gw
                .storage
                .settings()
                .set(key, &serde_json::to_string(&config)?)
                .await;
        }
        if key == crate::outbound_proxy::UPDATE_PROXY_MODE_KEY {
            let selection = crate::outbound_proxy::ProxySelection::parse(value)?;
            if let Some(proxy_id) = selection.proxy_id() {
                let proxies =
                    crate::outbound_proxy::OutboundProxySettings::load(self.gw.storage.settings())
                        .await?;
                anyhow::ensure!(
                    proxies.find(proxy_id).is_some(),
                    "selected outbound proxy does not exist"
                );
            }
            return self
                .gw
                .storage
                .settings()
                .set(key, selection.as_str())
                .await;
        }
        if key == crate::media_generation::config::SETTINGS_KEY {
            self.update_media_generation_config(serde_json::from_str(value)?)
                .await?;
            return Ok(());
        }
        if key == crate::rpm::SETTINGS_KEY {
            let _save = crate::rpm::CONFIG_WRITE.lock().await;
            let config = crate::rpm::RpmConfig::from_setting(Some(value))?;
            self.gw
                .storage
                .settings()
                .set(key, &serde_json::to_string(&config)?)
                .await?;
            self.gw.rpm_admission.configure(config);
            crate::storage::bump_config_epoch(self.gw.storage.settings()).await?;
            return Ok(());
        }
        if key == "artifact_settings" {
            // Serialize the persisted configuration and the live backend cutover together.
            static SAVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
            let _save = SAVE.lock().await;
            let settings: crate::agent::artifact::ArtifactSettings = serde_json::from_str(value)?;
            settings.validate_for_save()?;
            let previous = self.gw.storage.settings().get(key).await?;
            let previous: crate::agent::artifact::ArtifactSettings = previous
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default();
            let store = self
                .gw
                .artifact_store()
                .ok_or_else(|| anyhow::anyhow!("artifact storage unavailable"))?;
            store.configure(&settings).await?;
            if let Err(error) = self
                .gw
                .storage
                .settings()
                .set(key, &serde_json::to_string(&settings)?)
                .await
            {
                store.configure(&previous).await?;
                return Err(error);
            }
            return Ok(());
        }
        if key == "reversible_redaction_enabled" && !matches!(value, "true" | "false") {
            anyhow::bail!("reversible_redaction_enabled must be true or false");
        }
        let retention_days =
            if key == "log_retention_days" {
                Some(value.parse::<u32>().map_err(|_| {
                    anyhow::anyhow!("log_retention_days must be a positive integer")
                })?)
            } else {
                None
            };
        if let Some(days) = retention_days {
            anyhow::ensure!(
                days >= crate::history_retention::MIN_RETENTION_DAYS,
                "log_retention_days must be at least {}",
                crate::history_retention::MIN_RETENTION_DAYS
            );
        }
        self.gw.storage.settings().set(key, value).await?;
        if let Some(days) = retention_days {
            self.gw.retention().set_days(days);
            self.gw.observation.apply_retention_change().await?;
            self.gw.sweep_history().await?;
        }
        Ok(())
    }

    pub fn describe_system_proxy(&self) -> crate::outbound_proxy::SystemProxySummary {
        crate::outbound_proxy::describe_system_proxy(self.gw.config.system_proxy_source)
    }

    pub async fn probe_outbound_proxy(
        &self,
        request: crate::outbound_proxy::ProxyIpProbeRequest,
    ) -> anyhow::Result<crate::outbound_proxy::ProxyIpProbeResult> {
        let resolved = if let Some(url) = request
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            crate::outbound_proxy::validate_proxy_url(url)?;
            crate::outbound_proxy::ResolvedOutboundProxy::Explicit {
                url: url.to_owned(),
                force_http1: false,
            }
        } else {
            let mode = request
                .proxy_mode
                .as_deref()
                .unwrap_or(crate::outbound_proxy::PROXY_MODE_SYSTEM);
            self.gw.vendor_proxy_snapshot(mode).await?
        };
        crate::outbound_proxy::probe_proxy_ai_ips(&resolved).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::GatewayConfig;
    use crate::db::models::{
        CreateProvider, ProviderCredentialInput, ProviderSourceInput, UpdateProvider,
    };
    use crate::gateway::Gateway;
    use crate::outbound_proxy::{
        OutboundProxyEntry, OutboundProxySettings, PROXY_MODE_NONE, PROXY_MODE_SYSTEM,
        SETTINGS_KEY, UPDATE_PROXY_MODE_KEY,
    };
    use crate::storage::MemoryStorage;

    #[tokio::test]
    async fn deleting_referenced_proxy_is_rejected_until_consumers_switch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let gateway = Gateway::from_storage(
            GatewayConfig {
                data_dir: dir.path().to_path_buf(),
                catalog_base_url: None,
                ..Default::default()
            },
            Arc::new(MemoryStorage::new(vec![], vec![], vec![])),
        )
        .await?;
        let admin = gateway.admin();

        let two_proxies = serde_json::to_string(&OutboundProxySettings {
            proxies: vec![
                OutboundProxyEntry {
                    id: "us-1".into(),
                    name: "US Residential".into(),
                    url: "http://127.0.0.1:8080".into(),
                },
                OutboundProxyEntry {
                    id: "jp-1".into(),
                    name: "JP Node".into(),
                    url: "socks5h://127.0.0.1:1080".into(),
                },
            ],
        })?;
        admin.set_setting(SETTINGS_KEY, &two_proxies).await?;

        let provider = admin
            .create_provider(CreateProvider {
                name: Some("OpenAI Primary".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some("openai-compatible".into()),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: Default::default(),
                proxy_mode: "us-1".into(),
            })
            .await?;

        admin.set_setting(UPDATE_PROXY_MODE_KEY, "us-1").await?;
        assert!(admin.set_setting("update_use_proxy", "true").await.is_err());
        assert!(
            admin
                .set_setting(UPDATE_PROXY_MODE_KEY, "nonexistent")
                .await
                .is_err()
        );

        let only_jp = serde_json::to_string(&OutboundProxySettings {
            proxies: vec![OutboundProxyEntry {
                id: "jp-1".into(),
                name: "JP Node".into(),
                url: "socks5h://127.0.0.1:1080".into(),
            }],
        })?;
        let err = admin
            .set_setting(SETTINGS_KEY, &only_jp)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("US Residential"), "error was: {err}");
        assert!(err.contains("OpenAI Primary"), "error was: {err}");
        assert!(err.contains("Product Updates"), "error was: {err}");

        admin
            .update_provider(
                &provider.id,
                UpdateProvider {
                    proxy_mode: Some(PROXY_MODE_SYSTEM.into()),
                    ..Default::default()
                },
            )
            .await?;
        admin
            .set_setting(UPDATE_PROXY_MODE_KEY, PROXY_MODE_NONE)
            .await?;
        admin.set_setting(SETTINGS_KEY, &only_jp).await?;

        gateway.shutdown().await;
        Ok(())
    }
}

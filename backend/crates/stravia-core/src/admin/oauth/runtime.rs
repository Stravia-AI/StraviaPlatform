use super::*;

struct OAuthRefreshLease {
    gateway: Gateway,
    provider_id: String,
    expected_version: i32,
    armed: bool,
}

impl OAuthRefreshLease {
    fn new(gateway: Gateway, provider_id: String, expected_version: i32) -> Self {
        Self {
            gateway,
            provider_id,
            expected_version,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OAuthRefreshLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let gateway = self.gateway.clone();
        let provider_id = self.provider_id.clone();
        let expected_version = self.expected_version;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = gateway
                    .storage
                    .oauth_credentials()
                    .cancel_refresh(&provider_id, expected_version)
                    .await
                {
                    tracing::warn!(
                        provider_id,
                        error = %error,
                        "Failed to release cancelled OAuth refresh lease"
                    );
                }
            });
        }
    }
}

pub(super) fn same_oauth_connection_generation(
    snapshot: &OAuthCredential,
    current: Option<&OAuthCredential>,
) -> bool {
    current.is_some_and(|current| current.connection_id == snapshot.connection_id)
}

impl AdminService {
    pub(crate) async fn resolve_provider_runtime(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<ResolvedProviderRuntime> {
        if provider.effective_auth_mode().trim() != "oauth" {
            if google_vertex::is_vertex_vendor(provider) {
                let credential = provider
                    .adapter_credential("credentials")
                    .unwrap_or_else(|| provider.effective_api_key());
                if credential.is_empty() {
                    anyhow::bail!("Vertex service account JSON or API key is empty");
                }
                let access_token = google_vertex::vertex_access_token(&credential).await?;
                let binding = RuntimeBinding {
                    base_url_override: Some(google_vertex::expand_vertex_base_url(
                        &provider.base_url,
                        &credential,
                    )),
                    ..RuntimeBinding::default()
                };
                return Ok(ResolvedProviderRuntime {
                    access_token,
                    binding,
                });
            }
            let api_key = provider.effective_api_key();
            if api_key.is_empty() {
                anyhow::bail!("provider api key is empty");
            }
            return Ok(ResolvedProviderRuntime {
                access_token: api_key,
                binding: RuntimeBinding::default(),
            });
        }

        let oauth_cred = self
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?;

        let oauth_cred = match oauth_cred {
            Some(c) => c,
            None => anyhow::bail!("provider oauth credential not found"),
        };

        let driver_key = if oauth_cred.driver_key.is_empty() {
            provider
                .vendor
                .as_deref()
                .map(auth::normalize_driver_key)
                .unwrap_or_default()
        } else {
            oauth_cred.driver_key.clone()
        };

        let credential = stored_credential_from_oauth(&oauth_cred, &driver_key);
        let access_token = credential
            .access_token
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();

        if !access_token.is_empty() && !is_expired_at(credential.expires_at.as_deref()) {
            let binding = if let Some(driver) = auth::build_driver(&driver_key) {
                driver.bind_runtime(provider, &credential)?
            } else {
                RuntimeBinding::default()
            };
            return Ok(ResolvedProviderRuntime {
                access_token,
                binding,
            });
        }

        let refresh_token = credential
            .refresh_token
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        if refresh_token.is_empty() {
            anyhow::bail!("provider oauth refresh token is missing");
        }

        let Some(driver) = auth::build_driver(&driver_key) else {
            anyhow::bail!("no auth driver found for key: {driver_key}");
        };

        // CAS lock: transition connected → refreshing to prevent concurrent refresh
        let oauth_store = self.gw.storage.oauth_credentials();
        let Some(locked) = oauth_store
            .try_begin_refresh(&provider.id, oauth_cred.status_version)
            .await?
        else {
            // Another caller is already refreshing — re-read and use whatever is there.
            let refreshed = oauth_store.get(&provider.id).await?.ok_or_else(|| {
                anyhow::anyhow!("provider oauth credential disappeared during refresh")
            })?;
            let refreshed_token = refreshed.access_token.trim().to_string();
            if !refreshed_token.is_empty() && !is_expired_at(refreshed.expires_at.as_deref()) {
                let cred = stored_credential_from_oauth(&refreshed, &driver_key);
                let binding = driver.bind_runtime(provider, &cred)?;
                return Ok(ResolvedProviderRuntime {
                    access_token: refreshed_token,
                    binding,
                });
            }
            anyhow::bail!("concurrent refresh in progress but no valid token available");
        };

        let mut refresh_lease =
            OAuthRefreshLease::new(self.gw.clone(), provider.id.clone(), locked.status_version);

        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let bundle = match driver
            .refresh(
                &credential,
                RefreshAuthContext {
                    use_proxy: provider.use_proxy,
                    http_client: Some(client),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(bundle) => bundle,
            Err(error) => {
                if let Err(store_error) = oauth_store
                    .fail_refresh(&provider.id, locked.status_version, &error.to_string())
                    .await
                {
                    return Err(error.context(format!(
                        "refresh oauth access token; failed to persist refresh failure: {store_error}"
                    )));
                }
                refresh_lease.disarm();
                return Err(error.context("refresh oauth access token"));
            }
        };

        let refreshed_credential =
            stored_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        let credential_input =
            upsert_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        self.gw
            .storage
            .oauth_credentials()
            .complete_refresh(&provider.id, locked.status_version, credential_input)
            .await?;
        refresh_lease.disarm();
        let refreshed_provider = self
            .sync_provider_runtime_fields(provider, &refreshed_credential)
            .await?;
        let new_access_token = bundle
            .access_token
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| {
                anyhow::anyhow!("provider credential refresh returned empty access token")
            })?;

        Ok(ResolvedProviderRuntime {
            access_token: new_access_token,
            binding: driver.bind_runtime(&refreshed_provider, &refreshed_credential)?,
        })
    }

    pub(in crate::admin) async fn sync_provider_runtime_fields(
        &self,
        provider: &Provider,
        credential: &StoredCredential,
    ) -> anyhow::Result<Provider> {
        let Some(driver) = auth::build_driver(&credential.driver_key) else {
            return Ok(provider.clone());
        };

        let binding = driver.bind_runtime(provider, credential)?;
        let base_url = binding
            .base_url_override
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| provider.base_url.clone());
        let models_source = binding
            .models_source_override
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| provider.models_source.clone());

        self.gw
            .storage
            .providers()
            .update(
                &provider.id,
                UpdateProvider {
                    base_url: Some(base_url),
                    models_source,
                    api_key: Some(String::new()),
                    auth_mode: Some("oauth".to_string()),
                    is_enabled: Some(provider.is_enabled),
                    ..Default::default()
                },
            )
            .await
    }
    pub(crate) async fn resolve_provider_runtime_from_snapshot(
        &self,
        provider: &Provider,
        oauth_snapshot: Option<&OAuthCredential>,
    ) -> anyhow::Result<ResolvedProviderRuntime> {
        if provider.effective_auth_mode().trim() != "oauth" || oauth_snapshot.is_none() {
            return self.resolve_provider_runtime(provider).await;
        }
        let oauth_cred = oauth_snapshot.expect("checked above");

        // A run may outlive an administrator deleting or reconnecting its Provider,
        // so an unexpired captured credential never depends on later storage state.
        let driver_key = if oauth_cred.driver_key.is_empty() {
            provider
                .vendor
                .as_deref()
                .map(auth::normalize_driver_key)
                .unwrap_or_default()
        } else {
            oauth_cred.driver_key.clone()
        };
        let credential = stored_credential_from_oauth(oauth_cred, &driver_key);
        let access_token = credential
            .access_token
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        let Some(driver) = auth::build_driver(&driver_key) else {
            anyhow::bail!("no auth driver found for key: {driver_key}");
        };
        if !access_token.is_empty() && !is_expired_at(credential.expires_at.as_deref()) {
            return Ok(ResolvedProviderRuntime {
                access_token,
                binding: driver.bind_runtime(provider, &credential)?,
            });
        }
        let current = self
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?;
        if same_oauth_connection_generation(oauth_cred, current.as_ref()) {
            return self.resolve_provider_runtime(provider).await;
        }

        anyhow::ensure!(
            credential
                .refresh_token
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            "provider oauth refresh token is missing"
        );
        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let bundle = driver
            .refresh(
                &credential,
                RefreshAuthContext {
                    use_proxy: provider.use_proxy,
                    http_client: Some(client),
                    ..Default::default()
                },
            )
            .await?;
        let refreshed_credential =
            stored_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        let access_token = bundle
            .access_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| {
                anyhow::anyhow!("provider credential refresh returned empty access token")
            })?;
        Ok(ResolvedProviderRuntime {
            access_token,
            binding: driver.bind_runtime(provider, &refreshed_credential)?,
        })
    }

    pub async fn refresh_oauth_providers(&self) -> anyhow::Result<usize> {
        let oauth_store = self.gw.storage.oauth_credentials();

        // Recover stale refreshing credentials (timeout = 60s)
        let recovered = oauth_store
            .recover_stale_refreshing(std::time::Duration::from_secs(60))
            .await
            .unwrap_or(0);
        if recovered > 0 {
            tracing::info!("recovered {recovered} stale refreshing oauth credentials");
        }

        // Find credentials expiring within 300 seconds
        let expiring = oauth_store
            .list_expiring(std::time::Duration::from_secs(300))
            .await?;

        let mut refreshed = 0usize;
        for cred in expiring {
            let has_refresh = cred
                .refresh_token
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());
            if !has_refresh {
                continue;
            }

            let provider = match self.gw.storage.providers().get(&cred.provider_id).await? {
                Some(p) => p,
                None => continue,
            };

            match self.proactive_refresh_credential(&provider, &cred).await {
                Ok(_) => refreshed += 1,
                Err(error) => tracing::warn!(
                    "background oauth refresh failed for provider {} ({}): {}",
                    provider.id,
                    provider.name,
                    error
                ),
            }
        }

        Ok(refreshed)
    }

    /// Proactively refresh an OAuth credential that is approaching expiry.
    /// Unlike `resolve_provider_runtime` (which skips refresh if the token is
    /// still valid), this always attempts to obtain a new token.
    async fn proactive_refresh_credential(
        &self,
        provider: &Provider,
        cred: &OAuthCredential,
    ) -> anyhow::Result<()> {
        let driver_key = if cred.driver_key.is_empty() {
            provider
                .vendor
                .as_deref()
                .map(auth::normalize_driver_key)
                .unwrap_or_default()
        } else {
            cred.driver_key.clone()
        };

        let credential = stored_credential_from_oauth(cred, &driver_key);

        let Some(driver) = auth::build_driver(&driver_key) else {
            anyhow::bail!("no auth driver found for key: {driver_key}");
        };

        // CAS lock: transition connected → refreshing
        let oauth_store = self.gw.storage.oauth_credentials();
        let Some(locked) = oauth_store
            .try_begin_refresh(&provider.id, cred.status_version)
            .await?
        else {
            // Another caller is already refreshing — skip.
            return Ok(());
        };
        let mut refresh_lease =
            OAuthRefreshLease::new(self.gw.clone(), provider.id.clone(), locked.status_version);

        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let bundle = match driver
            .refresh(
                &credential,
                RefreshAuthContext {
                    use_proxy: provider.use_proxy,
                    http_client: Some(client),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(bundle) => bundle,
            Err(error) => {
                oauth_store
                    .fail_refresh(&provider.id, locked.status_version, &error.to_string())
                    .await?;
                refresh_lease.disarm();
                return Err(error.context("proactive oauth refresh"));
            }
        };

        let refreshed_credential =
            stored_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        let credential_input =
            upsert_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        oauth_store
            .complete_refresh(&provider.id, locked.status_version, credential_input)
            .await?;
        refresh_lease.disarm();
        self.sync_provider_runtime_fields(provider, &refreshed_credential)
            .await?;
        Ok(())
    }
    pub(in crate::admin) async fn reconnect_provider_oauth_record(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderOAuthStatusData> {
        let provider = self.get_provider(id).await?;
        let driver_key = provider
            .vendor
            .as_deref()
            .map(auth::normalize_driver_key)
            .unwrap_or_default();

        if driver_key.is_empty() {
            anyhow::bail!("provider vendor is empty");
        }
        let driver = auth::build_driver(&driver_key)
            .ok_or_else(|| anyhow::anyhow!("auth vendor not implemented: {driver_key}"))?;
        if !driver.metadata().supports_existing_provider {
            anyhow::bail!("auth vendor does not support reconnect: {driver_key}");
        }

        let oauth_cred = self
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider oauth credential not found"))?;

        let credential = stored_credential_from_oauth(&oauth_cred, &driver_key);
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        if refresh_token.is_empty() {
            anyhow::bail!("provider oauth refresh token is missing");
        }

        let oauth_store = self.gw.storage.oauth_credentials();
        let Some(locked) = oauth_store
            .try_begin_refresh(&provider.id, oauth_cred.status_version)
            .await?
        else {
            anyhow::bail!("provider OAuth credential refresh is already in progress");
        };
        let mut refresh_lease =
            OAuthRefreshLease::new(self.gw.clone(), provider.id.clone(), locked.status_version);

        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let bundle = match driver
            .refresh(
                &credential,
                RefreshAuthContext {
                    use_proxy: provider.use_proxy,
                    http_client: Some(client),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(bundle) => bundle,
            Err(error) => {
                if oauth_store
                    .fail_refresh(&provider.id, locked.status_version, &error.to_string())
                    .await
                    .unwrap_or(false)
                {
                    refresh_lease.disarm();
                }
                return Ok(build_provider_oauth_status(
                    &provider,
                    &driver_key,
                    Some(AuthBindingStatus::Error.as_str().to_string()),
                    Some(error.to_string()),
                ));
            }
        };

        let refreshed_credential =
            stored_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        let credential_input =
            upsert_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        oauth_store
            .complete_refresh(&provider.id, locked.status_version, credential_input)
            .await?;
        refresh_lease.disarm();
        let refreshed_provider = self
            .sync_provider_runtime_fields(&provider, &refreshed_credential)
            .await?;

        Ok(build_provider_oauth_status(
            &refreshed_provider,
            &driver_key,
            Some(AuthBindingStatus::Connected.as_str().to_string()),
            None,
        ))
    }
}

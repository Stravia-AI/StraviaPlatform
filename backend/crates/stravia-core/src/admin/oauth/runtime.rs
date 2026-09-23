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

impl AdminService {
    pub(in crate::admin) async fn sync_provider_runtime_fields(
        &self,
        provider: &Provider,
        _credential: &StoredCredential,
    ) -> anyhow::Result<Provider> {
        let vendor_id = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let (_, operation, _) = self.gw.vendor_plugins.acquire(vendor_id)?;
        let write_fence = operation.write_fence().await?;
        let current = self
            .gw
            .storage
            .providers()
            .get(&provider.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider was removed"))?;
        anyhow::ensure!(
            serde_json::to_vec(&current)? == serde_json::to_vec(provider)?,
            "provider changed while binding OAuth credentials"
        );
        let updated = self
            .gw
            .storage
            .providers()
            .update(
                &provider.id,
                UpdateProvider {
                    api_key: Some(String::new()),
                    auth_mode: Some("oauth".to_string()),
                    is_enabled: Some(provider.is_enabled),
                    ..Default::default()
                },
            )
            .await?;
        drop(write_fence);
        drop(operation);
        Ok(updated)
    }

    pub(crate) async fn force_refresh_provider_oauth(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<OAuthCredential> {
        self.force_refresh_provider_oauth_with_context(
            provider_id,
            None,
            stravia_runtime_contract::CancellationToken::new(),
            stravia_runtime_contract::Deadline::fixed(
                std::time::Instant::now() + std::time::Duration::from_secs(120),
            ),
        )
        .await
    }

    pub(crate) async fn recover_provider_auth_with_lease(
        &self,
        provider_id: &str,
        pinned: &crate::plugin::execution::PreparedVendorExecution,
        cancellation: stravia_runtime_contract::CancellationToken,
        deadline: stravia_runtime_contract::Deadline,
    ) -> anyhow::Result<()> {
        if pinned.oauth_connection_id().is_some() {
            self.force_refresh_provider_oauth_with_context(
                provider_id,
                Some(pinned),
                cancellation,
                deadline,
            )
            .await?;
            return Ok(());
        }

        let context = crate::plugin::VendorCallContext::new(cancellation, deadline);
        let prepared = self
            .gw
            .prepare_vendor_execution_with_lease(
                pinned,
                provider_id,
                None,
                stravia_vendor_sdk::Operation::Auth,
                &context,
            )
            .await?;
        let execution = self
            .gw
            .execute_prepared_vendor(
                prepared,
                crate::plugin::VendorRequest::Auth(stravia_vendor_sdk::AuthRequest {
                    step: stravia_vendor_sdk::AuthStep::Refresh,
                }),
                context,
            )
            .await?;
        let publication = execution.publication.write_fence().await?;
        anyhow::ensure!(
            matches!(
                execution.output,
                stravia_vendor_sdk::OperationOutput::Auth(
                    stravia_vendor_sdk::AuthResponse::Credentials { .. }
                )
            ),
            "vendor returned an invalid authentication refresh result"
        );
        drop(publication);
        Ok(())
    }

    async fn force_refresh_provider_oauth_with_context(
        &self,
        provider_id: &str,
        pinned: Option<&crate::plugin::execution::PreparedVendorExecution>,
        cancellation: stravia_runtime_contract::CancellationToken,
        deadline: stravia_runtime_contract::Deadline,
    ) -> anyhow::Result<OAuthCredential> {
        let provider = self.get_provider(provider_id).await?;
        let vendor_id = provider
            .vendor
            .as_deref()
            .map(str::trim)
            .filter(|vendor| !vendor.is_empty())
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?
            .to_owned();
        let oauth_store = self.gw.storage.oauth_credentials();
        let credential = oauth_store
            .get(provider_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider OAuth credential not found"))?;
        anyhow::ensure!(
            credential
                .refresh_token
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            "provider OAuth refresh token is missing"
        );
        let Some(locked) = oauth_store
            .try_begin_refresh(provider_id, credential.status_version)
            .await?
        else {
            let current = oauth_store
                .get(provider_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("provider OAuth credential disappeared"))?;
            anyhow::ensure!(
                !current.access_token.trim().is_empty()
                    && !is_expired_at(current.expires_at.as_deref()),
                "provider OAuth refresh is already in progress"
            );
            return Ok(current);
        };
        let mut refresh_lease = OAuthRefreshLease::new(
            self.gw.clone(),
            provider_id.to_owned(),
            locked.status_version,
        );
        let provider_fingerprint = serde_json::to_vec(&provider)?;
        let context = crate::plugin::VendorCallContext::new(cancellation, deadline);
        let execution = match pinned {
            Some(pinned) => match self
                .gw
                .prepare_vendor_execution_with_lease(
                    pinned,
                    provider_id,
                    None,
                    stravia_vendor_sdk::Operation::Auth,
                    &context,
                )
                .await
            {
                Ok(prepared) => {
                    self.gw
                        .execute_prepared_vendor(
                            prepared,
                            crate::plugin::VendorRequest::Auth(stravia_vendor_sdk::AuthRequest {
                                step: stravia_vendor_sdk::AuthStep::Refresh,
                            }),
                            context,
                        )
                        .await
                }
                Err(error) => Err(error),
            },
            None => {
                self.gw
                    .execute_vendor(
                        provider_id,
                        None,
                        crate::plugin::VendorRequest::Auth(stravia_vendor_sdk::AuthRequest {
                            step: stravia_vendor_sdk::AuthStep::Refresh,
                        }),
                        context,
                    )
                    .await
            }
        };
        let execution = match execution {
            Ok(execution) => execution,
            Err(error) => {
                let (fence, operation) = match pinned {
                    Some(pinned) => (pinned.write_fence().await?, None),
                    None => {
                        let (_, operation, _) = self.gw.vendor_plugins.acquire(&vendor_id)?;
                        let fence = operation.write_fence().await?;
                        (fence, Some(operation))
                    }
                };
                oauth_store
                    .fail_refresh(provider_id, locked.status_version, &error.to_string())
                    .await?;
                refresh_lease.disarm();
                drop(fence);
                drop(operation);
                return Err(error.context("refresh provider OAuth credential"));
            }
        };
        let publication = execution.publication.write_fence().await?;
        let stravia_vendor_sdk::OperationOutput::Auth(
            response @ stravia_vendor_sdk::AuthResponse::Credentials { .. },
        ) = execution.output
        else {
            anyhow::bail!("vendor returned an invalid OAuth refresh result")
        };
        let bundle = super::credential_bundle_from_response(response)?;
        let current_provider = self
            .gw
            .storage
            .providers()
            .get(provider_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider was removed during OAuth refresh"))?;
        anyhow::ensure!(
            serde_json::to_vec(&current_provider)? == provider_fingerprint,
            "provider changed during OAuth refresh"
        );
        let current_credential = oauth_store.get(provider_id).await?.ok_or_else(|| {
            anyhow::anyhow!("provider OAuth credential was removed during refresh")
        })?;
        anyhow::ensure!(
            current_credential.connection_id == credential.connection_id
                && current_credential.status_version == locked.status_version,
            "provider OAuth connection changed during refresh"
        );
        oauth_store
            .complete_refresh(
                provider_id,
                locked.status_version,
                upsert_credential_from_bundle(&vendor_id, &credential.scheme, &bundle),
            )
            .await?;
        self.gw
            .vendor_plugins
            .store
            .recovered(provider_id, "credentials")
            .await?;
        refresh_lease.disarm();
        let refreshed = oauth_store
            .get(provider_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("refreshed OAuth credential was not persisted"))?;
        drop(publication);
        Ok(refreshed)
    }

    pub async fn refresh_oauth_providers(&self) -> anyhow::Result<usize> {
        let oauth_store = self.gw.storage.oauth_credentials();
        let recovered = oauth_store
            .recover_stale_refreshing(std::time::Duration::from_secs(60))
            .await?;
        if recovered > 0 {
            tracing::info!("recovered {recovered} stale refreshing OAuth credentials");
        }
        let expiring = oauth_store
            .list_expiring(std::time::Duration::from_secs(300))
            .await?;
        let mut refreshed = 0;
        for credential in expiring {
            if credential
                .refresh_token
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                continue;
            }
            match self
                .force_refresh_provider_oauth(&credential.provider_id)
                .await
            {
                Ok(_) => refreshed += 1,
                Err(error) => tracing::warn!(
                    provider_id = credential.provider_id,
                    %error,
                    "background OAuth refresh failed"
                ),
            }
        }
        Ok(refreshed)
    }

    pub(in crate::admin) async fn reconnect_provider_oauth_record(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderOAuthStatusData> {
        let provider = self.get_provider(id).await?;
        let credential = self.force_refresh_provider_oauth(id).await?;
        Ok(build_provider_oauth_status_from_credential(
            &provider,
            provider.vendor.as_deref().unwrap_or_default(),
            &credential,
        ))
    }
}

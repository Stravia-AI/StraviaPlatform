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
            crate::admin::provider_connection::same_provider_generation(&current, provider),
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
            false,
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
                true,
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
        let credential_version = prepared.credential_version();
        let execution = self
            .gw
            .execute_prepared_vendor(
                prepared,
                crate::plugin::VendorRequest::Auth(stravia_vendor_sdk::AuthRequest {
                    step: stravia_vendor_sdk::AuthStep::Refresh,
                }),
                context,
            )
            .await;
        let execution = match execution {
            Ok(execution) => execution,
            Err(error) => {
                // ADR-0073：此路径只在已观测到上游 401 后进入——恢复未产出
                // 新凭据即恢复耗尽，标记失效；取消/超时不是凭据证据。
                if !crate::plugin::execution::is_execution_interruption(&error) {
                    self.gw
                        .mark_provider_credential_invalid(provider_id, credential_version)
                        .await;
                }
                return Err(error);
            }
        };
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
        // ADR-0073：vendor 侧刷新产出了被上游接受的新凭据——清除失效。
        self.gw.clear_provider_credential_invalid(provider_id).await;
        drop(publication);
        Ok(())
    }

    /// `mark_exhausted` 仅由已观测到上游 401 的恢复路径传入：此时任何非
    /// 中断的刷新失败都等于恢复耗尽，应标记失效。主动刷新（后台任务、手动
    /// 重连）只在刷新本身被上游凭据拒绝时标记。
    async fn force_refresh_provider_oauth_with_context(
        &self,
        provider_id: &str,
        pinned: Option<&crate::plugin::execution::PreparedVendorExecution>,
        cancellation: stravia_runtime_contract::CancellationToken,
        deadline: stravia_runtime_contract::Deadline,
        mark_exhausted: bool,
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
        if credential
            .refresh_token
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            // ADR-0073：无 refresh token 意味着被拒的 access token 无法恢复，
            // 恢复路径到此即耗尽。
            if mark_exhausted {
                self.gw
                    .mark_provider_credential_invalid(
                        provider_id,
                        crate::db::models::ProviderCredentialVersion {
                            provider_revision: provider.revision,
                            oauth_status_version: Some(credential.status_version),
                        },
                    )
                    .await;
            }
            anyhow::bail!("provider OAuth refresh token is missing");
        }
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
        let provider_fingerprint = provider.clone();
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
                // ADR-0073：标记必须在 fail_refresh 之前——后者 bump
                // status_version，会让条件写把本证据判为过期。刷新请求本身被
                // 上游凭据拒绝总是证据；恢复耗尽上下文里任何真实失败都是。
                if crate::plugin::execution::is_credential_rejection(&error)
                    || (mark_exhausted
                        && !crate::plugin::execution::is_execution_interruption(&error))
                {
                    self.gw
                        .mark_provider_credential_invalid(
                            provider_id,
                            crate::db::models::ProviderCredentialVersion {
                                provider_revision: provider.revision,
                                oauth_status_version: Some(locked.status_version),
                            },
                        )
                        .await;
                }
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
            crate::admin::provider_connection::same_provider_generation(
                &current_provider,
                &provider_fingerprint
            ),
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
        // ADR-0073：刷新成功=上游已接受新凭据，清除失效标记。
        self.gw.clear_provider_credential_invalid(provider_id).await;
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

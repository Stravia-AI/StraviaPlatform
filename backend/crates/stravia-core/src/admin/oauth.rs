use super::*;

mod runtime;
mod session_store;
#[cfg(test)]
use runtime::same_oauth_connection_generation;

impl AdminService {
    pub async fn init_oauth_session(
        &self,
        vendor: &str,
        use_proxy: bool,
        options: OAuthSessionStartOptions,
    ) -> anyhow::Result<AuthSessionInitData> {
        match super::provider_connection::ProviderConnection::new(self)
            .reconnect(super::provider_connection::ProviderReconnect::Start(
                super::provider_connection::ProviderReconnectStart::Authorization {
                    vendor: vendor.to_string(),
                    use_proxy,
                    options,
                },
            ))
            .await?
        {
            super::provider_connection::ProviderReconnectResult::Redirect(started) => Ok(started),
            super::provider_connection::ProviderReconnectResult::Complete(_) => {
                unreachable!("OAuth Start cannot complete a callback")
            }
            _ => anyhow::bail!("OAuth authorization start returned an unexpected result"),
        }
    }

    pub(super) async fn init_oauth_session_record(
        &self,
        vendor: &str,
        use_proxy: bool,
        options: OAuthSessionStartOptions,
    ) -> anyhow::Result<AuthSessionInitData> {
        let driver_key = auth::normalize_driver_key(vendor);
        if driver_key.is_empty() {
            anyhow::bail!("auth vendor cannot be empty");
        }
        let driver = auth::build_driver(&driver_key)
            .ok_or_else(|| anyhow::anyhow!("auth vendor not implemented: {driver_key}"))?;
        let client = self.gw.http_client_for_provider(use_proxy).await?;
        let created = driver
            .start(StartAuthContext {
                use_proxy,
                redirect_uri: Some(options.redirect_uri.clone()),
                http_client: Some(client),
                ..Default::default()
            })
            .await?;
        let session = self.create_auth_session_record(created, options).await?;
        build_auth_session_init_data(&session)
    }

    pub async fn get_oauth_session_status(
        &self,
        session_id: &str,
    ) -> anyhow::Result<AuthSessionStatusData> {
        let session = self
            .get_auth_session_record(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {session_id}"))?;

        if is_expired_at(session.expires_at.as_deref()) {
            self.delete_auth_session_record(&session.id).await?;
            return Ok(AuthSessionStatusData::Error {
                code: "AUTH_TIMEOUT".to_string(),
                message: "auth session expired".to_string(),
            });
        }

        match session.status.as_str() {
            "exchanging" => return Ok(build_auth_session_exchanging_data(&session)),
            "ready" => {
                let bundle = parse_auth_session_bundle(&session)?;
                return Ok(build_auth_session_ready_data(&session, &bundle));
            }
            "error" => {
                let message = session
                    .last_error
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "auth session failed".to_string());
                return Ok(AuthSessionStatusData::Error {
                    code: session
                        .error_code
                        .clone()
                        .unwrap_or_else(|| "AUTH_SESSION_ERROR".to_string()),
                    message,
                });
            }
            "cancelled" => {
                return Ok(AuthSessionStatusData::Error {
                    code: "AUTH_SESSION_CANCELLED".to_string(),
                    message: "auth session cancelled".to_string(),
                });
            }
            _ => {}
        }

        if session.scheme == AuthScheme::OAuthAuthCodePkce.as_str()
            || session.scheme == AuthScheme::SetupToken.as_str()
        {
            return Ok(build_auth_session_pending_data(&session));
        }

        let driver = auth::build_driver(&session.driver_key).ok_or_else(|| {
            anyhow::anyhow!("auth vendor not implemented: {}", session.driver_key)
        })?;
        let client = self.gw.http_client_for_provider(session.use_proxy).await?;

        match driver
            .poll(
                &session,
                RefreshAuthContext {
                    use_proxy: session.use_proxy,
                    http_client: Some(client),
                    ..Default::default()
                },
            )
            .await?
        {
            AuthPollState::Pending(progress) => {
                let updated = self
                    .update_auth_session_record(
                        &session.id,
                        UpdateAuthSession {
                            user_code: progress.user_code,
                            verification_uri: progress.verification_uri,
                            verification_uri_complete: progress.verification_uri_complete,
                            expires_at: progress.expires_at,
                            poll_interval_seconds: progress.poll_interval_seconds,
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(build_auth_session_pending_data(&updated))
            }
            AuthPollState::Ready(bundle) => {
                let updated = self
                    .update_auth_session_record(
                        &session.id,
                        UpdateAuthSession {
                            status: Some(AuthSessionStatus::Ready.as_str().to_string()),
                            result_json: Some(serde_json::to_string(&bundle)?),
                            expires_at: bundle.expires_at.clone(),
                            last_error: Some(String::new()),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(build_auth_session_ready_data(&updated, &bundle))
            }
            AuthPollState::Error { code, message } => {
                self.delete_auth_session_record(&session.id).await?;
                Ok(AuthSessionStatusData::Error { code, message })
            }
        }
    }

    pub async fn cancel_oauth_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.delete_auth_session_record(session_id).await
    }

    pub async fn mark_oauth_session_error(
        &self,
        session_id: &str,
        code: &str,
        message: &str,
    ) -> anyhow::Result<()> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let Some(session) = sessions.get_mut(session_id) else {
            return Ok(());
        };
        if !matches!(session.status.as_str(), "pending" | "exchanging") {
            return Ok(());
        }
        session.status = AuthSessionStatus::Error.as_str().to_string();
        session.error_code = Some(code.to_string());
        session.last_error = Some(message.to_string());
        session.listener_state = "stopped".to_string();
        session.updated_at = now_rfc3339();
        Ok(())
    }

    pub async fn update_oauth_session_proxy(
        &self,
        session_id: &str,
        use_proxy: bool,
    ) -> anyhow::Result<AuthSessionStatusData> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {session_id}"))?;
        if !matches!(session.status.as_str(), "pending" | "exchanging") {
            return Err(coded_error(
                "AUTH_SESSION_NOT_ACTIVE",
                "only a pending or exchanging OAuth session can change proxy settings",
                serde_json::json!({}),
            ));
        }
        if is_expired_at(session.expires_at.as_deref()) {
            sessions.remove(session_id);
            return Err(coded_error(
                "AUTH_SESSION_EXPIRED",
                "auth session expired",
                serde_json::json!({}),
            ));
        }
        session.use_proxy = use_proxy;
        session.updated_at = now_rfc3339();
        Ok(match session.status.as_str() {
            "exchanging" => build_auth_session_exchanging_data(session),
            _ => build_auth_session_pending_data(session),
        })
    }

    pub async fn complete_oauth_session(
        &self,
        session_id: &str,
        input: auth::AuthExchangeInput,
    ) -> anyhow::Result<AuthSessionStatusData> {
        match super::provider_connection::ProviderConnection::new(self)
            .reconnect(super::provider_connection::ProviderReconnect::Callback(
                super::provider_connection::ProviderReconnectCallback::Complete {
                    authorization_id: session_id.to_string(),
                    input,
                },
            ))
            .await?
        {
            super::provider_connection::ProviderReconnectResult::Complete(completed) => {
                Ok(completed)
            }
            super::provider_connection::ProviderReconnectResult::Redirect(_) => {
                unreachable!("OAuth Callback cannot start an authorization")
            }
            _ => anyhow::bail!("OAuth authorization callback returned an unexpected result"),
        }
    }

    pub(super) async fn complete_oauth_session_record(
        &self,
        session_id: &str,
        input: auth::AuthExchangeInput,
    ) -> anyhow::Result<AuthSessionStatusData> {
        let session = self.claim_pending_auth_session(session_id).await?;
        if session
            .status
            .eq_ignore_ascii_case(AuthSessionStatus::Ready.as_str())
        {
            let bundle = parse_auth_session_bundle(&session)?;
            return Ok(build_auth_session_ready_data(&session, &bundle));
        }

        let exchange_result = async {
            let driver = auth::build_driver(&session.driver_key).ok_or_else(|| {
                anyhow::anyhow!("auth vendor not implemented: {}", session.driver_key)
            })?;
            let client = self.gw.http_client_for_provider(session.use_proxy).await?;
            driver
                .exchange(
                    &session,
                    input,
                    ExchangeAuthContext {
                        use_proxy: session.use_proxy,
                        http_client: Some(client),
                        ..Default::default()
                    },
                )
                .await
        }
        .await;

        match exchange_result {
            Ok(bundle) => {
                let updated = self
                    .finish_claimed_auth_session(&session.id, &bundle)
                    .await?;
                Ok(build_auth_session_ready_data(&updated, &bundle))
            }
            Err(error) => {
                let exchange_error = error.downcast_ref::<auth::OAuthExchangeError>();
                let terminal = exchange_error.is_some_and(auth::OAuthExchangeError::is_terminal);
                let code = exchange_error
                    .map(auth::OAuthExchangeError::code)
                    .unwrap_or("AUTH_EXCHANGE_RETRYABLE");
                let message = error.to_string();
                self.fail_claimed_auth_session(&session.id, terminal, code, &message)
                    .await?;
                Err(coded_error(code, &message, serde_json::json!({})))
            }
        }
    }

    pub async fn create_provider_with_oauth_session(
        &self,
        session_id: &str,
        input: CreateProvider,
    ) -> anyhow::Result<Provider> {
        super::provider_connection::ProviderConnection::new(self)
            .save(super::provider_connection::ProviderSave::Catalog {
                input,
                authorization_id: Some(session_id.to_string()),
            })
            .await
    }

    pub(super) async fn create_provider_with_oauth_session_record(
        &self,
        session_id: &str,
        input: CreateProvider,
    ) -> anyhow::Result<Provider> {
        let session = self.take_ready_auth_session_record(session_id).await?;
        if is_expired_at(session.expires_at.as_deref()) {
            anyhow::bail!("auth session expired");
        }

        let bundle = parse_auth_session_bundle(&session)?;
        bundle
            .access_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("auth session missing access token"))?;

        let provider = match self.create_provider_from_input(input, true).await {
            Ok(provider) => provider,
            Err(error) => {
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };

        let credential_input =
            upsert_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let provisioned = async {
            self.gw
                .storage
                .oauth_credentials()
                .upsert(&provider.id, credential_input)
                .await?;
            let credential =
                stored_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
            self.sync_provider_runtime_fields(&provider, &credential)
                .await
        }
        .await;

        let provider = match provisioned {
            Ok(provider) => provider,
            Err(error) => {
                if let Err(cleanup_error) = self.delete_provider(&provider.id).await {
                    tracing::warn!(
                        "failed to rollback oauth provider {} after provisioning error: {}",
                        provider.id,
                        cleanup_error
                    );
                }
                self.restore_auth_session_record(session).await?;
                return Err(error.context("create oauth provider"));
            }
        };

        Ok(provider)
    }

    pub async fn get_provider_oauth_status(
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
            return Ok(build_provider_oauth_status(&provider, "", None, None));
        }

        let oauth_cred = self.gw.storage.oauth_credentials().get(id).await?;
        match oauth_cred {
            Some(cred) => Ok(build_provider_oauth_status_from_credential(
                &provider,
                &driver_key,
                &cred,
            )),
            None => Ok(build_provider_oauth_status(
                &provider,
                &driver_key,
                None,
                None,
            )),
        }
    }

    pub async fn reconnect_provider_oauth(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderOAuthStatusData> {
        match super::provider_connection::ProviderConnection::new(self)
            .reconnect(super::provider_connection::ProviderReconnect::Start(
                super::provider_connection::ProviderReconnectStart::Existing {
                    provider_id: id.to_string(),
                },
            ))
            .await?
        {
            super::provider_connection::ProviderReconnectResult::Status(status) => Ok(status),
            _ => anyhow::bail!("Provider reconnect returned an unexpected result"),
        }
    }

    pub async fn logout_provider_oauth(&self, id: &str) -> anyhow::Result<ProviderOAuthStatusData> {
        let provider = self.get_provider(id).await?;
        let driver_key = provider
            .vendor
            .as_deref()
            .map(auth::normalize_driver_key)
            .unwrap_or_default();

        if driver_key.is_empty() {
            return Ok(build_provider_oauth_status(&provider, "", None, None));
        }

        self.gw
            .storage
            .oauth_credentials()
            .delete(&provider.id)
            .await?;

        let updated = self
            .gw
            .storage
            .providers()
            .update(
                &provider.id,
                UpdateProvider {
                    auth_mode: Some("oauth".to_string()),
                    api_key: Some(String::new()),
                    ..Default::default()
                },
            )
            .await?;

        Ok(build_provider_oauth_status(
            &updated,
            &driver_key,
            Some(AuthBindingStatus::Disconnected.as_str().to_string()),
            None,
        ))
    }

    pub async fn bind_provider_with_oauth_session(
        &self,
        provider_id: &str,
        session_id: &str,
    ) -> anyhow::Result<Provider> {
        match super::provider_connection::ProviderConnection::new(self)
            .reconnect(super::provider_connection::ProviderReconnect::Callback(
                super::provider_connection::ProviderReconnectCallback::Bind {
                    provider_id: provider_id.to_string(),
                    authorization_id: session_id.to_string(),
                },
            ))
            .await?
        {
            super::provider_connection::ProviderReconnectResult::Provider(provider) => Ok(provider),
            _ => anyhow::bail!("Provider reconnect returned an unexpected result"),
        }
    }

    pub(super) async fn bind_provider_with_oauth_session_record(
        &self,
        provider_id: &str,
        session_id: &str,
    ) -> anyhow::Result<Provider> {
        let provider = self.get_provider(provider_id).await?;
        let session = self.take_ready_auth_session_record(session_id).await?;
        if is_expired_at(session.expires_at.as_deref()) {
            anyhow::bail!("auth session expired");
        }

        let bundle = parse_auth_session_bundle(&session)?;
        bundle
            .access_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("auth session missing access token"))?;

        let credential =
            stored_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let credential_input =
            upsert_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        match self
            .gw
            .storage
            .oauth_credentials()
            .upsert(&provider.id, credential_input)
            .await
        {
            Ok(_) => {}
            Err(error) => {
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        }
        let provider = match self
            .sync_provider_runtime_fields(&provider, &credential)
            .await
        {
            Ok(provider) => provider,
            Err(error) => {
                let _ = self
                    .gw
                    .storage
                    .oauth_credentials()
                    .delete(&provider.id)
                    .await;
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };

        Ok(provider)
    }
}

#[cfg(test)]
mod tests;

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

fn same_oauth_connection_generation(
    snapshot: &OAuthCredential,
    current: Option<&OAuthCredential>,
) -> bool {
    current.is_some_and(|current| current.connection_id == snapshot.connection_id)
}

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

    async fn create_auth_session_record(
        &self,
        input: auth::CreateAuthSession,
        options: OAuthSessionStartOptions,
    ) -> anyhow::Result<AuthSession> {
        // OAuth sessions are process-local. Callback requests must reach this
        // Gateway instance until a shared session store is introduced.
        if !self.gw.config.config_poll_interval.is_zero() {
            tracing::debug!(
                "creating oauth session in multi-replica mode \
                 — ensure the callback reaches this replica (session affinity required)"
            );
        }
        let now = now_rfc3339();
        let listener_state = if input.scheme == AuthScheme::OAuthDeviceCode.as_str() {
            "not_required".to_string()
        } else if options.callback_mode == OAuthCallbackMode::Auto {
            "listening".to_string()
        } else {
            "not_started".to_string()
        };
        let session = AuthSession {
            callback_mode: options.callback_mode,
            listener_state,
            listener_port: options.listener_port,
            redirect_uri: options.redirect_uri,
            fallback_reason: options.fallback_reason,
            id: uuid::Uuid::new_v4().to_string(),
            provider_id: input.provider_id,
            driver_key: input.driver_key,
            scheme: input.scheme,
            status: input.status,
            use_proxy: input.use_proxy,
            user_code: input.user_code,
            verification_uri: input.verification_uri,
            verification_uri_complete: input.verification_uri_complete,
            state_json: input.state_json,
            context_json: input.context_json,
            result_json: input.result_json,
            expires_at: input.expires_at,
            poll_interval_seconds: input.poll_interval_seconds,
            last_error: input.last_error,
            error_code: None,
            created_at: now.clone(),
            updated_at: now,
        };
        self.gw
            .auth_sessions
            .write()
            .await
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub(super) async fn get_auth_session_record(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<AuthSession>> {
        Ok(self.gw.auth_sessions.read().await.get(id).cloned())
    }

    pub(super) async fn claim_pending_auth_session(&self, id: &str) -> anyhow::Result<AuthSession> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let expired = sessions
            .get(id)
            .is_some_and(|session| is_expired_at(session.expires_at.as_deref()));
        if expired {
            sessions.remove(id);
            return Err(coded_error(
                "AUTH_SESSION_EXPIRED",
                "auth session expired",
                serde_json::json!({}),
            ));
        }

        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {id}"))?;
        match session.status.as_str() {
            "pending" => {
                session.status = AuthSessionStatus::Exchanging.as_str().to_string();
                session.updated_at = now_rfc3339();
                Ok(session.clone())
            }
            "exchanging" => Err(coded_error(
                "AUTH_COMPLETION_IN_PROGRESS",
                "OAuth completion is already in progress",
                serde_json::json!({}),
            )),
            "ready" => Ok(session.clone()),
            "error" | "cancelled" => Err(coded_error(
                "AUTH_SESSION_TERMINAL",
                session
                    .last_error
                    .as_deref()
                    .filter(|message| !message.trim().is_empty())
                    .unwrap_or("auth session cannot be completed"),
                serde_json::json!({}),
            )),

            _ => Err(coded_error(
                "AUTH_SESSION_INVALID_STATE",
                "auth session has an invalid state",
                serde_json::json!({}),
            )),
        }
    }
    async fn finish_claimed_auth_session(
        &self,
        id: &str,
        bundle: &CredentialBundle,
    ) -> anyhow::Result<AuthSession> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {id}"))?;
        if session.status != AuthSessionStatus::Exchanging.as_str() {
            return Err(coded_error(
                "AUTH_SESSION_REPLACED",
                "OAuth session changed while completion was in progress",
                serde_json::json!({}),
            ));
        }
        session.status = AuthSessionStatus::Ready.as_str().to_string();
        session.result_json = Some(serde_json::to_string(bundle)?);
        session.expires_at = bundle.expires_at.clone();
        session.last_error = None;
        session.error_code = None;
        session.listener_state = "stopped".to_string();
        session.updated_at = now_rfc3339();
        Ok(session.clone())
    }

    async fn fail_claimed_auth_session(
        &self,
        id: &str,
        terminal: bool,
        code: &str,
        message: &str,
    ) -> anyhow::Result<()> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {id}"))?;
        if session.status != AuthSessionStatus::Exchanging.as_str() {
            return Err(coded_error(
                "AUTH_SESSION_REPLACED",
                "OAuth session changed while completion was in progress",
                serde_json::json!({}),
            ));
        }
        session.status = if terminal {
            AuthSessionStatus::Error.as_str().to_string()
        } else {
            AuthSessionStatus::Pending.as_str().to_string()
        };
        session.error_code = Some(code.to_string());
        session.last_error = Some(message.to_string());
        if terminal {
            session.listener_state = "stopped".to_string();
        }
        session.updated_at = now_rfc3339();
        Ok(())
    }

    pub(super) async fn take_ready_auth_session_record(
        &self,
        id: &str,
    ) -> anyhow::Result<AuthSession> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let session = sessions
            .remove(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {id}"))?;
        if !session
            .status
            .eq_ignore_ascii_case(AuthSessionStatus::Ready.as_str())
        {
            sessions.insert(id.to_string(), session);
            anyhow::bail!("auth session is not ready");
        }
        Ok(session)
    }

    pub(super) async fn update_auth_session_record(
        &self,
        id: &str,
        input: UpdateAuthSession,
    ) -> anyhow::Result<AuthSession> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let current = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {id}"))?;
        if let Some(value) = input.status {
            current.status = value;
        }
        if let Some(value) = input.user_code {
            current.user_code = Some(value);
        }
        if let Some(value) = input.use_proxy {
            current.use_proxy = value;
        }
        if let Some(value) = input.verification_uri {
            current.verification_uri = Some(value);
        }
        if let Some(value) = input.verification_uri_complete {
            current.verification_uri_complete = Some(value);
        }
        if let Some(value) = input.state_json {
            current.state_json = Some(value);
        }
        if let Some(value) = input.context_json {
            current.context_json = Some(value);
        }
        if let Some(value) = input.result_json {
            current.result_json = Some(value);
        }
        if let Some(value) = input.expires_at {
            current.expires_at = Some(value);
        }
        if let Some(value) = input.poll_interval_seconds {
            current.poll_interval_seconds = Some(value);
        }
        if let Some(value) = input.last_error {
            current.last_error = Some(value);
        }
        if let Some(value) = input.error_code {
            current.error_code = Some(value);
        }
        current.updated_at = now_rfc3339();
        Ok(current.clone())
    }

    async fn delete_auth_session_record(&self, id: &str) -> anyhow::Result<()> {
        self.gw.auth_sessions.write().await.remove(id);
        Ok(())
    }

    pub(super) async fn restore_auth_session_record(
        &self,
        mut session: AuthSession,
    ) -> anyhow::Result<()> {
        session.updated_at = now_rfc3339();
        self.gw
            .auth_sessions
            .write()
            .await
            .insert(session.id.clone(), session);
        Ok(())
    }
    pub(crate) async fn cleanup_auth_sessions(&self) -> anyhow::Result<usize> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, session| !is_expired_at(session.expires_at.as_deref()));
        Ok(before.saturating_sub(sessions.len()))
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

    pub(super) async fn reconnect_provider_oauth_record(
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

    pub(super) async fn sync_provider_runtime_fields(
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
}

#[cfg(test)]
mod tests;

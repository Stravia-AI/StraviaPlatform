use super::*;

impl AdminService {
    pub(super) async fn create_auth_session_record(
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

    pub(in crate::admin) async fn get_auth_session_record(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<AuthSession>> {
        Ok(self.gw.auth_sessions.read().await.get(id).cloned())
    }

    pub(in crate::admin) async fn claim_pending_auth_session(
        &self,
        id: &str,
    ) -> anyhow::Result<AuthSession> {
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
    pub(super) async fn finish_claimed_auth_session(
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

    pub(super) async fn fail_claimed_auth_session(
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

    pub(in crate::admin) async fn take_ready_auth_session_record(
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

    pub(in crate::admin) async fn update_auth_session_record(
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

    pub(super) async fn delete_auth_session_record(&self, id: &str) -> anyhow::Result<()> {
        self.gw.auth_sessions.write().await.remove(id);
        Ok(())
    }

    pub(in crate::admin) async fn restore_auth_session_record(
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
}

use super::*;

fn validate_auth_browser_url(value: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(value)
        .map_err(|_| anyhow::anyhow!("vendor authorization URL is invalid"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "vendor authorization URL must be an absolute HTTP(S) URL without credentials"
    );
    Ok(())
}

impl AdminService {
    pub(super) async fn create_auth_session_record(
        &self,
        candidate: AuthSessionCandidate,
        auth_descriptor: stravia_vendor_sdk::AuthDescriptor,
        state: String,
        response: Option<stravia_vendor_sdk::AuthResponse>,
        scope: crate::plugin::VendorSessionScope,
        options: OAuthSessionStartOptions,
    ) -> anyhow::Result<AuthSession> {
        // Authentication sessions are process-local. The captured component,
        // private state, and cancellation token remain isolated to this session.
        if !self.gw.config.config_poll_interval.is_zero() {
            tracing::debug!(
                "creating oauth session in multi-replica mode \
                 — ensure the callback reaches this replica (session affinity required)"
            );
        }
        let (auth_url, user_code, verification_uri, interval_seconds) = match response {
            Some(stravia_vendor_sdk::AuthResponse::Authorization {
                url,
                user_code,
                verification_uri,
                interval_seconds,
            }) => (Some(url), user_code, verification_uri, interval_seconds),
            None if auth_descriptor.flow == stravia_vendor_sdk::AuthFlow::Manual => {
                (None, None, None, None)
            }
            _ => anyhow::bail!("vendor auth start did not return an authorization response"),
        };
        if let Some(value) = auth_url.as_deref() {
            validate_auth_browser_url(value)?;
        }
        if let Some(value) = verification_uri.as_deref() {
            validate_auth_browser_url(value)?;
        }

        let now = Utc::now();
        let scheme = match auth_descriptor.flow {
            stravia_vendor_sdk::AuthFlow::AuthorizationCode => AuthScheme::OAuthAuthCodePkce,
            stravia_vendor_sdk::AuthFlow::DeviceCode => AuthScheme::OAuthDeviceCode,
            stravia_vendor_sdk::AuthFlow::Manual => AuthScheme::SetupToken,
        };
        let listener_state =
            if auth_descriptor.flow != stravia_vendor_sdk::AuthFlow::AuthorizationCode {
                "not_required".to_string()
            } else if options.callback_mode == OAuthCallbackMode::Auto {
                "listening".to_string()
            } else {
                "not_started".to_string()
            };
        let verification_uri = verification_uri.or_else(|| auth_url.clone());
        let session = AuthSession {
            callback_mode: options.callback_mode,
            listener_state,
            listener_port: options.listener_port,
            redirect_uri: options.redirect_uri,
            fallback_reason: options.fallback_reason,
            id: stravia_runtime_contract::identifier::new_id(),
            provider_id: candidate.provider_id.clone(),
            driver_key: candidate.vendor_id,
            channel: candidate.channel,
            auth_descriptor,
            scheme: scheme.as_str().to_string(),
            status: AuthSessionStatus::Pending.as_str().to_string(),
            use_proxy: candidate.use_proxy,
            user_code,
            verification_uri,
            verification_uri_complete: auth_url,
            state_json: Some(serde_json::json!({ "state": state }).to_string()),
            context_json: None,
            result_json: None,
            expires_at: Some((now + chrono::Duration::minutes(10)).to_rfc3339()),
            poll_interval_seconds: interval_seconds
                .map(|seconds| i32::try_from(seconds).unwrap_or(i32::MAX)),
            last_error: None,
            error_code: None,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            vendor_runtime: Some(std::sync::Arc::new(
                crate::auth::types::VendorAuthSessionRuntime {
                    scope: std::sync::Arc::new(scope),
                    cancellation: stravia_runtime_contract::CancellationToken::new(),
                    publication: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                },
            )),
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
            let removed = sessions.remove(id);
            drop(sessions);
            if let Some(runtime) = removed.and_then(|session| session.vendor_runtime) {
                runtime.cancellation.cancel();
            }
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
        let runtime = terminal.then(|| session.vendor_runtime.clone()).flatten();
        if terminal {
            session.listener_state = "stopped".to_string();
        }
        session.updated_at = now_rfc3339();
        drop(sessions);
        if let Some(runtime) = runtime {
            runtime.cancellation.cancel();
        }
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
        if let Some(value) = input.verification_uri.as_deref() {
            validate_auth_browser_url(value)?;
        }
        if let Some(value) = input.verification_uri_complete.as_deref() {
            validate_auth_browser_url(value)?;
        }

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
        if let Some(session) = self.gw.auth_sessions.write().await.remove(id)
            && let Some(runtime) = session.vendor_runtime
        {
            runtime.cancellation.cancel();
        }
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
        let expired: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| is_expired_at(session.expires_at.as_deref()))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            if let Some(session) = sessions.remove(id)
                && let Some(runtime) = session.vendor_runtime
            {
                runtime.cancellation.cancel();
            }
        }
        Ok(expired.len())
    }
}

#[cfg(test)]
mod tests {
    use crate::Gateway;
    use crate::auth::{AuthSessionCandidate, OAuthCallbackMode, OAuthSessionStartOptions};
    use crate::config::GatewayConfig;

    #[tokio::test]
    async fn guest_script_authorization_url_is_not_stored() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let gw = Gateway::from_storage(
            GatewayConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..Default::default()
            },
            std::sync::Arc::new(crate::storage::MemoryStorage::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        )
        .await?;
        crate::plugin::test_support::install_distributed_vendor(&gw, "openai-codex").await?;
        let admin = gw.admin();
        let candidate = AuthSessionCandidate {
            vendor_id: "openai-codex".into(),
            channel: "codex".into(),
            provider_id: None,
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            protocol: None,
            options: Default::default(),
            credentials: Default::default(),
            use_proxy: false,
        };
        let (provider, auth_descriptor) =
            admin.provider_auth_candidate_snapshot(&candidate).await?;
        let scope = gw.create_vendor_session_scope(&candidate.vendor_id, provider)?;
        let error = admin
            .create_auth_session_record(
                candidate,
                auth_descriptor,
                "test-state".into(),
                Some(stravia_vendor_sdk::AuthResponse::Authorization {
                    url: "javascript:alert('session-secret')".into(),
                    user_code: None,
                    verification_uri: Some("https://auth.openai.com".into()),
                    interval_seconds: None,
                }),
                scope,
                OAuthSessionStartOptions {
                    callback_mode: OAuthCallbackMode::Manual,
                    redirect_uri: "http://localhost:1457/auth/callback".into(),
                    listener_port: None,
                    fallback_reason: None,
                },
            )
            .await
            .expect_err("guest script URL should be rejected");

        assert!(!error.to_string().contains("session-secret"));
        assert!(gw.auth_sessions.read().await.is_empty());
        Ok(())
    }
}

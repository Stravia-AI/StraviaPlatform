use super::*;

mod runtime;
mod session_store;

fn callback_parameter(url: &reqwest::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.into_owned())
        .or_else(|| {
            let fragment = url.fragment()?;
            reqwest::Url::parse(&format!("https://callback.invalid/?{fragment}"))
                .ok()?
                .query_pairs()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.into_owned())
        })
}

fn validate_auth_callback(
    session: &AuthSession,
    value: &str,
) -> Result<String, (&'static str, String, bool)> {
    let callback = reqwest::Url::parse(value.trim()).map_err(|_| {
        (
            "AUTH_CALLBACK_URL_INVALID",
            "OAuth callback URL is invalid".to_string(),
            false,
        )
    })?;
    let expected = reqwest::Url::parse(&session.redirect_uri).map_err(|_| {
        (
            "AUTH_SESSION_INVALIDATED",
            "authentication session redirect policy is invalid".to_string(),
            true,
        )
    })?;
    let same_endpoint = callback.scheme() == expected.scheme()
        && callback.host_str().map(str::to_ascii_lowercase)
            == expected.host_str().map(str::to_ascii_lowercase)
        && callback.port_or_known_default() == expected.port_or_known_default()
        && callback.path() == expected.path()
        && callback.username() == expected.username()
        && callback.password() == expected.password();
    if !same_endpoint {
        return Err((
            "AUTH_CALLBACK_URL_INVALID",
            "OAuth callback URL does not match this authentication session".to_string(),
            false,
        ));
    }
    if callback_parameter(&callback, "error").as_deref() == Some("access_denied") {
        return Err((
            "AUTH_ACCESS_DENIED",
            callback_parameter(&callback, "error_description")
                .unwrap_or_else(|| "OAuth authorization was denied".to_string()),
            true,
        ));
    }
    let expected_state = session
        .state_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| {
            value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    if expected_state.is_none() || callback_parameter(&callback, "state") != expected_state {
        return Err((
            "AUTH_CALLBACK_STATE_MISMATCH",
            "OAuth callback state mismatch".to_string(),
            false,
        ));
    }
    Ok(callback.to_string())
}

fn classify_auth_execution_error(error: &anyhow::Error) -> (&'static str, bool) {
    use stravia_vendor_runtime::RuntimeError;

    if error
        .to_string()
        .contains("incompatible with the installed plugin")
    {
        return ("AUTH_SESSION_INVALIDATED", true);
    }
    match error.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::Plugin {
            kind: stravia_vendor_sdk::ErrorKind::Upstream(_),
            ..
        }) => ("AUTH_EXCHANGE_RETRYABLE", false),
        Some(RuntimeError::Plugin {
            kind: stravia_vendor_sdk::ErrorKind::Auth,
            ..
        }) => ("AUTH_EXCHANGE_REJECTED", true),
        Some(RuntimeError::DeadlineExceeded) => ("AUTH_TIMEOUT", true),
        Some(RuntimeError::Cancelled) => ("AUTH_SESSION_CANCELLED", true),
        Some(_) => ("AUTH_EXCHANGE_FAILED", true),
        None => ("AUTH_EXCHANGE_FAILED", true),
    }
}

fn credential_bundle_from_response(
    response: stravia_vendor_sdk::AuthResponse,
) -> anyhow::Result<CredentialBundle> {
    let stravia_vendor_sdk::AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    } = response
    else {
        anyhow::bail!("vendor did not return credentials")
    };
    let string = |key: &str| {
        values
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let access_token = string("access_token")
        .or_else(|| string("session_token"))
        .or_else(|| string("apiKey"))
        .ok_or_else(|| anyhow::anyhow!("vendor credentials are missing an access token"))?;
    let scopes = values
        .get("scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("credential scopes must be strings"))
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .transpose()?
        .or_else(|| {
            string("scope").map(|scope| scope.split_ascii_whitespace().map(str::to_owned).collect())
        })
        .unwrap_or_default();
    let expires_at = expires_at_unix_ms
        .and_then(chrono::DateTime::<Utc>::from_timestamp_millis)
        .map(|value| value.to_rfc3339());
    let raw = Value::Object(values.into_iter().collect());
    Ok(CredentialBundle {
        access_token: Some(access_token),
        refresh_token: raw
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at,
        resource_url: raw
            .get("resource_url")
            .and_then(Value::as_str)
            .map(str::to_owned),
        subject_id: raw
            .get("subject_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        scopes,
        raw,
    })
}

fn auth_session_uses_profile(session: &AuthSession, provider_id: &str) -> bool {
    session.vendor_runtime.as_ref().map_or_else(
        || session.driver_key == provider_id,
        |runtime| runtime.scope.vendor_id == provider_id,
    )
}

impl AdminService {
    pub async fn affected_vendor_auth_sessions(&self, provider_id: &str) -> usize {
        self.gw
            .auth_sessions
            .read()
            .await
            .values()
            .filter(|session| auth_session_uses_profile(session, provider_id))
            .count()
    }

    pub async fn cancel_vendor_sessions(&self, provider_id: &str) -> usize {
        let removed = {
            let mut sessions = self.gw.auth_sessions.write().await;
            let ids: Vec<_> = sessions
                .iter()
                .filter(|(_, session)| auth_session_uses_profile(session, provider_id))
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| sessions.remove(&id))
                .collect::<Vec<_>>()
        };
        let count = removed.len();
        for session in removed {
            if let Some(runtime) = session.vendor_runtime {
                runtime.cancellation.cancel();
            }
        }
        count
    }

    pub async fn init_oauth_session(
        &self,
        candidate: AuthSessionCandidate,
        options: OAuthSessionStartOptions,
    ) -> anyhow::Result<AuthSessionInitData> {
        match super::provider_connection::ProviderConnection::new(self)
            .reconnect(super::provider_connection::ProviderReconnect::Start(
                super::provider_connection::ProviderReconnectStart::Authorization {
                    candidate: Box::new(candidate),
                    options,
                },
            ))
            .await?
        {
            super::provider_connection::ProviderReconnectResult::Redirect(started) => Ok(started),
            super::provider_connection::ProviderReconnectResult::Complete(_) => {
                unreachable!("auth start cannot complete an input")
            }
            _ => anyhow::bail!("authentication start returned an unexpected result"),
        }
    }

    pub(super) async fn init_oauth_session_record(
        &self,
        candidate: AuthSessionCandidate,
        options: OAuthSessionStartOptions,
    ) -> anyhow::Result<AuthSessionInitData> {
        let (mut provider, auth_descriptor) =
            self.provider_auth_candidate_snapshot(&candidate).await?;
        provider
            .operation_metadata
            .insert("use_proxy".into(), Value::Bool(candidate.use_proxy));
        let scope = self
            .gw
            .create_vendor_session_scope(&candidate.vendor_id, provider)?;
        let state = stravia_runtime_contract::identifier::new_id();
        let (response, publication) =
            if auth_descriptor.flow == stravia_vendor_sdk::AuthFlow::Manual {
                (None, None)
            } else {
                let execution = self
                    .gw
                    .execute_vendor_session(
                        &scope,
                        crate::plugin::VendorRequest::Auth(stravia_vendor_sdk::AuthRequest {
                            step: stravia_vendor_sdk::AuthStep::Start {
                                redirect_uri: options.redirect_uri.clone(),
                                state: state.clone(),
                            },
                        }),
                        crate::plugin::VendorCallContext::new(
                            stravia_runtime_contract::CancellationToken::new(),
                            stravia_runtime_contract::Deadline::fixed(
                                std::time::Instant::now() + std::time::Duration::from_secs(120),
                            ),
                        ),
                    )
                    .await?;
                let publication = execution.publication.write_fence().await?;
                let stravia_vendor_sdk::OperationOutput::Auth(response) = execution.output else {
                    anyhow::bail!("vendor returned the wrong authentication start result")
                };
                (Some(response), Some(publication))
            };
        let session = self
            .create_auth_session_record(candidate, auth_descriptor, state, response, scope, options)
            .await?;
        drop(publication);
        build_auth_session_init_data(&session)
    }

    async fn execute_auth_session_step(
        &self,
        session: &AuthSession,
        step: stravia_vendor_sdk::AuthStep,
    ) -> anyhow::Result<crate::plugin::VendorExecution> {
        let runtime = session
            .vendor_runtime
            .clone()
            .ok_or_else(|| anyhow::anyhow!("authentication session runtime is unavailable"))?;
        let mut context = crate::plugin::VendorCallContext::new(
            runtime.cancellation.clone(),
            stravia_runtime_contract::Deadline::fixed(
                std::time::Instant::now() + std::time::Duration::from_secs(10 * 60),
            ),
        );
        context
            .metadata
            .insert("use_proxy".into(), Value::Bool(session.use_proxy));
        self.gw
            .execute_vendor_session(
                &runtime.scope,
                crate::plugin::VendorRequest::Auth(stravia_vendor_sdk::AuthRequest { step }),
                context,
            )
            .await
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

        if session.auth_descriptor.flow != stravia_vendor_sdk::AuthFlow::DeviceCode {
            return Ok(build_auth_session_pending_data(&session));
        }

        let execution = match self
            .execute_auth_session_step(&session, stravia_vendor_sdk::AuthStep::Poll)
            .await
        {
            Ok(execution) => execution,
            Err(error) => {
                let (code, terminal) = classify_auth_execution_error(&error);
                let message = error.to_string();
                if terminal {
                    self.mark_oauth_session_error(&session.id, code, &message)
                        .await?;
                } else {
                    self.update_auth_session_record(
                        &session.id,
                        UpdateAuthSession {
                            error_code: Some(code.into()),
                            last_error: Some(message.clone()),
                            ..Default::default()
                        },
                    )
                    .await?;
                }
                return Err(coded_error(code, &message, serde_json::json!({})));
            }
        };
        let publication_token = execution.publication.clone();
        let publication = publication_token.write_fence().await?;
        let result = match execution.output {
            stravia_vendor_sdk::OperationOutput::Auth(
                stravia_vendor_sdk::AuthResponse::Pending {
                    retry_after_seconds,
                },
            ) => {
                let updated = self
                    .update_auth_session_record(
                        &session.id,
                        UpdateAuthSession {
                            poll_interval_seconds: retry_after_seconds
                                .map(|seconds| i32::try_from(seconds).unwrap_or(i32::MAX)),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(build_auth_session_pending_data(&updated))
            }
            stravia_vendor_sdk::OperationOutput::Auth(
                response @ stravia_vendor_sdk::AuthResponse::Credentials { .. },
            ) => {
                let bundle = credential_bundle_from_response(response)?;
                let runtime = session.vendor_runtime.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("authentication session runtime is unavailable")
                })?;
                *runtime.publication.lock().await = Some(publication_token);
                let updated = self
                    .update_auth_session_record(
                        &session.id,
                        UpdateAuthSession {
                            status: Some(AuthSessionStatus::Ready.as_str().to_string()),
                            result_json: Some(serde_json::to_string(&bundle)?),
                            last_error: Some(String::new()),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(build_auth_session_ready_data(&updated, &bundle))
            }
            _ => anyhow::bail!("vendor returned an invalid OAuth poll result"),
        };
        drop(publication);
        result
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
        if let Some(runtime) = &session.vendor_runtime {
            runtime.cancellation.cancel();
        }
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
        input: AuthCompletionInput,
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
        input: AuthCompletionInput,
    ) -> anyhow::Result<AuthSessionStatusData> {
        let session = self.claim_pending_auth_session(session_id).await?;
        if session
            .status
            .eq_ignore_ascii_case(AuthSessionStatus::Ready.as_str())
        {
            let bundle = parse_auth_session_bundle(&session)?;
            return Ok(build_auth_session_ready_data(&session, &bundle));
        }

        let step = match input.input {
            AuthCompletionValue::CallbackUrl { value } => {
                if session.auth_descriptor.flow != stravia_vendor_sdk::AuthFlow::AuthorizationCode {
                    self.fail_claimed_auth_session(
                        &session.id,
                        false,
                        "AUTH_INPUT_NOT_ALLOWED",
                        "this authentication flow does not accept a callback URL",
                    )
                    .await?;
                    return Err(coded_error(
                        "AUTH_INPUT_NOT_ALLOWED",
                        "this authentication flow does not accept a callback URL",
                        serde_json::json!({}),
                    ));
                }
                if session.callback_mode == OAuthCallbackMode::Manual
                    && !session
                        .auth_descriptor
                        .manual_input
                        .as_ref()
                        .is_some_and(|input| {
                            input.input_type == stravia_vendor_sdk::AuthManualInputType::CallbackUrl
                        })
                {
                    self.fail_claimed_auth_session(
                        &session.id,
                        false,
                        "AUTH_INPUT_NOT_ALLOWED",
                        "this authentication flow does not allow manual callback input",
                    )
                    .await?;
                    return Err(coded_error(
                        "AUTH_INPUT_NOT_ALLOWED",
                        "this authentication flow does not allow manual callback input",
                        serde_json::json!({}),
                    ));
                }
                let callback = match validate_auth_callback(&session, &value) {
                    Ok(callback) => callback,
                    Err((code, message, terminal)) => {
                        self.fail_claimed_auth_session(&session.id, terminal, code, &message)
                            .await?;
                        return Err(coded_error(code, &message, serde_json::json!({})));
                    }
                };
                stravia_vendor_sdk::AuthStep::Exchange {
                    callback_url: callback,
                }
            }
            AuthCompletionValue::Manual { value } => {
                let allowed = session
                    .auth_descriptor
                    .manual_input
                    .as_ref()
                    .is_some_and(|input| {
                        input.input_type == stravia_vendor_sdk::AuthManualInputType::Text
                    });
                if !allowed || value.trim().is_empty() {
                    self.fail_claimed_auth_session(
                        &session.id,
                        false,
                        "AUTH_INPUT_NOT_ALLOWED",
                        "this authentication flow does not accept manual text input",
                    )
                    .await?;
                    return Err(coded_error(
                        "AUTH_INPUT_NOT_ALLOWED",
                        "this authentication flow does not accept manual text input",
                        serde_json::json!({}),
                    ));
                }
                stravia_vendor_sdk::AuthStep::ManualInput { value }
            }
        };

        let exchange_result = self.execute_auth_session_step(&session, step).await;

        match exchange_result {
            Ok(execution) => {
                let completed = async {
                    let publication_token = execution.publication.clone();
                    let publication = publication_token.write_fence().await?;
                    let stravia_vendor_sdk::OperationOutput::Auth(
                        response @ stravia_vendor_sdk::AuthResponse::Credentials { .. },
                    ) = execution.output
                    else {
                        anyhow::bail!("vendor returned an invalid authentication result")
                    };
                    let bundle = credential_bundle_from_response(response)?;
                    let runtime = session.vendor_runtime.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("authentication session runtime is unavailable")
                    })?;
                    *runtime.publication.lock().await = Some(publication_token);
                    let updated = self
                        .finish_claimed_auth_session(&session.id, &bundle)
                        .await?;
                    drop(publication);
                    Ok::<_, anyhow::Error>(build_auth_session_ready_data(&updated, &bundle))
                }
                .await;
                match completed {
                    Ok(status) => Ok(status),
                    Err(error) => {
                        if let Some(runtime) = &session.vendor_runtime {
                            runtime.publication.lock().await.take();
                        }
                        let (code, terminal) = classify_auth_execution_error(&error);
                        let message = error.to_string();
                        if let Err(claim_error) = self
                            .fail_claimed_auth_session(&session.id, terminal, code, &message)
                            .await
                        {
                            return Err(error.context(claim_error.to_string()));
                        }
                        Err(coded_error(code, &message, serde_json::json!({})))
                    }
                }
            }
            Err(error) => {
                let (code, terminal) = classify_auth_execution_error(&error);
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
        self.create_provider_with_oauth_session_record(session_id, input)
            .await
    }

    pub(super) async fn create_provider_with_oauth_session_record(
        &self,
        session_id: &str,
        mut input: CreateProvider,
    ) -> anyhow::Result<Provider> {
        let session = self.take_ready_auth_session_record(session_id).await?;
        if is_expired_at(session.expires_at.as_deref()) {
            if let Some(runtime) = &session.vendor_runtime {
                runtime.cancellation.cancel();
            }
            anyhow::bail!("auth session expired");
        }
        let runtime = session
            .vendor_runtime
            .clone()
            .ok_or_else(|| anyhow::anyhow!("authentication session runtime is unavailable"))?;
        let bundle = parse_auth_session_bundle(&session)?;
        bundle
            .access_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("auth session missing access token"))?;

        let publication =
            runtime.publication.lock().await.take().ok_or_else(|| {
                anyhow::anyhow!("authentication session publication is unavailable")
            })?;
        let write_fence = publication.write_fence().await?;
        publication.ensure_current()?;
        if !runtime.scope.provider.credentials.is_empty() {
            input.credential = ProviderCredentialInput::Fields {
                values: runtime.scope.provider.credentials.clone(),
            };
        }
        let provider = match self.create_provider_from_input(input, true).await {
            Ok(provider) => provider,
            Err(error) => {
                drop(write_fence);
                *runtime.publication.lock().await = Some(publication);
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };
        if provider.vendor.as_deref() != Some(session.driver_key.as_str())
            || provider.channel.as_deref() != Some(session.channel.as_str())
        {
            let error = anyhow::anyhow!(
                "authentication session vendor or channel does not match the Provider"
            );
            if let Err(cleanup_error) = self.delete_provider(&provider.id).await {
                tracing::warn!(%cleanup_error, provider_id = %provider.id, "failed to roll back mismatched OAuth Provider");
            }
            drop(write_fence);
            *runtime.publication.lock().await = Some(publication);
            self.restore_auth_session_record(session).await?;
            return Err(error);
        }

        let credential_input =
            upsert_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let provisioned = async {
            publication.ensure_current()?;
            self.gw
                .storage
                .oauth_credentials()
                .upsert(&provider.id, credential_input)
                .await?;
            let preview = self
                .preview_provider_configuration(crate::admin::ProviderConfigurationPreviewInput {
                    provider_id: Some(provider.id.clone()),
                    vendor_id: session.driver_key.clone(),
                    channel: provider
                        .channel
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("OAuth Provider channel is missing"))?,
                    base_url: provider.base_url.clone(),
                    options: serde_json::from_str(&provider.vendor_options)?,
                    credentials: std::collections::BTreeMap::new(),
                })
                .await?;
            super::provider_connection::ensure_configuration_accepted(
                &preview,
                &provider.base_url,
            )?;
            publication.ensure_current()?;
            self.gw
                .vendor_plugins
                .store
                .recovered(&provider.id, "credentials")
                .await?;
            Ok::<_, anyhow::Error>(provider.clone())
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
                drop(write_fence);
                *runtime.publication.lock().await = Some(publication);
                self.restore_auth_session_record(session).await?;
                return Err(error.context("create oauth provider"));
            }
        };
        drop(write_fence);
        drop(publication);
        Ok(provider)
    }

    pub async fn get_provider_oauth_status(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderOAuthStatusData> {
        let provider = self.get_provider(id).await?;
        let driver_key = provider.vendor.clone().unwrap_or_default();

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
        let driver_key = provider.vendor.clone().unwrap_or_default();

        if driver_key.is_empty() {
            return Ok(build_provider_oauth_status(&provider, "", None, None));
        }

        let (_, operation, _) = self.gw.vendor_plugins.acquire(&driver_key)?;
        let publication = operation.publication_fence(
            stravia_runtime_contract::CancellationToken::new(),
            stravia_runtime_contract::Deadline::fixed(
                std::time::Instant::now() + std::time::Duration::from_secs(120),
            ),
        );
        drop(operation);
        let write_fence = publication.write_fence().await?;
        let current = self.get_provider(id).await?;
        anyhow::ensure!(
            serde_json::to_vec(&current)? == serde_json::to_vec(&provider)?,
            "provider changed while logging out"
        );
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
        drop(write_fence);
        drop(publication);

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
            if let Some(runtime) = &session.vendor_runtime {
                runtime.cancellation.cancel();
            }
            anyhow::bail!("auth session expired");
        }
        if provider.vendor.as_deref() != Some(session.driver_key.as_str())
            || provider.channel.as_deref() != Some(session.channel.as_str())
        {
            self.restore_auth_session_record(session).await?;
            anyhow::bail!("authentication session vendor or channel does not match the Provider");
        }
        let runtime = session
            .vendor_runtime
            .clone()
            .ok_or_else(|| anyhow::anyhow!("authentication session runtime is unavailable"))?;
        let bundle = parse_auth_session_bundle(&session)?;
        bundle
            .access_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("auth session missing access token"))?;
        let publication =
            runtime.publication.lock().await.take().ok_or_else(|| {
                anyhow::anyhow!("authentication session publication is unavailable")
            })?;
        let write_fence = publication.write_fence().await?;
        publication.ensure_current()?;
        let current = self.get_provider(provider_id).await?;
        if serde_json::to_vec(&current)? != serde_json::to_vec(&provider)? {
            drop(write_fence);
            *runtime.publication.lock().await = Some(publication);
            self.restore_auth_session_record(session).await?;
            anyhow::bail!("provider changed while binding authentication credentials");
        }

        let credential_input =
            upsert_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let result = async {
            publication.ensure_current()?;
            self.gw
                .storage
                .oauth_credentials()
                .upsert(&provider.id, credential_input)
                .await?;
            let updated = self
                .gw
                .storage
                .providers()
                .update(
                    &provider.id,
                    UpdateProvider {
                        auth_mode: Some("oauth".into()),
                        api_key: Some(String::new()),
                        adapter_credentials: Some(std::collections::BTreeMap::new()),
                        ..Default::default()
                    },
                )
                .await?;
            let preview = self
                .preview_provider_configuration(crate::admin::ProviderConfigurationPreviewInput {
                    provider_id: Some(updated.id.clone()),
                    vendor_id: session.driver_key.clone(),
                    channel: updated
                        .channel
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("OAuth Provider channel is missing"))?,
                    base_url: updated.base_url.clone(),
                    options: serde_json::from_str(&updated.vendor_options)?,
                    credentials: std::collections::BTreeMap::new(),
                })
                .await?;
            super::provider_connection::ensure_configuration_accepted(&preview, &updated.base_url)?;
            publication.ensure_current()?;
            self.gw
                .vendor_plugins
                .store
                .recovered(&provider.id, "credentials")
                .await?;
            Ok::<_, anyhow::Error>(updated)
        }
        .await;
        let updated = match result {
            Ok(provider) => provider,
            Err(error) => {
                if let Err(cleanup_error) = self
                    .gw
                    .storage
                    .oauth_credentials()
                    .delete(&provider.id)
                    .await
                {
                    tracing::warn!(%cleanup_error, provider_id = %provider.id, "failed to delete OAuth credential after bind failure");
                }
                if let Err(rollback_error) = self
                    .gw
                    .storage
                    .providers()
                    .update(
                        &provider.id,
                        UpdateProvider {
                            auth_mode: Some(provider.auth_mode.clone()),
                            api_key: Some(provider.api_key.clone()),
                            adapter_credentials: Some(
                                serde_json::from_str(&provider.adapter_credentials)
                                    .unwrap_or_default(),
                            ),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    tracing::warn!(%rollback_error, provider_id = %provider.id, "failed to restore Provider after OAuth bind failure");
                }
                drop(write_fence);
                *runtime.publication.lock().await = Some(publication);
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };
        drop(write_fence);
        drop(publication);
        Ok(updated)
    }
}

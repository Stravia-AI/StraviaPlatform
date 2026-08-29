use super::*;

pub(super) fn parse_auth_session_bundle(session: &AuthSession) -> anyhow::Result<CredentialBundle> {
    let raw = session
        .result_json
        .as_deref()
        .context("auth session is missing result_json")?;
    serde_json::from_str(raw).context("parse auth session credential bundle")
}

pub(super) fn stored_credential_from_oauth(
    oauth: &OAuthCredential,
    driver_key: &str,
) -> StoredCredential {
    let scopes: Vec<String> = serde_json::from_str(&oauth.scopes).unwrap_or_default();
    let meta: Value = serde_json::from_str(&oauth.meta).unwrap_or(Value::Null);
    StoredCredential {
        driver_key: driver_key.to_string(),
        scheme: if oauth.scheme.is_empty() {
            AuthScheme::OAuthAuthCodePkce.as_str().to_string()
        } else {
            oauth.scheme.clone()
        },
        access_token: normalized_optional(Some(&oauth.access_token)),
        refresh_token: normalized_optional(oauth.refresh_token.as_deref()),
        expires_at: normalized_optional(oauth.expires_at.as_deref()),
        resource_url: normalized_optional(oauth.resource_url.as_deref()),
        subject_id: normalized_optional(oauth.subject_id.as_deref()),
        scopes,
        meta,
    }
}

pub(super) fn upsert_credential_from_bundle(
    driver_key: &str,
    scheme: &str,
    bundle: &CredentialBundle,
) -> UpsertOAuthCredential {
    let scopes_json = serde_json::to_string(&bundle.scopes).unwrap_or_else(|_| "[]".to_string());
    let meta_json = serde_json::to_string(&bundle.raw).unwrap_or_else(|_| "{}".to_string());
    UpsertOAuthCredential {
        driver_key: driver_key.to_string(),
        scheme: scheme.to_string(),
        access_token: bundle.access_token.clone().unwrap_or_default(),
        refresh_token: bundle.refresh_token.clone(),
        expires_at: bundle.expires_at.clone(),
        resource_url: bundle.resource_url.clone(),
        subject_id: bundle.subject_id.clone(),
        scopes: Some(scopes_json),
        meta: Some(meta_json),
    }
}

pub(super) fn upsert_credential_from_oauth(oauth: &OAuthCredential) -> UpsertOAuthCredential {
    UpsertOAuthCredential {
        driver_key: oauth.driver_key.clone(),
        scheme: oauth.scheme.clone(),
        access_token: oauth.access_token.clone(),
        refresh_token: oauth.refresh_token.clone(),
        expires_at: oauth.expires_at.clone(),
        resource_url: oauth.resource_url.clone(),
        subject_id: oauth.subject_id.clone(),
        scopes: Some(oauth.scopes.clone()),
        meta: Some(oauth.meta.clone()),
    }
}

pub(super) fn stored_credential_from_bundle(
    driver_key: &str,
    scheme: &str,
    bundle: &CredentialBundle,
) -> StoredCredential {
    StoredCredential {
        driver_key: driver_key.to_string(),
        scheme: scheme.to_string(),
        access_token: normalized_optional(bundle.access_token.as_deref()),
        refresh_token: normalized_optional(bundle.refresh_token.as_deref()),
        expires_at: normalized_optional(bundle.expires_at.as_deref()),
        resource_url: normalized_optional(bundle.resource_url.as_deref()),
        subject_id: normalized_optional(bundle.subject_id.as_deref()),
        scopes: bundle.scopes.clone(),
        meta: bundle.raw.clone(),
    }
}

pub(super) fn build_provider_oauth_status(
    provider: &Provider,
    driver_key: &str,
    status_override: Option<String>,
    fallback_error: Option<String>,
) -> ProviderOAuthStatusData {
    // This version is used when we don't have an OAuthCredential loaded.
    let status =
        status_override.unwrap_or_else(|| AuthBindingStatus::Disconnected.as_str().to_string());
    ProviderOAuthStatusData {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        driver_key: driver_key.to_string(),
        status,
        expires_at: None,
        resource_url: normalized_optional(Some(provider.base_url.as_str())),
        subject_id: None,
        last_error: fallback_error.filter(|value| !value.trim().is_empty()),
        updated_at: Some(provider.updated_at.clone()),
        has_refresh_token: false,
    }
}

pub(super) fn build_provider_oauth_status_from_credential(
    provider: &Provider,
    driver_key: &str,
    oauth: &OAuthCredential,
) -> ProviderOAuthStatusData {
    let status = match oauth.status.as_str() {
        "connected" => AuthBindingStatus::Connected.as_str().to_string(),
        "refreshing" => AuthBindingStatus::Pending.as_str().to_string(),
        "error" => AuthBindingStatus::Error.as_str().to_string(),
        _ => AuthBindingStatus::Disconnected.as_str().to_string(),
    };
    let has_refresh_token = oauth
        .refresh_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    ProviderOAuthStatusData {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        driver_key: driver_key.to_string(),
        status,
        expires_at: normalized_optional(oauth.expires_at.as_deref()),
        resource_url: normalized_optional(oauth.resource_url.as_deref())
            .or_else(|| normalized_optional(Some(provider.base_url.as_str()))),
        subject_id: normalized_optional(oauth.subject_id.as_deref()),
        last_error: oauth.last_error.clone(),
        updated_at: Some(oauth.updated_at.clone()),
        has_refresh_token,
    }
}

pub(super) fn build_auth_session_init_data(
    session: &AuthSession,
) -> anyhow::Result<AuthSessionInitData> {
    Ok(AuthSessionInitData {
        session_id: session.id.clone(),
        vendor: session.driver_key.clone(),
        scheme: session.scheme.clone(),
        auth_url: session
            .verification_uri_complete
            .clone()
            .or_else(|| session.verification_uri.clone())
            .unwrap_or_default(),
        user_code: session.user_code.clone(),
        callback_mode: session.callback_mode,
        listener_state: session.listener_state.clone(),
        listener_port: session.listener_port,
        redirect_uri: session.redirect_uri.clone(),
        fallback_reason: session.fallback_reason.clone(),
        expires_in: remaining_seconds_until(session.expires_at.as_deref()),
        interval: session.poll_interval_seconds.unwrap_or(2),
    })
}

pub(super) fn build_auth_session_pending_data(session: &AuthSession) -> AuthSessionStatusData {
    AuthSessionStatusData::Pending {
        scheme: session.scheme.clone(),
        auth_url: session
            .verification_uri_complete
            .clone()
            .or_else(|| session.verification_uri.clone())
            .unwrap_or_default(),
        user_code: session.user_code.clone(),
        callback_mode: session.callback_mode,
        listener_state: session.listener_state.clone(),
        listener_port: session.listener_port,
        redirect_uri: session.redirect_uri.clone(),
        fallback_reason: session.fallback_reason.clone(),
        error_code: session.error_code.clone(),
        last_error: session.last_error.clone(),
        expires_in: remaining_seconds_until(session.expires_at.as_deref()),
        interval: session.poll_interval_seconds.unwrap_or(2),
    }
}

pub(super) fn build_auth_session_exchanging_data(session: &AuthSession) -> AuthSessionStatusData {
    AuthSessionStatusData::Exchanging {
        expires_in: remaining_seconds_until(session.expires_at.as_deref()),
    }
}

pub(super) fn build_auth_session_ready_data(
    session: &AuthSession,
    bundle: &CredentialBundle,
) -> AuthSessionStatusData {
    AuthSessionStatusData::Ready {
        expires_in: remaining_seconds_until(
            bundle
                .expires_at
                .as_deref()
                .or(session.expires_at.as_deref()),
        ),
        resource_url: bundle.resource_url.clone(),
    }
}

pub(super) fn remaining_seconds_until(expires_at: Option<&str>) -> i64 {
    expires_at
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(parse_datetime_utc)
        .map(|value| (value - Utc::now()).num_seconds().max(0))
        .unwrap_or(0)
}

pub(super) fn is_expired_at(expires_at: Option<&str>) -> bool {
    expires_at
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(parse_datetime_utc)
        .map(|value| value <= Utc::now())
        .unwrap_or(false)
}

pub(super) fn parse_datetime_utc(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|value| DateTime::<Utc>::from_naive_utc_and_offset(value, Utc))
        })
}

pub(super) fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

pub(super) fn normalized_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

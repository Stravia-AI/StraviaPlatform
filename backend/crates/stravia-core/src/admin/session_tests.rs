use super::*;
use crate::auth::{
    AuthExchangeInput, AuthSessionInitData, OAuthCallbackMode, OAuthSessionStartOptions,
};
use crate::config::GatewayConfig;
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

const FAR_FUTURE_RFC3339: &str = "2099-01-01T00:00:00Z";
const PAST_RFC3339: &str = "2000-01-01T00:00:00Z";
const CODEX_RUNTIME_URL: &str = "https://chatgpt.com/backend-api/codex";

#[tokio::test]
async fn manual_oauth_session_exposes_its_effective_callback_contract() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = init_codex_session(&gw).await?;

    assert_eq!(init.callback_mode, OAuthCallbackMode::Manual);
    assert_eq!(init.redirect_uri, "http://localhost:1457/auth/callback");
    assert_eq!(init.listener_state, "not_started");
    assert_eq!(init.listener_port, None);
    assert_eq!(init.fallback_reason, None);
    assert!(
        init.auth_url
            .contains("redirect_uri=http%3A%2F%2Flocalhost%3A1457%2Fauth%2Fcallback")
    );

    Ok(())
}

#[tokio::test]
async fn oauth_session_is_shared_across_admin_instances_and_cancel_deletes_it() -> anyhow::Result<()>
{
    let gw = build_gateway().await?;

    let init = init_codex_session(&gw).await?;
    let status = gw
        .admin()
        .get_oauth_session_status(&init.session_id)
        .await?;
    assert!(matches!(status, AuthSessionStatusData::Pending { .. }));

    gw.admin().cancel_oauth_session(&init.session_id).await?;
    assert!(
        gw.admin()
            .get_auth_session_record(&init.session_id)
            .await?
            .is_none()
    );

    let err = gw
        .admin()
        .get_oauth_session_status(&init.session_id)
        .await
        .expect_err("cancelled session should be removed");
    assert!(err.to_string().contains("auth session not found"));

    Ok(())
}

#[tokio::test]
async fn invalid_callback_keeps_session_pending_for_retry() -> anyhow::Result<()> {
    let gw = build_gateway().await?;

    let init = init_codex_session(&gw).await?;
    let err = gw
        .admin()
        .complete_oauth_session(
            &init.session_id,
            AuthExchangeInput {
                callback_url: "https://app.example/callback?code=test-code&state=wrong-state"
                    .to_string(),
            },
        )
        .await
        .expect_err("invalid callback state should fail the exchange");

    assert!(
        err.to_string().contains("state"),
        "unexpected complete error: {err:#}"
    );
    let session = gw
        .admin()
        .get_auth_session_record(&init.session_id)
        .await?
        .expect("retryable session remains available");
    assert_eq!(session.status, AuthSessionStatus::Pending.as_str());
    assert!(matches!(
        gw.admin()
            .get_oauth_session_status(&init.session_id)
            .await?,
        AuthSessionStatusData::Pending {
            ref error_code,
            ref last_error,
            ..
        } if error_code.as_deref() == Some("AUTH_CALLBACK_STATE_MISMATCH")
            && last_error.as_deref().is_some_and(|message| message.contains("state"))
    ));

    Ok(())
}

#[tokio::test]
async fn denied_oauth_callback_becomes_a_terminal_session_error() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = init_codex_session(&gw).await?;
    let state = reqwest::Url::parse(&init.auth_url)?
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("authorization URL should contain state");

    gw.admin()
        .complete_oauth_session(
            &init.session_id,
            AuthExchangeInput {
                callback_url: format!(
                    "https://localhost/callback?error=access_denied&error_description=Denied&state={state}"
                ),
            },
        )
        .await
        .expect_err("access denial must fail completion");

    let status = gw
        .admin()
        .get_oauth_session_status(&init.session_id)
        .await?;
    assert!(matches!(
        status,
        AuthSessionStatusData::Error { ref code, .. } if code == "AUTH_ACCESS_DENIED"
    ));

    Ok(())
}

#[tokio::test]
async fn oauth_completion_claim_allows_only_one_exchange() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = init_codex_session(&gw).await?;

    let claimed = gw
        .admin()
        .claim_pending_auth_session(&init.session_id)
        .await?;
    assert_eq!(claimed.status, AuthSessionStatus::Exchanging.as_str());
    assert!(matches!(
        gw.admin()
            .update_oauth_session_proxy(&init.session_id, true)
            .await?,
        AuthSessionStatusData::Exchanging { .. }
    ));
    assert!(
        gw.admin()
            .get_auth_session_record(&init.session_id)
            .await?
            .expect("exchanging session")
            .use_proxy
    );

    let error = gw
        .admin()
        .claim_pending_auth_session(&init.session_id)
        .await
        .expect_err("a second completion must not exchange the same code");
    assert!(error.to_string().contains("AUTH_COMPLETION_IN_PROGRESS"));

    Ok(())
}

#[tokio::test]
async fn pending_oauth_session_uses_the_latest_proxy_setting() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = init_codex_session(&gw).await?;

    gw.admin()
        .update_oauth_session_proxy(&init.session_id, true)
        .await?;
    let session = gw
        .admin()
        .get_auth_session_record(&init.session_id)
        .await?
        .expect("pending session");
    assert!(session.use_proxy);

    Ok(())
}

#[tokio::test]
async fn completing_a_ready_session_is_idempotent() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = init_codex_session(&gw).await?;
    seed_ready_session(
        &gw.admin(),
        &init.session_id,
        CredentialBundle {
            access_token: Some("ready-token".to_string()),
            expires_at: Some(FAR_FUTURE_RFC3339.to_string()),
            ..Default::default()
        },
    )
    .await?;

    let status = gw
        .admin()
        .complete_oauth_session(
            &init.session_id,
            AuthExchangeInput {
                callback_url: "https://localhost/callback?code=unused&state=unused".to_string(),
            },
        )
        .await?;
    assert!(matches!(status, AuthSessionStatusData::Ready { .. }));

    Ok(())
}

#[tokio::test]
async fn timeout_and_cleanup_remove_expired_sessions() -> anyhow::Result<()> {
    let gw = build_gateway().await?;

    let timed_out = init_codex_session(&gw).await?;
    gw.admin()
        .update_auth_session_record(
            &timed_out.session_id,
            UpdateAuthSession {
                expires_at: Some(PAST_RFC3339.to_string()),
                ..Default::default()
            },
        )
        .await?;

    let status = gw
        .admin()
        .get_oauth_session_status(&timed_out.session_id)
        .await?;
    assert!(matches!(
        status,
        AuthSessionStatusData::Error { ref code, .. } if code == "AUTH_TIMEOUT"
    ));
    assert!(
        gw.admin()
            .get_auth_session_record(&timed_out.session_id)
            .await?
            .is_none()
    );

    let stale_ready = init_codex_session(&gw).await?;
    seed_ready_session(
        &gw.admin(),
        &stale_ready.session_id,
        CredentialBundle {
            access_token: Some("stale-access-token".to_string()),
            refresh_token: Some("stale-refresh-token".to_string()),
            expires_at: Some(PAST_RFC3339.to_string()),
            resource_url: None,
            subject_id: None,
            scopes: vec![],
            raw: json!({ "access_token": "stale-access-token" }),
        },
    )
    .await?;

    let removed = gw.admin().cleanup_auth_sessions().await?;
    assert_eq!(removed, 1);
    assert!(
        gw.admin()
            .get_auth_session_record(&stale_ready.session_id)
            .await?
            .is_none()
    );

    Ok(())
}

#[tokio::test]
async fn ready_session_is_single_use_and_provider_status_exposes_runtime_url() -> anyhow::Result<()>
{
    let gw = build_gateway().await?;

    let init = init_codex_session(&gw).await?;
    seed_ready_session(
        &gw.admin(),
        &init.session_id,
        CredentialBundle {
            access_token: Some("test-access-token".to_string()),
            refresh_token: Some("test-refresh-token".to_string()),
            expires_at: Some(FAR_FUTURE_RFC3339.to_string()),
            resource_url: None,
            subject_id: Some("acct_test".to_string()),
            scopes: vec!["openid".to_string(), "offline_access".to_string()],
            raw: json!({ "access_token": "test-access-token" }),
        },
    )
    .await?;

    let provider = gw
        .admin()
        .create_provider_with_oauth_session(&init.session_id, oauth_provider_input(&gw).await?)
        .await?;

    assert_eq!(provider.effective_auth_mode(), "oauth");
    assert_eq!(provider.base_url, CODEX_RUNTIME_URL);
    assert!(
        gw.admin()
            .get_auth_session_record(&init.session_id)
            .await?
            .is_none()
    );

    let err = gw
        .admin()
        .create_provider_with_oauth_session(&init.session_id, oauth_provider_input(&gw).await?)
        .await
        .expect_err("consumed ready session should not be reusable");
    assert!(err.to_string().contains("auth session not found"));

    let status = gw.admin().get_provider_oauth_status(&provider.id).await?;
    assert_eq!(status.status, AuthBindingStatus::Connected.as_str());
    assert_eq!(status.resource_url.as_deref(), Some(CODEX_RUNTIME_URL));

    Ok(())
}

async fn init_codex_session(gw: &Gateway) -> anyhow::Result<AuthSessionInitData> {
    gw.admin()
        .init_oauth_session(
            "codex",
            false,
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "http://localhost:1457/auth/callback".to_string(),
                listener_port: None,
                fallback_reason: None,
            },
        )
        .await
}

async fn build_gateway() -> anyhow::Result<Gateway> {
    let config = GatewayConfig {
        data_dir: test_data_dir(),
        ..Default::default()
    };
    let (gw, _log_rx) = Gateway::new(config).await?;
    Ok(gw)
}

fn test_data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("stravia-oauth-admin-tests-{}", Uuid::new_v4()))
}

async fn oauth_provider_input(gw: &Gateway) -> anyhow::Result<CreateProvider> {
    let catalog = gw.provider_catalog.providers().await;
    let provider = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "openai")
        .ok_or_else(|| anyhow::anyhow!("OpenAI missing from built-in Catalog"))?;
    let channel = provider
        .channels
        .iter()
        .find(|channel| channel.id == "codex")
        .ok_or_else(|| anyhow::anyhow!("Codex channel missing from built-in Catalog"))?;
    Ok(CreateProvider {
        name: Some(format!("oauth-provider-{}", Uuid::new_v4())),
        source: ProviderSourceInput::Catalog {
            provider_id: provider.id.clone(),
            channel_id: channel.id.clone(),
            fingerprint: channel.fingerprint.clone(),
            base_url_override: None,
        },
        credential: ProviderCredentialInput::None,
        use_proxy: false,
    })
}

async fn seed_ready_session(
    admin: &AdminService,
    session_id: &str,
    bundle: CredentialBundle,
) -> anyhow::Result<()> {
    admin
        .update_auth_session_record(
            session_id,
            UpdateAuthSession {
                status: Some(AuthSessionStatus::Ready.as_str().to_string()),
                result_json: Some(serde_json::to_string(&bundle)?),
                expires_at: bundle.expires_at.clone(),
                last_error: Some(String::new()),
                ..Default::default()
            },
        )
        .await?;
    Ok(())
}

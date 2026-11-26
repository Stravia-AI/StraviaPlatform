use std::path::Path;
use std::sync::Arc;

use super::*;
use axum::body::to_bytes;
use stravia_core::storage::MemoryStorage;
use tokio::io::{AsyncBufReadExt, BufReader};

async fn memory_gateway(data_dir: &Path) -> anyhow::Result<Gateway> {
    Gateway::from_storage(
        stravia_core::config::GatewayConfig {
            data_dir: data_dir.to_path_buf(),
            ..Default::default()
        },
        Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new())),
    )
    .await
}

#[tokio::test]
async fn callback_success_copy_uses_only_the_supported_locale_allow_list() -> anyhow::Result<()> {
    for (requested, lang, title, message) in [
        (
            None,
            "en-US",
            "OAuth complete",
            "Authorization succeeded. Return to Stravia to save the provider.",
        ),
        (
            Some("en-US"),
            "en-US",
            "OAuth complete",
            "Authorization succeeded. Return to Stravia to save the provider.",
        ),
        (
            Some("zh-TW"),
            "en-US",
            "OAuth complete",
            "Authorization succeeded. Return to Stravia to save the provider.",
        ),
        (
            Some("zh-CN"),
            "zh-CN",
            "OAuth 已完成",
            "授权成功。请返回 Stravia 保存 Provider。",
        ),
    ] {
        let copy = CallbackLocale::from_requested(requested).copy();
        let response = callback_html(
            StatusCode::OK,
            copy.lang,
            copy.complete_title,
            copy.complete_message,
        );
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let body = String::from_utf8(body.to_vec())?;

        assert!(body.contains(&format!("<html lang=\"{lang}\">")));
        assert!(body.contains(title));
        assert!(body.contains(message));
    }

    Ok(())
}

fn fixed_policy(primary: u16, fallback: Option<u16>) -> AuthCallback {
    AuthCallback {
        bind_host: "127.0.0.1".into(),
        redirect_host: "localhost".into(),
        path: "/auth/callback".into(),
        port: AuthCallbackPort::Fixed { primary, fallback },
        manual_redirect_uri: Some("http://localhost:1457/auth/callback".into()),
        cancel_path: Some("/cancel".into()),
    }
}

async fn assert_cancel_request(stream: &mut TcpStream, path: &str) -> io::Result<()> {
    let mut request = BufReader::new(stream);
    let mut line = Vec::new();
    request.read_until(b'\n', &mut line).await?;
    assert_eq!(
        line.as_slice(),
        format!("GET {path} HTTP/1.1\r\n").as_bytes()
    );
    // Consume the headers before closing the connection to avoid a TCP reset.
    loop {
        line.clear();
        request.read_until(b'\n', &mut line).await?;
        if line == b"\r\n" {
            return Ok(());
        }
        if !line.ends_with(b"\r\n") {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete cancel request",
            ));
        }
    }
}

// An explicit fallback port must be free when the production binder takes it.
async fn unused_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    Ok(listener.local_addr()?.port())
}

#[tokio::test]
async fn fixed_callback_reclaims_a_stale_primary_listener_via_declared_cancel_path()
-> anyhow::Result<()> {
    let stale = TcpListener::bind(("127.0.0.1", 0)).await?;
    let primary = stale.local_addr()?.port();
    let cancelled = tokio::spawn(async move {
        let (mut stream, _) = stale.accept().await?;
        assert_cancel_request(&mut stream, "/oauth/cancel/custom").await?;
        drop(stale);
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await?;
        Ok::<_, io::Error>(())
    });
    let mut policy = fixed_policy(primary, None);
    policy.cancel_path = Some("/oauth/cancel/custom".into());

    let binding = bind_callback_listener(policy).await?;
    match binding {
        CallbackBinding::Listening { port, .. } => assert_eq!(port, primary),
        CallbackBinding::ManualFallback { .. } => panic!("primary should be reclaimed"),
    }
    cancelled.await??;

    Ok(())
}

#[tokio::test]
async fn fixed_callback_does_not_reclaim_primary_after_failed_cancel_response() -> anyhow::Result<()>
{
    let primary = TcpListener::bind(("127.0.0.1", 0)).await?;
    let primary_port = primary.local_addr()?.port();
    let fallback_port = unused_port().await?;
    let (release_primary, mut stop_primary) = tokio::sync::oneshot::channel();
    let rejected_cancel = tokio::spawn(async move {
        let (mut stream, _) = tokio::select! {
            accepted = primary.accept() => accepted?,
            _ = &mut stop_primary => return Err(io::Error::other("cancel request not received")),
        };
        assert_cancel_request(&mut stream, "/cancel").await?;
        // Make the primary available so an erroneous reclaim cannot fail on AddrInUse.
        drop(primary);
        stream
            .write_all(b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\n\r\n")
            .await?;
        Ok::<_, io::Error>(())
    });

    let binding = bind_callback_listener(fixed_policy(primary_port, Some(fallback_port))).await?;
    let _ = release_primary.send(());
    match binding {
        CallbackBinding::Listening { port, .. } => assert_eq!(port, fallback_port),
        CallbackBinding::ManualFallback { .. } => panic!("available fallback should be used"),
    }
    rejected_cancel.await??;

    Ok(())
}

#[tokio::test]
async fn fixed_callback_without_cancel_path_does_not_contact_the_primary_listener()
-> anyhow::Result<()> {
    let primary = TcpListener::bind(("127.0.0.1", 0)).await?;
    let primary_port = primary.local_addr()?.port();
    let fallback_port = unused_port().await?;
    let mut policy = fixed_policy(primary_port, Some(fallback_port));
    policy.cancel_path = None;

    let binding = bind_callback_listener(policy).await?;
    match binding {
        CallbackBinding::Listening { port, .. } => assert_eq!(port, fallback_port),
        CallbackBinding::ManualFallback { .. } => panic!("available fallback should be used"),
    }
    assert_eq!(
        primary.into_std()?.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    Ok(())
}

#[tokio::test]
async fn fixed_callback_falls_back_to_manual_when_both_registered_ports_are_busy()
-> anyhow::Result<()> {
    let primary = TcpListener::bind(("127.0.0.1", 0)).await?;
    let fallback = TcpListener::bind(("127.0.0.1", 0)).await?;
    let primary_port = primary.local_addr()?.port();
    let fallback_port = fallback.local_addr()?.port();
    let (release_primary, mut keep_primary_busy) = tokio::sync::oneshot::channel();
    let occupied_primary = tokio::spawn(async move {
        let (mut stream, _) = tokio::select! {
            accepted = primary.accept() => accepted?,
            _ = &mut keep_primary_busy => return Err(io::Error::other("cancel request not received")),
        };
        assert_cancel_request(&mut stream, "/cancel").await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await?;
        stream.shutdown().await?;
        drop(stream);
        let _ = keep_primary_busy.await;
        Ok::<_, io::Error>(())
    });

    let binding = bind_callback_listener(fixed_policy(primary_port, Some(fallback_port))).await?;
    match binding {
        CallbackBinding::ManualFallback { reason } => {
            assert_eq!(reason, "callback_ports_unavailable")
        }
        CallbackBinding::Listening { .. } => panic!("occupied ports must use manual mode"),
    }
    let _ = release_primary.send(());
    occupied_primary.await??;
    Ok(())
}

fn auth_candidate(vendor_id: &str, channel: &str, base_url: &str) -> AuthSessionCandidate {
    AuthSessionCandidate {
        vendor_id: vendor_id.into(),
        channel: channel.into(),
        provider_id: None,
        base_url: base_url.into(),
        protocol: None,
        options: Default::default(),
        credentials: Default::default(),
        proxy_mode: "none".into(),
    }
}

#[tokio::test]
async fn hosted_callback_uses_manual_code_without_opening_a_listener() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    stravia_core::plugin::test_support::install_distributed_vendor(&gateway, "antigravity").await?;
    let manager = OAuthCallbackManager::new(gateway.clone());
    let session = manager
        .init_session(
            auth_candidate(
                "antigravity",
                "oauth",
                "https://daily-cloudcode-pa.googleapis.com",
            ),
            OAuthCallbackMode::Auto,
            None,
        )
        .await?;
    let status = gateway
        .admin()
        .get_oauth_session_status(&session.session_id)
        .await?;
    let value = serde_json::to_value(&status)?;
    assert_eq!(value["callback_mode"], "manual");
    assert_eq!(value["listener_state"], "not_required");
    assert!(
        session
            .auth_url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://accounts.google.com/o/oauth2/auth?"))
    );
    assert!(
        manager
            .inner
            .active
            .lock()
            .await
            .get(&session.session_id)
            .unwrap()
            .shutdown
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_sessions_keep_independent_listener_lifetimes() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    stravia_core::plugin::test_support::install_distributed_vendor(&gateway, "openai-codex")
        .await?;
    stravia_core::plugin::test_support::install_distributed_vendor(&gateway, "devin").await?;
    let manager = OAuthCallbackManager::new(gateway.clone());

    let first = manager
        .init_session(
            auth_candidate("devin", "devin", "https://server.codeium.com"),
            OAuthCallbackMode::Auto,
            None,
        )
        .await?;
    let second = manager
        .init_session(
            auth_candidate(
                "openai-codex",
                "codex",
                "https://chatgpt.com/backend-api/codex",
            ),
            OAuthCallbackMode::Manual,
            None,
        )
        .await?;
    let first_status = gateway
        .admin()
        .get_oauth_session_status(&first.session_id)
        .await?;
    let second_status = gateway
        .admin()
        .get_oauth_session_status(&second.session_id)
        .await?;

    assert!(matches!(
        first_status,
        stravia_core::auth::AuthSessionStatusData::Pending { .. }
    ));
    assert!(matches!(
        second_status,
        stravia_core::auth::AuthSessionStatusData::Pending { .. }
    ));

    Ok(())
}

#[tokio::test]
async fn invalid_callback_keeps_the_listener_and_session_available_for_retry() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    stravia_core::plugin::test_support::install_distributed_vendor(&gateway, "openai-codex")
        .await?;
    let init = gateway
        .admin()
        .init_oauth_session(
            auth_candidate(
                "openai-codex",
                "codex",
                "https://chatgpt.com/backend-api/codex",
            ),
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "http://localhost:1457/auth/callback".to_string(),
                listener_port: None,
                fallback_reason: None,
            },
        )
        .await?;
    let (shutdown, mut receiver) = watch::channel(false);
    let response = oauth_callback_handler(
        State(CallbackState {
            gateway: gateway.clone(),
            session_id: init.session_id.clone(),
            redirect_uri: init.redirect_uri,
            locale: CallbackLocale::EnUs,
            shutdown: shutdown.clone(),
        }),
        OriginalUri("/auth/callback?code=bad&state=wrong".parse()?),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.changed())
            .await
            .is_err()
    );
    assert!(matches!(
        gateway
            .admin()
            .get_oauth_session_status(&init.session_id)
            .await?,
        stravia_core::auth::AuthSessionStatusData::Pending {
            ref error_code,
            ..
        } if error_code.as_deref() == Some("AUTH_CALLBACK_STATE_MISMATCH")
    ));

    Ok(())
}

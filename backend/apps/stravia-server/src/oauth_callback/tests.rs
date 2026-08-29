use super::*;
use axum::body::to_bytes;

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

fn fixed_policy(primary: u16, fallback: u16) -> OAuthCallbackPolicy {
    OAuthCallbackPolicy {
        bind_host: "127.0.0.1",
        redirect_host: "localhost",
        path: "/auth/callback",
        port: OAuthCallbackPort::Fixed { primary, fallback },
        manual_redirect_uri: "http://localhost:1457/auth/callback",
        cancel_path: Some("/cancel"),
    }
}

async fn unused_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    Ok(listener.local_addr()?.port())
}

#[tokio::test]
async fn fixed_callback_reclaims_a_stale_primary_listener_via_cancel() -> anyhow::Result<()> {
    let stale = TcpListener::bind(("127.0.0.1", 0)).await?;
    let primary = stale.local_addr()?.port();
    let fallback = unused_port().await?;
    let cancelled = tokio::spawn(async move {
        let (mut stream, _) = stale.accept().await?;
        let mut request = [0_u8; 128];
        let count = stream.read(&mut request).await?;
        assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /cancel HTTP/1.1"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await?;
        Ok::<_, io::Error>(())
    });

    let binding = bind_callback_listener(fixed_policy(primary, fallback)).await?;
    cancelled.await??;
    match binding {
        CallbackBinding::Listening { port, .. } => assert_eq!(port, primary),
        CallbackBinding::ManualFallback { .. } => panic!("primary should be reclaimed"),
    }

    Ok(())
}

#[tokio::test]
async fn fixed_callback_falls_back_to_manual_when_both_registered_ports_are_busy()
-> anyhow::Result<()> {
    let primary = TcpListener::bind(("127.0.0.1", 0)).await?;
    let fallback = TcpListener::bind(("127.0.0.1", 0)).await?;
    let primary_port = primary.local_addr()?.port();
    let fallback_port = fallback.local_addr()?.port();
    let keep_primary_busy = tokio::spawn(async move {
        let (mut stream, _) = primary.accept().await?;
        let mut request = [0_u8; 128];
        let _ = stream.read(&mut request).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok::<_, io::Error>(())
    });

    let binding = bind_callback_listener(fixed_policy(primary_port, fallback_port)).await?;
    match binding {
        CallbackBinding::ManualFallback { reason } => {
            assert_eq!(reason, "callback_ports_unavailable")
        }
        CallbackBinding::Listening { .. } => panic!("occupied ports must use manual mode"),
    }
    keep_primary_busy.abort();
    Ok(())
}

#[tokio::test]
async fn newest_session_replaces_an_active_listener_without_overwriting_the_reason()
-> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(stravia_core::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let manager = OAuthCallbackManager::new(gateway.clone());

    let first = manager
        .init_session("claude-code", false, OAuthCallbackMode::Auto, None)
        .await?;
    let second = manager
        .init_session("codex", false, OAuthCallbackMode::Manual, None)
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
        stravia_core::auth::AuthSessionStatusData::Error { ref code, .. }
            if code == "AUTH_SESSION_REPLACED"
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
    let (gateway, _logs) = Gateway::new(stravia_core::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let init = gateway
        .admin()
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

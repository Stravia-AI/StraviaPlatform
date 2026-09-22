use super::*;

pub(super) enum CallbackBinding {
    Listening { listener: TcpListener, port: u16 },
    ManualFallback { reason: String },
}

pub(super) async fn bind_callback_listener(
    policy: AuthCallback,
) -> anyhow::Result<CallbackBinding> {
    match policy.port {
        AuthCallbackPort::Dynamic => {
            let listener = TcpListener::bind((policy.bind_host.as_str(), 0))
                .await
                .with_context(|| format!("bind OAuth callback listener on {}", policy.bind_host))?;
            let port = listener.local_addr()?.port();
            Ok(CallbackBinding::Listening { listener, port })
        }
        AuthCallbackPort::Fixed { primary, fallback } => {
            for (index, port) in std::iter::once(primary).chain(fallback).enumerate() {
                let mut bound = TcpListener::bind((policy.bind_host.as_str(), port)).await;
                if index == 0
                    && bound
                        .as_ref()
                        .is_err_and(|error| error.kind() == io::ErrorKind::AddrInUse)
                    && let Some(path) = policy.cancel_path.as_deref()
                {
                    match send_cancel_request(&policy.bind_host, port, path).await {
                        Ok(()) => {
                            // 等待旧监听器完成取消响应并关闭连接后，再尝试一次；不轮询抢占端口。
                            bound = TcpListener::bind((policy.bind_host.as_str(), port)).await;
                        }
                        Err(error) => {
                            tracing::debug!(%error, "failed to cancel the occupied OAuth callback listener")
                        }
                    }
                }
                match bound {
                    Ok(listener) => return Ok(CallbackBinding::Listening { listener, port }),
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "bind OAuth callback listener on {}:{port}",
                                policy.bind_host
                            )
                        });
                    }
                }
            }
            Ok(CallbackBinding::ManualFallback {
                reason: "callback_ports_unavailable".to_string(),
            })
        }
    }
}

pub(super) async fn send_cancel_request(host: &str, port: u16, path: &str) -> io::Result<()> {
    let mut stream = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "cancel connection timed out"))??;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    tokio::time::timeout(Duration::from_secs(2), stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "cancel write timed out"))??;
    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.take(4097).read_to_end(&mut response),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "cancel response timed out"))??;
    let status = std::str::from_utf8(&response)
        .ok()
        .and_then(|text| text.lines().next())
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok());
    if response.len() > 4096 || !status.is_some_and(|status| (200..300).contains(&status)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cancel response",
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct CallbackState {
    pub(super) gateway: Gateway,
    pub(super) session_id: String,
    pub(super) redirect_uri: String,
    pub(super) locale: CallbackLocale,
    pub(super) shutdown: watch::Sender<bool>,
}

pub(super) fn serve_callback_listener(
    listener: TcpListener,
    policy: AuthCallback,
    state: CallbackState,
    mut receiver: watch::Receiver<bool>,
) {
    let gateway = state.gateway.clone();
    let session_id = state.session_id.clone();
    let mut app = Router::new().route(&policy.path, get(oauth_callback_handler));
    if let Some(cancel_path) = policy.cancel_path.as_deref() {
        app = app.route(
            cancel_path,
            get(oauth_cancel_handler).post(oauth_cancel_handler),
        );
    }
    let app = app.with_state(state);

    tokio::spawn(async move {
        let timeout_gateway = gateway.clone();
        let timeout_session_id = session_id.clone();
        let shutdown_signal = async move {
            let timeout = tokio::time::sleep(CALLBACK_TTL);
            tokio::pin!(timeout);
            let mut lifecycle = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = receiver.changed() => break,
                    _ = &mut timeout => {
                        if let Err(error) = timeout_gateway.admin().mark_oauth_session_error(
                            &timeout_session_id,
                            "AUTH_TIMEOUT",
                            "auth session expired",
                        ).await {
                            tracing::debug!(%error, "failed to mark timed-out OAuth session");
                        }
                        break;
                    }
                    _ = lifecycle.tick() => {
                        if !matches!(
                            timeout_gateway.admin().get_oauth_session_status(&timeout_session_id).await,
                            Ok(stravia_core::auth::AuthSessionStatusData::Pending { .. }
                                | stravia_core::auth::AuthSessionStatusData::Exchanging { .. })
                        ) {
                            break;
                        }
                    }
                }
            }
        };
        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal)
            .await;
        let stopped_while_active = matches!(
            gateway.admin().get_oauth_session_status(&session_id).await,
            Ok(stravia_core::auth::AuthSessionStatusData::Pending { .. }
                | stravia_core::auth::AuthSessionStatusData::Exchanging { .. })
        );
        if (serve_result.is_err() || stopped_while_active)
            && let Err(error) = gateway
                .admin()
                .mark_oauth_session_error(
                    &session_id,
                    "AUTH_LISTENER_FATAL",
                    "OAuth callback listener stopped unexpectedly",
                )
                .await
        {
            tracing::debug!(%error, "failed to mark failed OAuth session");
        }
        if let Err(error) = serve_result {
            tracing::warn!(%error, "OAuth callback listener failed");
        } else if stopped_while_active {
            tracing::warn!("OAuth callback listener stopped while its session was still active");
        }
    });
}

pub(super) async fn oauth_callback_handler(
    State(state): State<CallbackState>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let callback_url = match uri.query() {
        Some(query) => format!("{}?{query}", state.redirect_uri),
        None => state.redirect_uri.clone(),
    };
    let result = state
        .gateway
        .admin()
        .complete_oauth_session(
            &state.session_id,
            stravia_core::auth::AuthCompletionInput {
                input: stravia_core::auth::AuthCompletionValue::CallbackUrl {
                    value: callback_url,
                },
            },
        )
        .await;

    let copy = state.locale.copy();
    match result {
        Ok(_) => {
            let _ = state.shutdown.send(true);
            callback_html(
                StatusCode::OK,
                copy.lang,
                copy.complete_title,
                copy.complete_message,
            )
        }
        Err(_) => {
            let terminal = matches!(
                state
                    .gateway
                    .admin()
                    .get_oauth_session_status(&state.session_id)
                    .await,
                Ok(stravia_core::auth::AuthSessionStatusData::Error { .. })
            );
            if terminal {
                let _ = state.shutdown.send(true);
            }
            callback_html(
                StatusCode::BAD_REQUEST,
                copy.lang,
                copy.failed_title,
                copy.failed_message,
            )
        }
    }
}

async fn oauth_cancel_handler(State(state): State<CallbackState>) -> Response {
    if let Err(error) = state
        .gateway
        .admin()
        .cancel_oauth_session(&state.session_id)
        .await
    {
        tracing::debug!(%error, "failed to mark cancelled OAuth session");
    }
    let _ = state.shutdown.send(true);
    (StatusCode::OK, "Login cancelled").into_response()
}

pub(super) fn callback_html(
    status: StatusCode,
    lang: &str,
    title: &str,
    message: &str,
) -> Response {
    let body = format!(
        "<!doctype html><html lang=\"{lang}\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><body><main><h1>{title}</h1><p>{message}</p></main></body></html>"
    );
    let mut response = (status, Html(body)).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    response
        .headers_mut()
        .insert(PRAGMA, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    response
}

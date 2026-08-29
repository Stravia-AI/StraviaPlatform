use super::*;

pub(super) enum CallbackBinding {
    Listening { listener: TcpListener, port: u16 },
    ManualFallback { reason: String },
}

pub(super) async fn bind_callback_listener(
    policy: OAuthCallbackPolicy,
) -> anyhow::Result<CallbackBinding> {
    match policy.port {
        OAuthCallbackPort::Dynamic => {
            let listener = TcpListener::bind((policy.bind_host, 0))
                .await
                .with_context(|| format!("bind OAuth callback listener on {}", policy.bind_host))?;
            let port = listener.local_addr()?.port();
            Ok(CallbackBinding::Listening { listener, port })
        }
        OAuthCallbackPort::Fixed { primary, fallback } => {
            for (index, port) in [primary, fallback].into_iter().enumerate() {
                let mut cancel_attempted = false;
                for attempt in 0..CODEX_BIND_ATTEMPTS {
                    match TcpListener::bind((policy.bind_host, port)).await {
                        Ok(listener) => {
                            return Ok(CallbackBinding::Listening { listener, port });
                        }
                        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                            if index == 0 && !cancel_attempted {
                                cancel_attempted = true;
                                let _ = send_cancel_request(policy.bind_host, port).await;
                            }
                            if attempt + 1 < CODEX_BIND_ATTEMPTS {
                                tokio::time::sleep(CODEX_BIND_RETRY_DELAY).await;
                            }
                        }
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
            }
            Ok(CallbackBinding::ManualFallback {
                reason: "callback_ports_unavailable".to_string(),
            })
        }
    }
}

pub(super) async fn send_cancel_request(host: &str, port: u16) -> io::Result<()> {
    let address = format!("{host}:{port}");
    let mut stream = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(&address))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "cancel connection timed out"))??;
    let request =
        format!("GET /cancel HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    tokio::time::timeout(Duration::from_secs(2), stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "cancel write timed out"))??;
    let mut response = [0_u8; 64];
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response)).await;
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
    policy: OAuthCallbackPolicy,
    state: CallbackState,
    mut receiver: watch::Receiver<bool>,
) {
    let gateway = state.gateway.clone();
    let session_id = state.session_id.clone();
    let mut app = Router::new().route(policy.path, get(oauth_callback_handler));
    if let Some(cancel_path) = policy.cancel_path {
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
            tokio::select! {
                _ = receiver.changed() => {}
                _ = tokio::time::sleep(CALLBACK_TTL) => {
                    let _ = timeout_gateway.admin().mark_oauth_session_error(
                        &timeout_session_id,
                        "AUTH_TIMEOUT",
                        "auth session expired",
                    ).await;
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
        if serve_result.is_err() || stopped_while_active {
            let _ = gateway
                .admin()
                .mark_oauth_session_error(
                    &session_id,
                    "AUTH_LISTENER_FATAL",
                    "OAuth callback listener stopped unexpectedly",
                )
                .await;
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
        .complete_oauth_session(&state.session_id, AuthExchangeInput { callback_url })
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
    let _ = state
        .gateway
        .admin()
        .cancel_oauth_session(&state.session_id)
        .await;
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

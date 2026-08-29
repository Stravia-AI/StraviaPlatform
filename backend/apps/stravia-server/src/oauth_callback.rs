mod locale;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue, PRAGMA};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use stravia_core::Gateway;
use stravia_core::auth::{
    AuthExchangeInput, AuthScheme, AuthSessionInitData, OAuthCallbackMode, OAuthCallbackPolicy,
    OAuthCallbackPort, OAuthSessionStartOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};

use locale::CallbackLocale;

const CALLBACK_TTL: Duration = Duration::from_secs(10 * 60);
const CODEX_BIND_ATTEMPTS: usize = 10;
const CODEX_BIND_RETRY_DELAY: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub(crate) struct OAuthCallbackManager {
    inner: Arc<OAuthCallbackManagerInner>,
}

struct OAuthCallbackManagerInner {
    gateway: Gateway,
    operation: Mutex<()>,
    active: Mutex<Option<ActiveCallback>>,
}

struct ActiveCallback {
    session_id: String,
    shutdown: Option<watch::Sender<bool>>,
}

impl Drop for OAuthCallbackManagerInner {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.try_lock()
            && let Some(active) = active.take()
            && let Some(shutdown) = active.shutdown
        {
            let _ = shutdown.send(true);
        }
    }
}

impl OAuthCallbackManager {
    pub(crate) fn new(gateway: Gateway) -> Self {
        Self {
            inner: Arc::new(OAuthCallbackManagerInner {
                gateway,
                operation: Mutex::new(()),
                active: Mutex::new(None),
            }),
        }
    }

    pub(crate) async fn init_session(
        &self,
        vendor: &str,
        use_proxy: bool,
        requested_mode: OAuthCallbackMode,
        requested_locale: Option<&str>,
    ) -> anyhow::Result<AuthSessionInitData> {
        let locale = CallbackLocale::from_requested(requested_locale);
        let _operation = self.inner.operation.lock().await;
        self.replace_active_session().await;
        if driver_metadata(vendor)?.scheme == AuthScheme::OAuthDeviceCode {
            let init = self
                .inner
                .gateway
                .admin()
                .init_oauth_session(
                    vendor,
                    use_proxy,
                    OAuthSessionStartOptions {
                        callback_mode: OAuthCallbackMode::Auto,
                        redirect_uri: String::new(),
                        listener_port: None,
                        fallback_reason: None,
                    },
                )
                .await?;
            *self.inner.active.lock().await = Some(ActiveCallback {
                session_id: init.session_id.clone(),
                shutdown: None,
            });
            return Ok(init);
        }
        if requested_mode == OAuthCallbackMode::Manual {
            let policy = callback_policy(vendor)?;
            let init = self
                .inner
                .gateway
                .admin()
                .init_oauth_session(
                    vendor,
                    use_proxy,
                    OAuthSessionStartOptions {
                        callback_mode: OAuthCallbackMode::Manual,
                        redirect_uri: policy.manual_redirect_uri.to_string(),
                        listener_port: None,
                        fallback_reason: None,
                    },
                )
                .await?;
            *self.inner.active.lock().await = Some(ActiveCallback {
                session_id: init.session_id.clone(),
                shutdown: None,
            });
            return Ok(init);
        }

        self.replace_active_session().await;
        let policy = callback_policy(vendor)?;
        match bind_callback_listener(policy).await? {
            CallbackBinding::ManualFallback { reason } => {
                let init = self
                    .inner
                    .gateway
                    .admin()
                    .init_oauth_session(
                        vendor,
                        use_proxy,
                        OAuthSessionStartOptions {
                            callback_mode: OAuthCallbackMode::Manual,
                            redirect_uri: policy.manual_redirect_uri.to_string(),
                            listener_port: None,
                            fallback_reason: Some(reason),
                        },
                    )
                    .await?;
                *self.inner.active.lock().await = Some(ActiveCallback {
                    session_id: init.session_id.clone(),
                    shutdown: None,
                });
                Ok(init)
            }
            CallbackBinding::Listening { listener, port } => {
                let redirect_uri =
                    format!("http://{}:{}{}", policy.redirect_host, port, policy.path);
                let init = self
                    .inner
                    .gateway
                    .admin()
                    .init_oauth_session(
                        vendor,
                        use_proxy,
                        OAuthSessionStartOptions {
                            callback_mode: OAuthCallbackMode::Auto,
                            redirect_uri: redirect_uri.clone(),
                            listener_port: Some(port),
                            fallback_reason: None,
                        },
                    )
                    .await;
                let init = match init {
                    Ok(init) => init,
                    Err(error) => {
                        drop(listener);
                        return Err(error);
                    }
                };

                let (shutdown, receiver) = watch::channel(false);
                serve_callback_listener(
                    listener,
                    policy,
                    CallbackState {
                        gateway: self.inner.gateway.clone(),
                        session_id: init.session_id.clone(),
                        redirect_uri,
                        locale,
                        shutdown: shutdown.clone(),
                    },
                    receiver,
                );
                *self.inner.active.lock().await = Some(ActiveCallback {
                    session_id: init.session_id.clone(),
                    shutdown: Some(shutdown),
                });
                Ok(init)
            }
        }
    }

    pub(crate) async fn cancel_session(&self, session_id: &str) -> anyhow::Result<()> {
        let _operation = self.inner.operation.lock().await;
        self.stop_if_matches(session_id).await;
        self.inner
            .gateway
            .admin()
            .cancel_oauth_session(session_id)
            .await
    }

    pub(crate) async fn release_if_terminal(&self, session_id: &str) {
        self.stop_if_matches(session_id).await;
    }

    async fn replace_active_session(&self) {
        let active = self.inner.active.lock().await.take();
        if let Some(active) = active {
            let _ = self
                .inner
                .gateway
                .admin()
                .mark_oauth_session_error(
                    &active.session_id,
                    "AUTH_SESSION_REPLACED",
                    "OAuth session was replaced by a newer local login",
                )
                .await;
            if let Some(shutdown) = active.shutdown {
                let _ = shutdown.send(true);
            }
        }
    }

    async fn stop_if_matches(&self, session_id: &str) {
        let mut active = self.inner.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|current| current.session_id == session_id)
            && let Some(current) = active.take()
            && let Some(shutdown) = current.shutdown
        {
            let _ = shutdown.send(true);
        }
    }
}

fn callback_policy(vendor: &str) -> anyhow::Result<OAuthCallbackPolicy> {
    driver_metadata(vendor)?
        .callback
        .ok_or_else(|| anyhow!("auth vendor does not support callback flow: {vendor}"))
}

fn driver_metadata(vendor: &str) -> anyhow::Result<stravia_core::auth::AuthDriverMetadata> {
    let driver_key = stravia_core::auth::normalize_driver_key(vendor);
    let driver = stravia_core::auth::build_driver(&driver_key)
        .ok_or_else(|| anyhow!("auth vendor not implemented: {driver_key}"))?;
    Ok(driver.metadata())
}

mod listener;

use listener::{CallbackBinding, CallbackState, bind_callback_listener, serve_callback_listener};
#[cfg(test)]
use listener::{callback_html, oauth_callback_handler};

#[cfg(test)]
mod tests;

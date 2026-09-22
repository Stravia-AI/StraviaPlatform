mod locale;

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue, PRAGMA};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use stravia_core::Gateway;
use stravia_core::auth::{
    AuthCallback, AuthCallbackPort, AuthFlow, AuthSessionCandidate, AuthSessionInitData,
    OAuthCallbackMode, OAuthSessionStartOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};

use locale::CallbackLocale;

const CALLBACK_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub(crate) struct OAuthCallbackManager {
    inner: Arc<OAuthCallbackManagerInner>,
}

struct OAuthCallbackManagerInner {
    gateway: Gateway,
    active: Mutex<HashMap<String, ActiveCallback>>,
}

struct ActiveCallback {
    shutdown: Option<watch::Sender<bool>>,
}

impl Drop for OAuthCallbackManagerInner {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.try_lock() {
            for (_, active) in active.drain() {
                if let Some(shutdown) = active.shutdown {
                    let _ = shutdown.send(true);
                }
            }
        }
    }
}

impl OAuthCallbackManager {
    pub(crate) fn new(gateway: Gateway) -> Self {
        Self {
            inner: Arc::new(OAuthCallbackManagerInner {
                gateway,
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(crate) async fn init_session(
        &self,
        candidate: AuthSessionCandidate,
        requested_mode: OAuthCallbackMode,
        requested_locale: Option<&str>,
    ) -> anyhow::Result<AuthSessionInitData> {
        let locale = CallbackLocale::from_requested(requested_locale);
        let descriptor = self
            .inner
            .gateway
            .admin()
            .vendor_metadata(&candidate.vendor_id)?;
        let channel = descriptor
            .channels
            .iter()
            .find(|channel| channel.id == candidate.channel)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Vendor `{}` does not declare channel `{}`",
                    candidate.vendor_id,
                    candidate.channel
                )
            })?;
        let auth = channel
            .auth
            .clone()
            .ok_or_else(|| anyhow::anyhow!("vendor channel does not declare authentication"))?;

        match auth.flow {
            AuthFlow::DeviceCode => {
                let init = self
                    .inner
                    .gateway
                    .admin()
                    .init_oauth_session(
                        candidate,
                        OAuthSessionStartOptions {
                            callback_mode: OAuthCallbackMode::Auto,
                            redirect_uri: String::new(),
                            listener_port: None,
                            fallback_reason: None,
                        },
                    )
                    .await?;
                self.track(&init.session_id, None).await;
                Ok(init)
            }
            AuthFlow::Manual => {
                let init = self
                    .inner
                    .gateway
                    .admin()
                    .init_oauth_session(
                        candidate,
                        OAuthSessionStartOptions {
                            callback_mode: OAuthCallbackMode::Manual,
                            redirect_uri: String::new(),
                            listener_port: None,
                            fallback_reason: None,
                        },
                    )
                    .await?;
                self.track(&init.session_id, None).await;
                Ok(init)
            }
            AuthFlow::AuthorizationCode => {
                let manual_callback_allowed = auth.manual_input.is_some();
                let callback = auth.callback.ok_or_else(|| {
                    anyhow::anyhow!("authorization-code channel has no callback policy")
                })?;
                if requested_mode == OAuthCallbackMode::Manual {
                    anyhow::ensure!(
                        manual_callback_allowed,
                        "vendor channel does not support manual callback input"
                    );
                    return self.init_manual_callback(candidate, callback, None).await;
                }
                match bind_callback_listener(callback.clone()).await? {
                    CallbackBinding::ManualFallback { reason } => {
                        anyhow::ensure!(
                            manual_callback_allowed,
                            "callback listener unavailable and vendor channel has no manual fallback"
                        );
                        self.init_manual_callback(candidate, callback, Some(reason))
                            .await
                    }
                    CallbackBinding::Listening { listener, port } => {
                        let redirect_uri = if callback.redirect_host.contains(':') {
                            format!(
                                "http://[{}]:{port}{}",
                                callback.redirect_host, callback.path
                            )
                        } else {
                            format!("http://{}:{port}{}", callback.redirect_host, callback.path)
                        };
                        let init = self
                            .inner
                            .gateway
                            .admin()
                            .init_oauth_session(
                                candidate,
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
                            callback,
                            CallbackState {
                                gateway: self.inner.gateway.clone(),
                                session_id: init.session_id.clone(),
                                redirect_uri,
                                locale,
                                shutdown: shutdown.clone(),
                            },
                            receiver,
                        );
                        self.track(&init.session_id, Some(shutdown)).await;
                        Ok(init)
                    }
                }
            }
        }
    }

    async fn init_manual_callback(
        &self,
        candidate: AuthSessionCandidate,
        callback: AuthCallback,
        fallback_reason: Option<String>,
    ) -> anyhow::Result<AuthSessionInitData> {
        let redirect_uri = callback.manual_redirect_uri.ok_or_else(|| {
            anyhow::anyhow!("vendor channel does not support manual callback input")
        })?;
        let init = self
            .inner
            .gateway
            .admin()
            .init_oauth_session(
                candidate,
                OAuthSessionStartOptions {
                    callback_mode: OAuthCallbackMode::Manual,
                    redirect_uri,
                    listener_port: None,
                    fallback_reason,
                },
            )
            .await?;
        self.track(&init.session_id, None).await;
        Ok(init)
    }

    async fn track(&self, session_id: &str, shutdown: Option<watch::Sender<bool>>) {
        self.inner
            .active
            .lock()
            .await
            .insert(session_id.to_string(), ActiveCallback { shutdown });
    }

    pub(crate) async fn cancel_session(&self, session_id: &str) -> anyhow::Result<()> {
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

    async fn stop_if_matches(&self, session_id: &str) {
        let active = self.inner.active.lock().await.remove(session_id);
        if let Some(ActiveCallback {
            shutdown: Some(shutdown),
        }) = active
        {
            let _ = shutdown.send(true);
        }
    }
}

mod listener;

use listener::{CallbackBinding, CallbackState, bind_callback_listener, serve_callback_listener};
#[cfg(test)]
use listener::{callback_html, oauth_callback_handler};

#[cfg(test)]
mod tests;

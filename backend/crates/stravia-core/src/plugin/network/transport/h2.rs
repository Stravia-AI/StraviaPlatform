//! Configurable HTTP/2 client connections over an already connected transport.

use std::time::Duration;

use rama_core::{extensions::ExtensionsRef, rt::Executor};
pub use rama_http_core::client::conn::http2::H2PeerSettingsHandle;
use rama_http_core::client::conn::http2::{Builder, SendRequest};
use rama_http_types::proto::h2::{
    PseudoHeaderOrder,
    frame::{EarlyFrame, SettingOrder, SettingsConfig},
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::body::RequestBody;

/// A cloneable, multiplexed HTTP/2 request sender.
pub type H2Sender = SendRequest<RequestBody>;

/// Settings applied to each new HTTP/2 connection.
#[derive(Clone, Debug)]
pub struct H2Settings {
    /// SETTINGS values, including an optional setting order.
    pub setting_config: SettingsConfig,
    /// Overrides the order in `setting_config` when present.
    pub setting_order: Option<SettingOrder>,
    pub pseudo_header_order: Option<PseudoHeaderOrder>,
    pub early_frames: Option<Vec<EarlyFrame>>,
    /// Overrides the initial stream window in `setting_config` when present.
    pub initial_stream_window_size: Option<u32>,
    pub initial_connection_window_size: Option<u32>,
    pub keep_alive_interval: Option<Duration>,
    pub keep_alive_timeout: Duration,
    pub keep_alive_while_idle: bool,
}

impl Default for H2Settings {
    fn default() -> Self {
        Self {
            setting_config: SettingsConfig::default(),
            setting_order: None,
            pseudo_header_order: None,
            early_frames: None,
            initial_stream_window_size: None,
            initial_connection_window_size: None,
            keep_alive_interval: Some(Duration::from_secs(15)),
            keep_alive_timeout: Duration::from_secs(10),
            keep_alive_while_idle: false,
        }
    }
}

/// Handshake and spawn the HTTP/2 connection driver on the current Tokio runtime.
pub async fn connect<T>(
    io: T,
    settings: H2Settings,
) -> Result<(H2Sender, H2PeerSettingsHandle), rama_http_core::Error>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + ExtensionsRef + 'static,
{
    let mut builder = Builder::new(Executor::default());
    builder.set_setting_config(&settings.setting_config);
    if let Some(order) = settings.setting_order {
        builder.set_setting_order(order);
    }
    if let Some(order) = settings.pseudo_header_order {
        builder.set_headers_pseudo_order(order);
    }
    if let Some(frames) = settings.early_frames {
        builder.set_early_frames(frames);
    }
    if let Some(size) = settings.initial_stream_window_size {
        builder.set_initial_stream_window_size(size);
    }
    if let Some(size) = settings.initial_connection_window_size {
        builder.set_initial_connection_window_size(size);
    }
    builder
        .set_keep_alive_interval(settings.keep_alive_interval)
        .set_keep_alive_timeout(settings.keep_alive_timeout)
        .set_keep_alive_while_idle(settings.keep_alive_while_idle);

    let (sender, connection) = builder.handshake::<_, RequestBody>(io).await?;
    let peer_settings = connection.peer_settings_handle();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "HTTP/2 connection closed with an error");
        }
    });
    Ok((sender, peer_settings))
}

/// Wait for the peer's initial SETTINGS before deciding whether to send an
/// extended CONNECT. `None` means the connection closed before SETTINGS arrived.
pub async fn peer_extended_connect_enabled(handle: &H2PeerSettingsHandle) -> Option<bool> {
    handle
        .await_settings()
        .await
        .map(|settings| settings.0.config.enable_connect_protocol == Some(1))
}

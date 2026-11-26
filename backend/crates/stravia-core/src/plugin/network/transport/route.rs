//! Apply the resolved outbound proxy policy (Direct, System, or Explicit).

use crate::outbound_proxy::{ResolvedOutboundProxy, intercept_to_url, system_proxy_matcher};

use super::{TransportError, connector};

pub(super) fn select_proxy(
    settings: &ResolvedOutboundProxy,
    destination: &url::Url,
) -> Result<connector::Route, TransportError> {
    settings
        .validate()
        .map_err(|e| TransportError::Proxy(e.to_string()))?;
    match settings {
        ResolvedOutboundProxy::Direct => Ok(connector::Route::Direct),
        ResolvedOutboundProxy::Explicit { url, .. } => connector::Proxy::from_url(url.trim())
            .map(connector::Route::Proxy)
            .map_err(|e| TransportError::Proxy(e.to_string())),
        ResolvedOutboundProxy::System(source) => {
            let mut destination = destination.clone();
            match destination.scheme() {
                "ws" => destination
                    .set_scheme("http")
                    .map_err(|_| TransportError::Protocol("invalid URL scheme".into()))?,
                "wss" => destination
                    .set_scheme("https")
                    .map_err(|_| TransportError::Protocol("invalid URL scheme".into()))?,
                "http" | "https" => {}
                _ => return Err(TransportError::Protocol("unsupported URL scheme".into())),
            }
            destination.set_fragment(None);
            let uri: http::Uri = destination
                .as_str()
                .parse()
                .map_err(|e: http::uri::InvalidUri| TransportError::Protocol(e.to_string()))?;
            let matcher = system_proxy_matcher(*source);
            let Some(intercept) = matcher.intercept(&uri) else {
                return Ok(connector::Route::Direct);
            };
            let proxy_url = intercept_to_url(&intercept)
                .ok_or_else(|| TransportError::Proxy("system proxy URL is invalid".into()))?;
            connector::Proxy::from_url(proxy_url.as_str())
                .map(connector::Route::Proxy)
                .map_err(|e| TransportError::Proxy(e.to_string()))
        }
    }
}

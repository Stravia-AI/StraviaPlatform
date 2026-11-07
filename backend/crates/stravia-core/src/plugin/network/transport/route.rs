//! Apply the same explicit bypass rules as the existing outbound proxy policy.

use crate::outbound_proxy::OutboundProxySettings;
use hyper_util::client::proxy::matcher::Matcher;

use super::{TransportError, connector};

pub(super) fn select_proxy(
    settings: &OutboundProxySettings,
    destination: &url::Url,
) -> Result<connector::Route, TransportError> {
    settings
        .validate()
        .map_err(|e| TransportError::Proxy(e.to_string()))?;
    if settings.url.trim().is_empty() {
        return Ok(connector::Route::Direct);
    }
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
    let bypass = settings
        .bypass
        .split(|c: char| c == ',' || c == ';' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(",")
        .to_ascii_lowercase();
    // This matcher only evaluates the explicitly supplied rules. Unlike the
    // environment constructors, the builder does not inspect system proxies.
    let matcher = Matcher::builder()
        .all(settings.url.trim())
        .no(bypass)
        .build();
    // Distinguish a bypass match from an unparseable proxy: silently routing
    // direct on a proxy parsing failure would leak the request.
    let baseline = Matcher::builder().all(settings.url.trim()).build();
    if baseline.intercept(&uri).is_none() {
        return Err(TransportError::Proxy("proxy URL cannot be matched".into()));
    }
    if matcher.intercept(&uri).is_none() {
        Ok(connector::Route::Direct)
    } else {
        connector::Proxy::from_url(settings.url.trim())
            .map(connector::Route::Proxy)
            .map_err(|e| TransportError::Proxy(e.to_string()))
    }
}

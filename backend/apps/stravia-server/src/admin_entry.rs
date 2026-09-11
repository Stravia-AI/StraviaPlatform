use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;

use crate::http_auth::auth_error;

/// Immutable, validated deployment policy for the entire Server management surface.
#[derive(Clone, Default)]
pub struct AdminEntryPolicy(Arc<Policy>);

#[derive(Default)]
struct Policy {
    origins: Vec<String>,
    proxies: Vec<IpNetwork>,
}

#[derive(Clone)]
pub(crate) struct RequestOrigin(String);

impl RequestOrigin {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn secure(&self) -> bool {
        self.0.starts_with("https://")
    }
}

impl AdminEntryPolicy {
    pub fn new(origins: &[String], proxies: &[String]) -> anyhow::Result<Self> {
        let origins = origins
            .iter()
            .map(|value| canonical_origin(value.trim()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let proxies = proxies
            .iter()
            .map(|value| IpNetwork::parse(value.trim()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self(Arc::new(Policy { origins, proxies })))
    }

    pub fn allows_http(&self) -> bool {
        self.0.origins.is_empty()
            || self
                .0
                .origins
                .iter()
                .any(|origin| origin.starts_with("http://"))
    }

    pub fn unrestricted(&self) -> bool {
        self.0.origins.is_empty()
    }

    pub(crate) fn protect(&self, app: Router) -> Router {
        app.layer(middleware::from_fn_with_state(self.clone(), require_entry))
    }

    fn recover(&self, request: &Request) -> anyhow::Result<RequestOrigin> {
        let trusted = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| {
                self.0
                    .proxies
                    .iter()
                    .any(|network| network.contains(peer.0.ip()))
            });
        let headers = request.headers();
        let (scheme, authority) = if trusted {
            if headers.contains_key("forwarded") {
                bail!("Forwarded is not supported");
            }
            let proto = single_header(headers, "x-forwarded-proto")?;
            let host = single_header(headers, "x-forwarded-host")?;
            match (proto, host) {
                (Some(proto @ ("http" | "https")), Some(host)) => (proto, host),
                (None, None) => (
                    "http",
                    single_header(headers, "host")?.context("Host required")?,
                ),
                _ => bail!("forwarding metadata must contain one protocol and authority pair"),
            }
        } else {
            (
                "http",
                single_header(headers, "host")?.context("Host required")?,
            )
        };
        if authority.contains(['/', '?', '#', '@']) {
            bail!("request authority must contain only host and optional port");
        }
        let origin = canonical_origin(&format!("{scheme}://{authority}"))?;
        // An absolute-form request target cannot contradict the transport Host.
        if let Some(uri_authority) = request.uri().authority() {
            let uri_scheme = request.uri().scheme_str().unwrap_or("http");
            let direct_host = single_header(headers, "host")?.context("Host required")?;
            if canonical_origin(&format!("{uri_scheme}://{uri_authority}"))?
                != canonical_origin(&format!("http://{direct_host}"))?
            {
                bail!("request target conflicts with Host");
            }
        }
        if !self.0.origins.is_empty() && !self.0.origins.contains(&origin) {
            bail!("management entry is not allowed");
        }
        Ok(RequestOrigin(origin))
    }
}

async fn require_entry(
    State(policy): State<AdminEntryPolicy>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    // These namespaces belong to independent protocol authentication, not management.
    if matches!(path, "/healthz" | "/readyz" | "/v1" | "/v1beta" | "/mcp")
        || path.starts_with("/v1/")
        || path.starts_with("/v1beta/")
        || path.starts_with("/mcp/")
    {
        return next.run(request).await;
    }
    match policy.recover(&request) {
        Ok(origin) => {
            request.extensions_mut().insert(origin);
            next.run(request).await
        }
        Err(_) => auth_error(StatusCode::FORBIDDEN, "admin_entry_forbidden"),
    }
}

pub(crate) fn canonical_origin(value: &str) -> anyhow::Result<String> {
    // URL parsers repair backslashes, whitespace and empty ports; configuration and
    // request authorities must instead reject ambiguous input before parsing.
    if value.is_empty()
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.contains(['\\', ',', '%', '*'])
    {
        bail!("management origin must be an HTTP(S) scheme, host and optional port");
    }
    let (_, authority) = value
        .split_once("://")
        .context("management origin requires a scheme")?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) || authority.ends_with(':')
    {
        bail!("management origin must contain no credentials, path, query or fragment");
    }
    let url = url::Url::parse(value).context("invalid management origin")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("invalid management origin");
    }
    Ok(url.origin().ascii_serialization())
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> anyhow::Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let value = value.to_str().context("invalid origin metadata")?;
    if values.next().is_some() || value.is_empty() || value.contains(',') {
        bail!("ambiguous origin metadata");
    }
    Ok(Some(value))
}

struct IpNetwork {
    address: IpAddr,
    prefix: u8,
}

impl IpNetwork {
    fn parse(value: &str) -> anyhow::Result<Self> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (
                address,
                Some(
                    prefix
                        .parse::<u8>()
                        .context("invalid trusted proxy prefix")?,
                ),
            ),
            None => (value, None),
        };
        let address: IpAddr = address
            .parse()
            .context("trusted proxy must be an IP address or CIDR")?;
        let bits = if address.is_ipv4() { 32 } else { 128 };
        let mut prefix = prefix.unwrap_or(bits);
        if prefix > bits {
            bail!("trusted proxy prefix exceeds address width");
        }
        let address = if let IpAddr::V6(ip) = address {
            if let Some(ip) = ip.to_ipv4_mapped() {
                if prefix < 96 {
                    bail!("mapped IPv4 proxy prefix must be at least 96");
                }
                prefix -= 96;
                IpAddr::V4(ip)
            } else {
                address
            }
        } else {
            address
        };
        Ok(Self { address, prefix })
    }

    fn contains(&self, peer: IpAddr) -> bool {
        let peer = match peer {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(peer),
            _ => peer,
        };
        match (self.address, peer) {
            (IpAddr::V4(network), IpAddr::V4(peer)) => {
                let mask = u32::MAX
                    .checked_shl(u32::from(32 - self.prefix))
                    .unwrap_or(0);
                u32::from(network) & mask == u32::from(peer) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(peer)) => {
                let mask = u128::MAX
                    .checked_shl(u32::from(128 - self.prefix))
                    .unwrap_or(0);
                u128::from(network) & mask == u128::from(peer) & mask
            }
            _ => false,
        }
    }
}

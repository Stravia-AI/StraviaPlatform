use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use http_body_util::BodyExt;
use url::{Host, Url};
use wreq_util::Emulation;

use super::{
    BackendFuture, FetchError, FetchErrorCode, HttpBackend, HttpResponse, DOWNLOAD_BYTE_CAP,
};
use crate::outbound::{LocalWeb, ResolvedProxy};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct NetworkBackend {
    snapshot: ResolvedProxy,
    proxied: wreq::Client,
}

impl NetworkBackend {
    pub(super) fn from_local_web(web: &LocalWeb) -> Self {
        Self {
            snapshot: web.snapshot().clone(),
            proxied: web.fetch_proxied_client(),
        }
    }
}

impl HttpBackend for NetworkBackend {
    fn pins_origin(&self, url: &Url) -> bool {
        self.snapshot.pins_origin(url)
    }

    fn resolve<'a>(&'a self, url: &'a Url) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
        Box::pin(async move {
            match url.host() {
                Some(Host::Ipv4(address)) => return Ok(vec![IpAddr::V4(address)]),
                Some(Host::Ipv6(address)) => return Ok(vec![IpAddr::V6(address)]),
                _ => {}
            }
            let hostname = url
                .host_str()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?;
            let port = url.port_or_known_default().unwrap_or(0);
            let addresses = tokio::net::lookup_host((hostname, port))
                .await
                .map_err(|error| {
                    FetchError::unavailable(format!(
                        "URL hostname could not be resolved: {hostname}: {error}"
                    ))
                })?
                .map(|address| address.ip())
                .collect::<Vec<_>>();
            Ok(addresses)
        })
    }

    fn get<'a>(
        &'a self,
        url: &'a Url,
        addresses: &'a [IpAddr],
    ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
        Box::pin(async move {
            let hostname = url
                .host_str()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?
                .to_owned();
            let client = if self.pins_origin(url) {
                let port = url.port_or_known_default().unwrap_or(80);
                let socket_addresses = addresses
                    .iter()
                    .copied()
                    .map(|address| SocketAddr::new(address, port))
                    .collect::<Vec<_>>();
                wreq::Client::builder()
                    .emulation(Emulation::Firefox139)
                    .timeout(HTTP_TIMEOUT)
                    .redirect(wreq::redirect::Policy::none())
                    .no_proxy()
                    .resolve_to_addrs(hostname, socket_addresses)
                    .build()
                    .map_err(|error| {
                        FetchError::unavailable(format!("HTTP client failed: {error}"))
                    })?
            } else {
                self.proxied.clone()
            };
            send_get(client, url).await
        })
    }
}

async fn send_get(client: wreq::Client, url: &Url) -> Result<HttpResponse, FetchError> {
    let mut response = client
        .get(url.as_str())
        .send()
        .await
        .map_err(|error| FetchError::unavailable(format!("HTTP request failed: {error}")))?;
    if response
        .content_length()
        .is_some_and(|length| length > DOWNLOAD_BYTE_CAP as u64)
    {
        return Err(response_too_large());
    }
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(wreq::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let location = response
        .headers()
        .get(wreq::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut body = Vec::new();
    while let Some(frame) = response.frame().await {
        let frame = frame
            .map_err(|error| FetchError::unavailable(format!("response read failed: {error}")))?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if body.len() + chunk.len() > DOWNLOAD_BYTE_CAP {
            return Err(response_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(HttpResponse {
        status,
        content_type,
        location,
        body,
    })
}

fn response_too_large() -> FetchError {
    FetchError::new(
        FetchErrorCode::ResponseTooLarge,
        format!("response exceeds the {DOWNLOAD_BYTE_CAP}-byte download cap"),
    )
}

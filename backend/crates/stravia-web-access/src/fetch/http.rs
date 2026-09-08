use std::{net::IpAddr, time::Duration};

use url::{Host, Url};
use wreq::{Method, Request};

use super::{
    BackendFuture, FetchError, FetchErrorCode, HttpBackend, HttpResponse, DOWNLOAD_BYTE_CAP,
};
use crate::http_client::{HttpClient, ResponseTooLarge};
use crate::outbound::{LocalWeb, ResolvedProxy};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct NetworkBackend {
    snapshot: ResolvedProxy,
    proxied: HttpClient,
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
                // 固定已验证的全部地址，避免验证与连接之间再次 DNS 解析。
                HttpClient::pinned(
                    &hostname,
                    addresses
                        .iter()
                        .map(|address| std::net::SocketAddr::new(*address, port))
                        .collect(),
                    HTTP_TIMEOUT,
                    DOWNLOAD_BYTE_CAP,
                )
                .map_err(|error| FetchError::unavailable(format!("HTTP client failed: {error}")))?
            } else {
                self.proxied.clone()
            };
            send_get(client, url).await
        })
    }
}

async fn send_get(client: HttpClient, url: &Url) -> Result<HttpResponse, FetchError> {
    let request = Request::new(
        Method::GET,
        url.as_str()
            .parse()
            .map_err(|_| FetchError::invalid_url(url.as_str()))?,
    );
    let (response, body) = client.fetch_once(request).await.map_err(|error| {
        if error.downcast_ref::<ResponseTooLarge>().is_some() {
            response_too_large()
        } else {
            FetchError::unavailable(format!("HTTP request failed: {error}"))
        }
    })?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let content_type = header("content-type");
    let location = header("location");
    Ok(HttpResponse {
        status: response.status().as_u16(),
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

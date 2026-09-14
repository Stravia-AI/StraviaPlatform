//! 统一读取路径的资源取得入口：把原 media 下载器并入 fetch 的 `HttpBackend` 网络缝。
//!
//! 公网 URL 与 Artifact 字节下载共用此处实现：DNS 先解析并核验全部公网地址，
//! 连接固定到已验证地址并禁用代理，实际连接地址再次核验；重定向复用父级
//! 逐跳复验算法，预算收紧为 5 跳。HTML 探测可在响应头结束（不读正文），
//! 其余资源一次请求取得完整正文，不做先探测再抓取，并按内容类型施加
//! 网页 10MiB、其他资源 100MiB 的字节上限。完整 Content-Type 与最终 URL
//! 原样保留，源 URL 的查询串不重排、不重编码。

use std::{net::IpAddr, time::Duration};

use futures::StreamExt;
use url::{Host, Url};

use super::{
    get_with_redirects, policy, BackendFuture, FetchError, FetchErrorCode, HttpBackend,
    HttpResponse, DOWNLOAD_BYTE_CAP,
};

/// 单请求（含正文流式读取）总超时，沿用原 media 下载器语义。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// 资源下载的重定向预算，低于普通网页抓取的 10 跳。
const RESOURCE_REDIRECT_LIMIT: usize = 5;
/// 非网页资源的字节上限；网页正文仍使用父级 [`DOWNLOAD_BYTE_CAP`]。
pub(crate) const RESOURCE_DOWNLOAD_BYTE_CAP: usize = 100 * 1024 * 1024;

/// 统一读取的资源取得结果。
///
/// `body` 为 `None` 表示 HTML 探测在响应头结束；`Some`（包括空文本）表示
/// 已一次请求取得完整正文。`content_type` 是完整 Content-Type 头，
/// `final_url` 是重定向链结束后的最终 URL。
#[derive(Debug)]
pub struct ReadResource {
    pub content_type: String,
    pub final_url: String,
    pub body: Option<Vec<u8>>,
}

/// 取得一个公网 HTTP(S) 资源。
///
/// `stop_at_html` 为真时，HTML 表征在响应头处结束探测并返回 `body: None`；
/// 为假（raw 正文、显式下载、媒体上传）时一次请求直接取得完整正文。
pub async fn fetch_read_resource(
    value: &str,
    stop_at_html: bool,
) -> Result<ReadResource, FetchError> {
    let backend = DirectBackend { stop_at_html };
    fetch_read_resource_with(value, stop_at_html, &backend).await
}

async fn fetch_read_resource_with(
    value: &str,
    stop_at_html: bool,
    backend: &impl HttpBackend,
) -> Result<ReadResource, FetchError> {
    let requested_url = policy::validate_url(value)?;
    let (final_url, response) =
        get_with_redirects(requested_url, backend, RESOURCE_REDIRECT_LIMIT).await?;
    if !(200..300).contains(&response.status) {
        return Err(FetchError::unavailable(format!(
            "HTTP request returned status {}",
            response.status
        )));
    }
    let content_type = response
        .content_type
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "application/octet-stream".into());
    let body = if stop_at_html && is_html_content(&content_type) {
        None
    } else {
        Some(response.body)
    };
    Ok(ReadResource {
        content_type,
        final_url: final_url.into(),
        body,
    })
}

/// 直连资源后端：无代理、固定已验证地址并核验实际连接地址。
struct DirectBackend {
    stop_at_html: bool,
}

impl HttpBackend for DirectBackend {
    fn resolve<'a>(&'a self, url: &'a Url) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
        Box::pin(async move {
            match url.host() {
                Some(Host::Ipv4(address)) => return Ok(vec![IpAddr::V4(address)]),
                Some(Host::Ipv6(address)) => return Ok(vec![IpAddr::V6(address)]),
                _ => {}
            }
            let host = url
                .host_str()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?;
            let port = url
                .port_or_known_default()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?;
            let addresses = tokio::net::lookup_host((host, port))
                .await
                .map_err(|error| {
                    FetchError::unavailable(format!(
                        "URL hostname could not be resolved: {host}: {error}"
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
            let host = url
                .host_str()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?
                .to_owned();
            let port = url
                .port_or_known_default()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?;
            let pinned = addresses
                .iter()
                .map(|address| std::net::SocketAddr::new(*address, port))
                .collect::<Vec<_>>();
            // 固定已验证的全部地址，禁用代理与自动重定向；
            // 重定向跳转交回父级算法逐跳复验。
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(DOWNLOAD_TIMEOUT)
                .resolve_to_addrs(&host, &pinned)
                .build()
                .map_err(|error| {
                    FetchError::unavailable(format!("resource client failed: {error}"))
                })?;
            let response = client.get(url.as_str()).send().await.map_err(|error| {
                FetchError::unavailable(format!("resource request failed: {error}"))
            })?;
            let connected = response.remote_addr().ok_or_else(|| {
                FetchError::unavailable("resource connection omitted its remote address")
            })?;
            if !crate::address_policy::is_public_ip(connected.ip())
                || !addresses.iter().any(|address| *address == connected.ip())
            {
                return Err(FetchError::invalid_url(url.as_str()));
            }
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let content_type = header("content-type");
            let location = header("location");
            let status = response.status().as_u16();
            let html = content_type.as_deref().is_some_and(is_html_content);
            // 重定向跳转与失败状态在响应头结束；HTML 探测同样不读正文。
            if !(200..300).contains(&status) || (self.stop_at_html && html) {
                return Ok(HttpResponse {
                    status,
                    content_type,
                    location,
                    body: Vec::new(),
                });
            }
            let cap = body_cap(html);
            if let Some(size) = response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
            {
                if size > cap {
                    return Err(response_too_large(cap));
                }
            }
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| {
                    FetchError::unavailable(format!("resource body failed: {error}"))
                })?;
                if body.len().saturating_add(chunk.len()) > cap {
                    return Err(response_too_large(cap));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(HttpResponse {
                status,
                content_type,
                location,
                body,
            })
        })
    }
}

fn is_html_content(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or_default().trim();
    mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
}

fn body_cap(html: bool) -> usize {
    if html {
        DOWNLOAD_BYTE_CAP
    } else {
        RESOURCE_DOWNLOAD_BYTE_CAP
    }
}

fn response_too_large(cap: usize) -> FetchError {
    FetchError::new(
        FetchErrorCode::ResponseTooLarge,
        format!("resource exceeds the {cap}-byte safety cap"),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::atomic::Ordering,
    };

    use crate::fetch::tests::StubBackend;

    use super::*;

    fn redirect(location: &str) -> HttpResponse {
        HttpResponse {
            status: 302,
            content_type: None,
            location: Some(location.into()),
            body: Vec::new(),
        }
    }

    fn set_responses(backend: &StubBackend, responses: impl IntoIterator<Item = HttpResponse>) {
        *backend.responses.lock().expect("stub response lock") = responses.into_iter().collect();
    }

    fn requested(backend: &StubBackend) -> Vec<String> {
        backend
            .requested_urls
            .lock()
            .expect("stub requested url lock")
            .clone()
    }

    fn request_count(backend: &StubBackend) -> usize {
        backend.requests.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn rejects_non_public_urls_before_network_io() {
        for url in [
            "file:///etc/passwd",
            "http://user:password@example.com/",
            "http://localhost/",
            "http://service.local/",
            "http://127.0.0.1/",
            "http://192.168.1.1/",
            "http://127.0.0.1../",
            "http://[::1]/",
            "http://[2002:a00:100::1]/",
        ] {
            let backend = StubBackend::response("text/plain", "unused");
            let error = fetch_read_resource_with(url, false, &backend)
                .await
                .unwrap_err();
            assert_eq!(error.code(), FetchErrorCode::InvalidUrl, "{url}");
            assert_eq!(request_count(&backend), 0, "{url}");
        }
    }

    #[tokio::test]
    async fn rejects_private_dns_answers_before_http() {
        struct PrivateDns;
        impl HttpBackend for PrivateDns {
            fn resolve<'a>(
                &'a self,
                _url: &'a Url,
            ) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
                Box::pin(async { Ok(vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]) })
            }

            fn get<'a>(
                &'a self,
                _url: &'a Url,
                _addresses: &'a [IpAddr],
            ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
                Box::pin(async { unreachable!("private DNS answers must stop before HTTP") })
            }
        }
        let error = fetch_read_resource_with("https://example.com/asset", false, &PrivateDns)
            .await
            .unwrap_err();
        assert_eq!(error.code(), FetchErrorCode::InvalidUrl);
    }

    #[tokio::test]
    async fn rejects_redirect_into_private_network_before_second_request() {
        let backend = StubBackend::default();
        set_responses(&backend, [redirect("http://192.168.1.1/asset")]);
        let error = fetch_read_resource_with("https://example.com/asset", false, &backend)
            .await
            .unwrap_err();
        assert_eq!(error.code(), FetchErrorCode::InvalidUrl);
        assert_eq!(request_count(&backend), 1);
    }

    #[tokio::test]
    async fn html_probe_ends_at_headers_without_a_body() {
        let backend = StubBackend::response("text/html; charset=utf-8", "ignored body");
        let resource =
            fetch_read_resource_with("https://example.com/page?utm=source", true, &backend)
                .await
                .expect("html probe succeeds");
        assert_eq!(resource.body, None);
        assert_eq!(resource.content_type, "text/html; charset=utf-8");
        assert_eq!(resource.final_url, "https://example.com/page?utm=source");
        assert_eq!(request_count(&backend), 1);
    }

    #[tokio::test]
    async fn full_download_preserves_signed_query_content_type_and_final_url() {
        let backend = StubBackend::default();
        set_responses(
            &backend,
            [
                redirect("https://cdn.example.net/file.png?sig=a%2Bb&question=platform"),
                HttpResponse {
                    status: 200,
                    content_type: Some("image/png; name=\"asset.png\"".into()),
                    location: None,
                    body: vec![1, 2, 3],
                },
            ],
        );
        let stravia_web_access_contract::read_path::ReadTarget::Resource(path) =
            stravia_web_access_contract::read_path::parse_read_path(
                "https://example.com/file.png?sig=a%2Bb&question=platform#stravia?question=private%20question"
            ).expect("resource path") else { panic!("resource target") };
        let resource = fetch_read_resource_with(&path.url, false, &backend)
            .await
            .expect("resource download succeeds");
        assert_eq!(resource.body, Some(vec![1, 2, 3]));
        assert_eq!(resource.content_type, "image/png; name=\"asset.png\"");
        assert_eq!(
            resource.final_url,
            "https://cdn.example.net/file.png?sig=a%2Bb&question=platform"
        );
        // 源 URL 的签名查询串按原始顺序原样抵达每一跳，不被重排或重编码。
        assert_eq!(
            requested(&backend),
            [
                "https://example.com/file.png?sig=a%2Bb&question=platform",
                "https://cdn.example.net/file.png?sig=a%2Bb&question=platform",
            ]
        );
    }

    #[tokio::test]
    async fn empty_text_resource_is_readable() {
        let backend = StubBackend::response("text/plain; charset=utf-8", Vec::<u8>::new());
        let resource = fetch_read_resource_with("https://example.com/empty.txt", false, &backend)
            .await
            .expect("empty text succeeds");
        assert_eq!(resource.body, Some(Vec::new()));
        assert_eq!(request_count(&backend), 1);
    }

    #[tokio::test]
    async fn redirect_budget_is_five_hops() {
        let five_hops = StubBackend::default();
        set_responses(
            &five_hops,
            (0..5)
                .map(|index| redirect(&format!("https://example.com/hop{index}")))
                .chain([HttpResponse {
                    status: 200,
                    content_type: Some("text/plain".into()),
                    location: None,
                    body: b"final".to_vec(),
                }]),
        );
        let resource = fetch_read_resource_with("https://example.com/start", false, &five_hops)
            .await
            .expect("five redirects stay within the resource budget");
        assert_eq!(resource.body, Some(b"final".to_vec()));
        assert_eq!(resource.final_url, "https://example.com/hop4");
        assert_eq!(request_count(&five_hops), 6);

        let exhausted = StubBackend::default();
        set_responses(
            &exhausted,
            (0..6).map(|index| redirect(&format!("https://example.com/hop{index}"))),
        );
        let error = fetch_read_resource_with("https://example.com/start", false, &exhausted)
            .await
            .unwrap_err();
        assert_eq!(error.code(), FetchErrorCode::Unavailable);
        assert_eq!(request_count(&exhausted), 6);
    }

    #[tokio::test]
    async fn rejects_http_error_status() {
        let backend = StubBackend::default();
        set_responses(
            &backend,
            [HttpResponse {
                status: 404,
                content_type: Some("text/plain".into()),
                location: None,
                body: Vec::new(),
            }],
        );
        let error = fetch_read_resource_with("https://example.com/missing", false, &backend)
            .await
            .unwrap_err();
        assert_eq!(error.code(), FetchErrorCode::Unavailable);
    }
}

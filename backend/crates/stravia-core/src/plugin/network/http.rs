//! 显式 HTTP 内容编码由宿主独占；旧入口保留手写编码头与原始正文语义。

use std::pin::Pin;

use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
use bytes::Bytes;
use http_body_util::BodyExt as _;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, HeaderMap, HeaderValue};
use reqwest::{Method, Response, Url};
use stravia_vendor_runtime::HostFailure;
use stravia_vendor_sdk::{HttpCompression, HttpContentCoding};
use tokio::io::AsyncWriteExt as _;
use tower::ServiceExt as _;
use tower_http::decompression::{Decompression, DecompressionBody};

use super::{invalid, transport_failure};

/// 只把唯一的 chunked 声明提升为宿主 framing 策略，不允许插件提交任意逐跳编码。
pub(super) fn take_chunked_header(
    headers: &mut Vec<(String, String)>,
) -> Result<bool, HostFailure> {
    let mut selected = None;
    for (index, (name, value)) in headers.iter().enumerate() {
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if selected.is_some()
                || !value
                    .trim_matches([' ', '\t'])
                    .eq_ignore_ascii_case("chunked")
            {
                return Err(invalid("HTTP Transfer-Encoding requires one chunked value"));
            }
            selected = Some(index);
        }
    }
    if let Some(index) = selected {
        headers.remove(index);
        Ok(true)
    } else {
        Ok(false)
    }
}

/// 必须在发送前调用；手写编码头会破坏宿主的唯一所有权，不能悄悄覆盖。
pub(super) fn prepare(
    headers: &mut HeaderMap,
    compression: &HttpCompression,
) -> Result<(), HostFailure> {
    if [ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH]
        .iter()
        .any(|name| headers.contains_key(name))
        || headers.contains_key("sec-websocket-extensions")
    {
        return Err(invalid(
            "HTTP compression headers are controlled by the host transport",
        ));
    }
    // identity 始终可接受；空列表不能省略此头，否则服务端可能自行选择编码。
    let accepted = if compression.response.is_empty() {
        HeaderValue::from_static("identity")
    } else {
        let mut accepted = String::new();
        let mut seen = 0_u8;
        for &coding in &compression.response {
            let bit = match coding {
                HttpContentCoding::Gzip => 1,
                HttpContentCoding::Deflate => 2,
                HttpContentCoding::Br => 4,
                HttpContentCoding::Zstd => 8,
            };
            if seen & bit != 0 {
                continue;
            }
            seen |= bit;
            if !accepted.is_empty() {
                accepted.push_str(", ");
            }
            accepted.push_str(coding_name(coding));
        }
        HeaderValue::from_maybe_shared(Bytes::from(accepted))
            .expect("content coding names are valid header values")
    };
    headers.insert(ACCEPT_ENCODING, accepted);
    if let Some(coding) = compression.request {
        headers.insert(
            CONTENT_ENCODING,
            HeaderValue::from_static(coding_name(coding)),
        );
    }
    Ok(())
}

fn coding_name(coding: HttpContentCoding) -> &'static str {
    match coding {
        HttpContentCoding::Gzip => "gzip",
        HttpContentCoding::Deflate => "deflate",
        HttpContentCoding::Br => "br",
        HttpContentCoding::Zstd => "zstd",
    }
}

fn response_failure(message: &'static str) -> HostFailure {
    transport_failure(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// 返回实际 wire 正文；调用方在取消 select 内 await，随后只克隆 Bytes 用于重定向。
/// 未编码时转移 Vec 的所有权，不分配、复制或遍历正文。
pub(super) async fn encode(
    body: Vec<u8>,
    coding: Option<HttpContentCoding>,
) -> Result<Bytes, HostFailure> {
    let Some(coding) = coding else {
        return Ok(Bytes::from(body));
    };
    // 内存源永远 ready；限制每轮输入并显式让出，避免大正文压缩垄断执行器，
    // 同时使父任务的取消分支能在编码完成之前获得轮询机会。
    macro_rules! encode_with {
        ($encoder:ident) => {{
            let mut encoder = $encoder::new(Vec::new());
            for chunk in body.chunks(16 * 1024) {
                encoder.write_all(chunk).await.map_err(transport_failure)?;
                tokio::task::yield_now().await;
            }
            encoder.shutdown().await.map_err(transport_failure)?;
            Bytes::from(encoder.into_inner())
        }};
    }
    Ok(match coding {
        HttpContentCoding::Gzip => encode_with!(GzipEncoder),
        // HTTP deflate 使用 RFC 1950 zlib 包装，而非裸 RFC 1951 流。
        HttpContentCoding::Deflate => encode_with!(ZlibEncoder),
        HttpContentCoding::Br => encode_with!(BrotliEncoder),
        HttpContentCoding::Zstd => encode_with!(ZstdEncoder),
    })
}

/// 已验证且惰性解码的响应；绝不把不受支持的压缩字节当应用正文交给插件。
pub(super) enum UpstreamResponse {
    Plain(Response),
    Empty {
        url: Url,
        status: u16,
        headers: HeaderMap,
    },
    Decoded {
        url: Url,
        status: u16,
        headers: HeaderMap,
        body: Pin<Box<DecompressionBody<reqwest::Body>>>,
        explicit: bool,
    },
}

impl UpstreamResponse {
    /// 旧入口仅由手写 Accept-Encoding 启用原有 tower-http 解码行为。
    pub(super) async fn legacy(response: Response, decode: bool) -> Self {
        if decode {
            Self::decode(response, false).await
        } else {
            Self::Plain(response)
        }
    }

    pub(super) async fn decoded(
        mut response: Response,
        method: &Method,
        compression: &HttpCompression,
    ) -> Result<Self, HostFailure> {
        let status = response.status();
        if *method == Method::HEAD
            || status.is_informational()
            || matches!(status.as_u16(), 204 | 205 | 304)
        {
            // 这些响应的编码头可能描述 GET 的表示，并不表示线上存在压缩流。
            let mut headers = response.headers().clone();
            headers.remove(CONTENT_ENCODING);
            headers.remove(CONTENT_LENGTH);
            return Ok(Self::Empty {
                url: response.url().clone(),
                status: status.as_u16(),
                headers,
            });
        }
        let mut encodings = response.headers().get_all(CONTENT_ENCODING).iter();
        let Some(encoding) = encodings.next() else {
            return Ok(Self::Plain(response));
        };
        if encodings.next().is_some() {
            return Err(response_failure(
                "multiple upstream Content-Encoding values are unsupported",
            ));
        }
        let encoding = encoding
            .to_str()
            .map_err(|_| response_failure("upstream Content-Encoding is not text"))?
            .trim();
        if encoding.eq_ignore_ascii_case("identity") {
            // 显式 identity 不经过解码器，但交给插件的头仍使用应用字节语义。
            response.headers_mut().remove(CONTENT_ENCODING);
            response.headers_mut().remove(CONTENT_LENGTH);
            return Ok(Self::Plain(response));
        }
        let coding = if encoding.eq_ignore_ascii_case("gzip") {
            HttpContentCoding::Gzip
        } else if encoding.eq_ignore_ascii_case("deflate") {
            HttpContentCoding::Deflate
        } else if encoding.eq_ignore_ascii_case("br") {
            HttpContentCoding::Br
        } else if encoding.eq_ignore_ascii_case("zstd") {
            HttpContentCoding::Zstd
        } else {
            return Err(response_failure(
                "unknown or layered upstream Content-Encoding",
            ));
        };
        if !compression.response.contains(&coding) {
            return Err(response_failure(
                "upstream Content-Encoding was not accepted",
            ));
        }
        // 统一规范化后再交给 tower-http，避免大小写/外侧空白通过校验却未被解码。
        response.headers_mut().insert(
            CONTENT_ENCODING,
            HeaderValue::from_static(coding_name(coding)),
        );
        Ok(Self::decode(response, true).await)
    }

    async fn decode(response: Response, explicit: bool) -> Self {
        let url = response.url().clone();
        let mut response = Some(axum::http::Response::from(response));
        let decoded = Decompression::new(tower::service_fn(move |_: axum::http::Request<()>| {
            std::future::ready(Ok::<_, std::convert::Infallible>(
                response.take().expect("oneshot calls the service once"),
            ))
        }))
        .oneshot(axum::http::Request::new(()))
        .await
        .unwrap_or_else(|never| match never {});
        let (mut parts, body) = decoded.into_parts();
        if explicit {
            parts.headers.remove(CONTENT_ENCODING);
            parts.headers.remove(CONTENT_LENGTH);
        }
        Self::Decoded {
            url,
            status: parts.status.as_u16(),
            headers: parts.headers,
            body: Box::pin(body),
            explicit,
        }
    }

    pub(super) fn url(&self) -> &Url {
        match self {
            Self::Plain(response) => response.url(),
            Self::Empty { url, .. } | Self::Decoded { url, .. } => url,
        }
    }

    pub(super) fn status(&self) -> u16 {
        match self {
            Self::Plain(response) => response.status().as_u16(),
            Self::Empty { status, .. } | Self::Decoded { status, .. } => *status,
        }
    }

    pub(super) fn headers(&self) -> &HeaderMap {
        match self {
            Self::Plain(response) => response.headers(),
            Self::Empty { headers, .. } | Self::Decoded { headers, .. } => headers,
        }
    }

    /// 诊断消费者必须区分实际线上正文与解码后的应用正文。
    pub(super) fn body_message_type(&self) -> &'static str {
        match self {
            Self::Decoded { explicit: true, .. } => "decoded_body_chunk",
            Self::Plain(_)
            | Self::Empty { .. }
            | Self::Decoded {
                explicit: false, ..
            } => "body_chunk",
        }
    }

    pub(super) async fn chunk(&mut self) -> Result<Option<Bytes>, HostFailure> {
        match self {
            Self::Empty { .. } => Ok(None),
            Self::Plain(response) => response.chunk().await.map_err(transport_failure),
            Self::Decoded { body, .. } => loop {
                match body.frame().await {
                    None => return Ok(None),
                    Some(Ok(frame)) => {
                        // trailers 与空数据帧不代表 EOF；每次只交付一个非空 chunk。
                        if let Ok(data) = frame.into_data()
                            && !data.is_empty()
                        {
                            return Ok(Some(data));
                        }
                    }
                    Some(Err(error)) => return Err(transport_failure(error.as_ref())),
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(response: Vec<HttpContentCoding>) -> HttpCompression {
        HttpCompression {
            request: None,
            response,
        }
    }

    fn response(body: impl Into<reqwest::Body>, coding: &str) -> Response {
        axum::http::Response::builder()
            .header(CONTENT_ENCODING, coding)
            .body(body.into())
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn all_codings_roundtrip_and_strip_wire_headers() {
        let plain = b"data: {\"message\":\"streaming application bytes\"}\n\n".repeat(2000);
        for coding in [
            HttpContentCoding::Gzip,
            HttpContentCoding::Deflate,
            HttpContentCoding::Br,
            HttpContentCoding::Zstd,
        ] {
            let wire = encode(plain.clone(), Some(coding)).await.unwrap();
            assert_ne!(wire.as_ref(), plain.as_slice());
            let mut upstream = response(wire, coding_name(coding));
            upstream
                .headers_mut()
                .insert(CONTENT_LENGTH, HeaderValue::from_static("999"));
            let mut decoded =
                UpstreamResponse::decoded(upstream, &Method::GET, &policy(vec![coding]))
                    .await
                    .unwrap();
            assert!(!decoded.headers().contains_key(CONTENT_ENCODING));
            assert!(!decoded.headers().contains_key(CONTENT_LENGTH));
            let mut actual = Vec::new();
            while let Some(chunk) = decoded.chunk().await.unwrap() {
                actual.extend_from_slice(&chunk);
            }
            assert_eq!(actual, plain);
        }
    }

    #[tokio::test]
    async fn truncated_streams_fail_instead_of_returning_wire_bytes() {
        for coding in [
            HttpContentCoding::Gzip,
            HttpContentCoding::Deflate,
            HttpContentCoding::Br,
            HttpContentCoding::Zstd,
        ] {
            let wire = encode(b"application bytes".repeat(2000), Some(coding))
                .await
                .unwrap();
            let truncated = wire.slice(..wire.len() / 2);
            let mut decoded = UpstreamResponse::decoded(
                response(truncated, coding_name(coding)),
                &Method::GET,
                &policy(vec![coding]),
            )
            .await
            .unwrap();
            loop {
                match decoded.chunk().await {
                    Err(_) => break,
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("truncated {} stream was accepted", coding_name(coding)),
                }
            }
        }
    }

    #[tokio::test]
    async fn response_allowlist_rejects_unknown_unaccepted_and_layered_codings() {
        for coding in ["gzip", "br", "zstd", "deflate", "compress", "gzip, br", ""] {
            assert!(
                UpstreamResponse::decoded(response("body", coding), &Method::GET, &policy(vec![]),)
                    .await
                    .is_err()
            );
        }
        let mut repeated = response("body", "gzip");
        repeated
            .headers_mut()
            .append(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        assert!(
            UpstreamResponse::decoded(
                repeated,
                &Method::GET,
                &policy(vec![HttpContentCoding::Gzip]),
            )
            .await
            .is_err()
        );
        let mut identity = UpstreamResponse::decoded(
            response("application bytes", "identity"),
            &Method::GET,
            &policy(vec![]),
        )
        .await
        .unwrap();
        assert_eq!(
            identity.chunk().await.unwrap().unwrap(),
            "application bytes"
        );
    }

    #[tokio::test]
    async fn streaming_yields_application_bytes_before_wire_eof() {
        use futures::StreamExt as _;
        let plain = b"data: still-open SSE stream\n\n".repeat(1000);
        let wire = encode(plain, Some(HttpContentCoding::Gzip)).await.unwrap();
        // 不提供 gzip footer，且线上连接保持打开；不能为了收齐正文才返回 SSE 数据。
        let prefix = wire.slice(..wire.len() - 8);
        let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(prefix) })
            .chain(futures::stream::pending());
        let mut decoded = UpstreamResponse::decoded(
            response(reqwest::Body::wrap_stream(stream), " GZip "),
            &Method::GET,
            &policy(vec![HttpContentCoding::Gzip]),
        )
        .await
        .unwrap();
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), decoded.chunk())
            .await
            .expect("decoder waited for EOF")
            .unwrap()
            .unwrap();
        assert!(chunk.starts_with(b"data: still-open SSE stream\n\n"));
    }

    #[tokio::test]
    async fn bodyless_responses_do_not_poll_a_compressed_stream() {
        for (method, status) in [(Method::HEAD, 200), (Method::GET, 204), (Method::GET, 304)] {
            let body = reqwest::Body::wrap_stream(futures::stream::pending::<
                Result<Bytes, std::io::Error>,
            >());
            let upstream = axum::http::Response::builder()
                .status(status)
                .header(CONTENT_ENCODING, "gzip")
                .body(body)
                .unwrap()
                .into();
            let mut decoded = UpstreamResponse::decoded(upstream, &method, &policy(vec![]))
                .await
                .unwrap();
            assert!(decoded.chunk().await.unwrap().is_none());
        }
    }

    #[test]
    fn compression_headers_have_one_owner() {
        for name in [
            "accept-encoding",
            "content-encoding",
            "content-length",
            "sec-websocket-extensions",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_static("gzip"),
            );
            assert!(prepare(&mut headers, &policy(vec![])).is_err());
        }
        let mut headers = HeaderMap::new();
        prepare(&mut headers, &policy(vec![])).unwrap();
        assert_eq!(headers[ACCEPT_ENCODING], "identity");
    }
}

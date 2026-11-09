//! HTTP content coding without changing the plugin's ordered wire fields.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
use bytes::{Bytes, BytesMut};
use stravia_vendor_runtime::HostFailure;
use stravia_vendor_sdk::net::{self, Field, Fields, Options};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader, ReadBuf};
use url::Url;

use super::{invalid, transport, transport_failure};

const CHUNK_SIZE: usize = 16 * 1024;

fn option<'a>(options: &'a Options, key: &str, default: &'a str) -> &'a str {
    options
        .iter()
        .find_map(|(name, value)| (name == key).then_some(value))
        .unwrap_or(default)
}

/// Check declarations only; fields are always supplied by the plugin.
pub(super) fn prepare(headers: &Fields, options: &Options) -> Result<(), HostFailure> {
    if let Some(request) = options
        .iter()
        .find_map(|(key, value)| (key == net::keys::REQUEST_ENCODING).then_some(value))
    {
        let mut content = headers
            .iter()
            .filter(|f| f.name.eq_ignore_ascii_case("content-encoding"));
        if let Some(field) = content.next() {
            if content.next().is_some()
                || !field
                    .bytes
                    .trim_ascii()
                    .eq_ignore_ascii_case(request.as_bytes())
            {
                return Err(invalid(
                    "Content-Encoding does not match HTTP request encoding",
                ));
            }
        } else if request != "identity" {
            return Err(invalid("HTTP request encoding requires Content-Encoding"));
        }
    }

    let decoding = option(options, net::keys::RESPONSE_DECODING, "none");
    if decoding != "none"
        && (!headers
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("accept-encoding"))
            || decoding.split(',').any(|coding| {
                let mut exact = None;
                let mut wildcard = None;
                for item in headers
                    .iter()
                    .filter(|f| f.name.eq_ignore_ascii_case("accept-encoding"))
                    .flat_map(|f| f.bytes.split(|b| *b == b','))
                {
                    let mut parts = item.split(|b| *b == b';');
                    let name = parts.next().unwrap_or_default().trim_ascii();
                    let mut quality = 1.0_f32;
                    for part in parts {
                        let part = part.trim_ascii();
                        if let Some(index) = part.iter().position(|b| *b == b'=') {
                            let (key, rest) = part.split_at(index);
                            if key.trim_ascii().eq_ignore_ascii_case(b"q") {
                                quality = std::str::from_utf8(rest[1..].trim_ascii())
                                    .ok()
                                    .and_then(|v| v.parse::<f32>().ok())
                                    .filter(|v| (0.0..=1.0).contains(v))
                                    .unwrap_or(0.0);
                            }
                        }
                    }
                    if name.eq_ignore_ascii_case(coding.as_bytes()) {
                        exact = Some(quality);
                    } else if name == b"*" {
                        wildcard = Some(quality);
                    }
                }
                exact
                    .or(wildcard)
                    .unwrap_or(if coding == "identity" { 1.0 } else { 0.0 })
                    == 0.0
            }))
    {
        return Err(invalid(
            "HTTP response decoding requires matching Accept-Encoding",
        ));
    }
    Ok(())
}

/// The returned bytes are replayable for redirects. Identity transfers ownership.
pub(super) async fn encode(body: Vec<u8>, options: &Options) -> Result<Bytes, HostFailure> {
    macro_rules! encode_with {
        ($encoder:ident) => {{
            let mut encoder = $encoder::new(Vec::new());
            for chunk in body.chunks(CHUNK_SIZE) {
                encoder.write_all(chunk).await.map_err(transport_failure)?;
                tokio::task::yield_now().await;
            }
            encoder.shutdown().await.map_err(transport_failure)?;
            Bytes::from(encoder.into_inner())
        }};
    }
    Ok(
        match option(options, net::keys::REQUEST_ENCODING, "identity") {
            "identity" => Bytes::from(body),
            "gzip" => encode_with!(GzipEncoder),
            "deflate" => encode_with!(ZlibEncoder),
            "br" => encode_with!(BrotliEncoder),
            "zstd" => encode_with!(ZstdEncoder),
            _ => return Err(invalid("invalid HTTP request encoding")),
        },
    )
}

// One outstanding transport read, with no producer task or unbounded queue. The
// decoder drives this reader only as the guest consumes decoded chunks.
type BodyFuture = Pin<
    Box<
        dyn Future<
                Output = (
                    transport::UpstreamBody,
                    Result<Option<Bytes>, transport::TransportError>,
                ),
            > + Send,
    >,
>;
struct BodyReader {
    body: Option<transport::UpstreamBody>,
    pending: Option<BodyFuture>,
    chunk: Bytes,
    eof: bool,
}
impl BodyReader {
    fn new(body: transport::UpstreamBody) -> Self {
        Self {
            body: Some(body),
            pending: None,
            chunk: Bytes::new(),
            eof: false,
        }
    }
}
impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.chunk.is_empty() {
                let count = self.chunk.len().min(buf.remaining());
                buf.put_slice(&self.chunk.split_to(count));
                return Poll::Ready(Ok(()));
            }
            if self.eof || buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            if self.pending.is_none() {
                let mut body = self.body.take().expect("body exists between reads");
                self.pending = Some(Box::pin(async move {
                    let result = body.next_chunk().await;
                    (body, result)
                }));
            }
            let (body, result) =
                std::task::ready!(self.pending.as_mut().unwrap().as_mut().poll(cx));
            self.pending = None;
            self.body = Some(body);
            match result {
                Ok(Some(chunk)) if chunk.is_empty() => continue,
                Ok(Some(chunk)) => self.chunk = chunk,
                Ok(None) => self.eof = true,
                Err(error) => return Poll::Ready(Err(std::io::Error::other(error))),
            }
        }
    }
}

type Decoder = Pin<Box<dyn AsyncRead + Send>>;
enum Body {
    Empty,
    Plain(transport::UpstreamBody),
    Decoded(Decoder),
}

pub(super) struct UpstreamResponse {
    url: Url,
    status: u16,
    headers: Vec<Field>,
    body: Body,
    #[cfg(test)]
    prefetched: std::collections::VecDeque<Bytes>,
    #[cfg(test)]
    prefetch_error: Option<HostFailure>,
}
impl UpstreamResponse {
    pub(super) fn new(
        response: transport::UpstreamResponse,
        url: Url,
        method: &str,
        options: &Options,
    ) -> Result<Self, HostFailure> {
        let transport::UpstreamResponse { head, body } = response;
        let mut headers = head.fields;
        let decoding = option(options, net::keys::RESPONSE_DECODING, "none");
        if decoding == "none" {
            return Ok(Self {
                url,
                status: head.status,
                headers,
                #[cfg(test)]
                prefetched: Default::default(),
                #[cfg(test)]
                prefetch_error: None,
                body: Body::Plain(body),
            });
        }
        if method.eq_ignore_ascii_case("HEAD")
            || (100..200).contains(&head.status)
            || matches!(head.status, 204 | 205 | 304)
        {
            headers.retain(|f| {
                !f.name.eq_ignore_ascii_case("content-encoding")
                    && !f.name.eq_ignore_ascii_case("content-length")
            });
            return Ok(Self {
                url,
                status: head.status,
                headers,
                #[cfg(test)]
                prefetched: Default::default(),
                #[cfg(test)]
                prefetch_error: None,
                body: Body::Empty,
            });
        }
        let mut encodings = headers
            .iter()
            .filter(|f| f.name.eq_ignore_ascii_case("content-encoding"));
        let Some(field) = encodings.next() else {
            return Ok(Self {
                url,
                status: head.status,
                headers,
                #[cfg(test)]
                prefetched: Default::default(),
                #[cfg(test)]
                prefetch_error: None,
                body: Body::Plain(body),
            });
        };
        if encodings.next().is_some() {
            return Err(response_failure(
                "multiple upstream Content-Encoding values are unsupported",
            ));
        }
        let coding = std::str::from_utf8(field.bytes.trim_ascii())
            .map_err(|_| response_failure("upstream Content-Encoding is not text"))?;
        let known = ["identity", "gzip", "deflate", "br", "zstd"]
            .into_iter()
            .find(|v| coding.eq_ignore_ascii_case(v))
            .ok_or_else(|| response_failure("unknown or layered upstream Content-Encoding"))?;
        if known != "identity" && !decoding.split(',').any(|v| v == known) {
            return Err(response_failure(
                "upstream Content-Encoding was not accepted",
            ));
        }
        let body = if known == "identity" {
            Body::Plain(body)
        } else {
            let reader = BufReader::new(BodyReader::new(body));
            let decoder: Decoder = match known {
                "gzip" => Box::pin(GzipDecoder::new(reader)),
                "deflate" => Box::pin(ZlibDecoder::new(reader)),
                "br" => Box::pin(BrotliDecoder::new(reader)),
                "zstd" => Box::pin(ZstdDecoder::new(reader)),
                _ => unreachable!(),
            };
            Body::Decoded(decoder)
        };
        headers.retain(|f| {
            !f.name.eq_ignore_ascii_case("content-encoding")
                && !f.name.eq_ignore_ascii_case("content-length")
        });
        Ok(Self {
            url,
            status: head.status,
            headers,
            body,
            #[cfg(test)]
            prefetched: Default::default(),
            #[cfg(test)]
            prefetch_error: None,
        })
    }
    pub(super) fn url(&self) -> &Url {
        &self.url
    }
    pub(super) fn status(&self) -> u16 {
        self.status
    }
    pub(super) fn headers(&self) -> &[Field] {
        &self.headers
    }
    pub(super) fn body_message_type(&self) -> &'static str {
        if matches!(self.body, Body::Decoded(_)) {
            "decoded_body_chunk"
        } else {
            "body_chunk"
        }
    }
    #[cfg(test)]
    pub(super) async fn prefetch_diagnostic(&mut self, capture: &transport::DiagnosticCapture) {
        let id = capture.begin(
            self.status,
            transport::DiagnosticContentCoding::from_fields(&self.headers),
        );
        let mut prefetched = std::collections::VecDeque::new();
        let mut length = 0usize;
        while length < 256 * 1024 {
            match self.chunk().await {
                Ok(Some(chunk)) => {
                    capture.chunk(id, &chunk);
                    length = length.saturating_add(chunk.len());
                    prefetched.push_back(chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    self.prefetch_error = Some(error);
                    break;
                }
            }
        }
        self.prefetched = prefetched;
    }

    pub(super) async fn chunk(&mut self) -> Result<Option<Bytes>, HostFailure> {
        #[cfg(test)]
        {
            if let Some(chunk) = self.prefetched.pop_front() {
                return Ok(Some(chunk));
            }
            if let Some(error) = self.prefetch_error.take() {
                return Err(error);
            }
        }
        match &mut self.body {
            Body::Empty => Ok(None),
            Body::Plain(body) => loop {
                match body.next_chunk().await.map_err(transport_failure)? {
                    Some(bytes) if bytes.is_empty() => continue,
                    result => return Ok(result),
                }
            },
            Body::Decoded(body) => {
                let mut buffer = BytesMut::zeroed(CHUNK_SIZE);
                let size = body.read(&mut buffer).await.map_err(transport_failure)?;
                if size == 0 {
                    Ok(None)
                } else {
                    buffer.truncate(size);
                    Ok(Some(buffer.freeze()))
                }
            }
        }
    }
}
fn response_failure(message: &'static str) -> HostFailure {
    transport_failure(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

//! 沿用 rmcp 3.5 的 HTTP 语义，在 JSON/SSE 解码之前捕获字节。
//! rmcp 在独立 worker 中调用客户端，因此这里显式持有请求的抓包归属。
use crate::wire::{self, Exchange};
use futures::StreamExt;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{ClientJsonRpcMessage, ClientRequest, ServerJsonRpcMessage},
    transport::{
        common::client_side_sse::BoxedSseResponse,
        streamable_http_client::{
            AuthRequiredError, InsufficientScopeError, StreamableHttpClient,
            StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
        },
        StreamableHttpClientTransport,
    },
    RoleClient,
};

/// 创建远程工具的 MCP HTTP 传输，不发起网络请求。
///
/// 配置、认证与 SSE 限额遵循 rmcp；连接、协议和正文错误在使用传输时返回。
/// 构造时绑定当前请求的 Debug 接收端，使后台 worker 和重连仍归属同一请求。
pub fn mcp_http_transport(
    client: reqwest::Client,
    config: StreamableHttpClientTransportConfig,
) -> impl rmcp::transport::Transport<RoleClient> {
    let client = Client::new(client);
    let transport = StreamableHttpClientTransport::with_client(client.clone(), config);
    let cancellation = transport.cancel_token();
    client.cancel_on_event_limit(move || cancellation.cancel());
    transport
}
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use stravia_runtime_contract::wire_debug::{self, Capture};

type Error = StreamableHttpError<reqwest::Error>;
const DEFAULT_MAX_EVENT_SIZE: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct FatalEvent {
    exceeded: AtomicBool,
    cancel: parking_lot::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
impl FatalEvent {
    fn reject(&self) {
        self.exceeded.store(true, Ordering::Release);
        if let Some(cancel) = self.cancel.lock().as_ref() {
            cancel();
        }
    }
}

#[derive(Clone)]
pub(crate) struct Client {
    inner: reqwest::Client,
    capture: Option<Capture>,
    fatal: Arc<FatalEvent>,
}
impl Client {
    pub(crate) fn new(inner: reqwest::Client) -> Self {
        Self {
            inner,
            capture: wire_debug::current(),
            fatal: Arc::default(),
        }
    }

    pub(crate) fn cancel_on_event_limit(&self, cancel: impl Fn() + Send + Sync + 'static) {
        *self.fatal.cancel.lock() = Some(Box::new(cancel));
    }

    fn request(
        &self,
        method: reqwest::Method,
        uri: &str,
        session: Option<&str>,
        token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        accept: bool,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let mut request = self.inner.request(method, uri);
        if accept {
            request = request.header("accept", "text/event-stream, application/json");
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(session) = session {
            request = request.header("Mcp-Session-Id", session);
        }
        for (name, value) in headers {
            if ["accept", "mcp-session-id", "last-event-id"]
                .iter()
                .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
            {
                return Err(Error::ReservedHeaderConflict(name.to_string()));
            }
            request = request.header(name, value);
        }
        Ok(request)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(reqwest::Response, Option<Exchange>), Error> {
        // rmcp 的致命 SSE 标记不公开。超限时取消传输，并在任何立即重连的
        // HTTP 执行前拒绝请求；普通网络错误不置位，保留既有重连语义。
        if self.fatal.exceeded.load(Ordering::Acquire) {
            return Err(Error::UnexpectedServerResponse(
                "SSE event size limit exceeded".into(),
            ));
        }
        wire::send(request, self.capture.clone(), "web-access/mcp")
            .await
            .map_err(Error::Client)
    }
}

fn auth_error(response: &reqwest::Response) -> Result<(), Error> {
    let status = response.status().as_u16();
    if !matches!(status, 401 | 403) {
        return Ok(());
    }
    let Some(value) = response.headers().get("www-authenticate") else {
        return Ok(());
    };
    let value = value
        .to_str()
        .map_err(|_| {
            Error::UnexpectedServerResponse("invalid www-authenticate header value".into())
        })?
        .to_owned();
    if status == 401 {
        return Err(Error::AuthRequired(AuthRequiredError::new(value)));
    }
    let scope = value
        .to_ascii_lowercase()
        .find("scope=")
        .and_then(|position| {
            let suffix = &value[position + 6..];
            if let Some(quoted) = suffix.strip_prefix('"') {
                quoted.find('"').map(|end| quoted[..end].to_owned())
            } else {
                let end = suffix
                    .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
                    .unwrap_or(suffix.len());
                (end > 0).then(|| suffix[..end].to_owned())
            }
        });
    Err(Error::InsufficientScope(InsufficientScopeError::new(
        value, scope,
    )))
}

fn unread(response: &reqwest::Response, exchange: &Option<Exchange>) {
    if response.content_length() != Some(0) {
        if let Some(exchange) = exchange {
            exchange.gap("mcp_http_body_not_consumed");
        }
    }
}

// 对齐 rmcp 的原始事件上限：注释不跨行保留，CRLF 只算一次换行，
// 保留的非注释行计入拼接换行符。
struct EventLimit {
    max: usize,
    retained: usize,
    line: usize,
    comment: bool,
    cr: bool,
}
impl EventLimit {
    fn observe(&mut self, bytes: &[u8]) -> bool {
        for &byte in bytes {
            if self.cr {
                self.cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    if self.line == 0 {
                        self.retained = 0;
                    } else if !self.comment {
                        self.retained = self.retained.saturating_add(self.line).saturating_add(1);
                    }
                    self.line = 0;
                    self.comment = false;
                    self.cr = byte == b'\r';
                }
                _ => {
                    if self.line == 0 {
                        self.comment = byte == b':';
                    }
                    self.line = self.line.saturating_add(1);
                }
            }
            if self.retained.saturating_add(self.line) > self.max {
                return false;
            }
        }
        true
    }
}

fn sse(
    response: reqwest::Response,
    exchange: Option<Exchange>,
    max: usize,
    fatal: Arc<FatalEvent>,
) -> BoxedSseResponse {
    let source = wire::stream(response, exchange).boxed();
    let limited = futures::stream::unfold(
        (
            source,
            EventLimit {
                max,
                retained: 0,
                line: 0,
                comment: false,
                cr: false,
            },
            false,
        ),
        move |(mut source, mut limit, failed)| {
            let fatal = fatal.clone();
            async move {
                if failed {
                    return None;
                }
                let chunk = source.next().await?;
                let chunk = match chunk {
                    Ok(bytes) if limit.observe(&bytes) => Ok(bytes),
                    Ok(_) => {
                        fatal.reject();
                        Err(std::io::Error::other(format!(
                            "SSE event exceeded the maximum size of {} bytes",
                            limit.max
                        )))
                    }
                    Err(error) => Err(std::io::Error::other(error)),
                };
                let failed = chunk.is_err();
                Some((chunk, (source, limit, failed)))
            }
        },
    );
    sse_stream::SseStream::from_bytes_stream(limited).boxed()
}

impl StreamableHttpClient for Client {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session,
            token,
            headers,
            DEFAULT_MAX_EVENT_SIZE,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let attached = session.is_some();
        let request = self
            .request(
                reqwest::Method::POST,
                &uri,
                session.as_deref(),
                token,
                headers,
                true,
            )?
            .json(&message);
        let (response, exchange) = self.send(request).await?;
        if let Err(error) = auth_error(&response) {
            unread(&response, &exchange);
            return Err(error);
        }
        let status = response.status();
        if matches!(status.as_u16(), 202 | 204) {
            unread(&response, &exchange);
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status.as_u16() == 404 && attached {
            unread(&response, &exchange);
            return Err(Error::SessionExpired);
        }
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let no_reply = matches!(
            message,
            ClientJsonRpcMessage::Notification(_)
                | ClientJsonRpcMessage::Response(_)
                | ClientJsonRpcMessage::Error(_)
        );
        if status.is_success() && response.content_length() == Some(0) && no_reply {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !status.is_success() {
            let bytes = wire::body(response, exchange).await;
            let body = bytes
                .as_ref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_else(|_| "<failed to read response body>".to_owned());
            // 保留 rmcp 对旧服务发现拒绝响应的请求关联。
            if !attached && status.is_client_error() && !matches!(status.as_u16(), 401 | 403) {
                if let ClientJsonRpcMessage::Request(request) = &message {
                    if matches!(request.request, ClientRequest::DiscoverRequest(_)) {
                        let error = match serde_json::from_str::<ServerJsonRpcMessage>(&body) {
                            Ok(ServerJsonRpcMessage::Error(error)) => error.error,
                            _ => rmcp::ErrorData::invalid_request(
                                format!("server/discover rejected with HTTP {status}: {body}"),
                                None,
                            ),
                        };
                        return Ok(StreamableHttpPostResponse::Json(
                            ServerJsonRpcMessage::error(error, Some(request.id.clone())),
                            None,
                        ));
                    }
                }
            }
            if content_type
                .as_deref()
                .is_some_and(|v| v.starts_with("application/json"))
            {
                if let Ok(message @ ServerJsonRpcMessage::Error(_)) = serde_json::from_str(&body) {
                    return Ok(StreamableHttpPostResponse::Json(message, session_id));
                }
            }
            return Err(Error::UnexpectedServerResponse(
                format!("HTTP {status}: {body}").into(),
            ));
        }
        match content_type.as_deref() {
            Some(value) if value.starts_with("text/event-stream") => {
                Ok(StreamableHttpPostResponse::Sse(
                    sse(response, exchange, max, self.fatal.clone()),
                    session_id,
                ))
            }
            Some(value) if value.starts_with("application/json") => {
                let body = wire::body(response, exchange)
                    .await
                    .map_err(Error::Client)?;
                match serde_json::from_slice(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session_id)),
                    Err(error) if no_reply => {
                        tracing::warn!(%error, "invalid MCP notification response treated as accepted");
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                    Err(error) => Err(Error::UnexpectedServerResponse(
                        format!(
                            "could not parse JSON response as ServerJsonRpcMessage: {error}: {}",
                            String::from_utf8_lossy(&body[..body.len().min(256)])
                        )
                        .into(),
                    )),
                }
            }
            _ => {
                unread(&response, &exchange);
                Err(Error::UnexpectedContentType(content_type))
            }
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, Error> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session,
            last,
            token,
            headers,
            DEFAULT_MAX_EVENT_SIZE,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<BoxedSseResponse, Error> {
        let mut request = self.request(
            reqwest::Method::GET,
            &uri,
            session.as_deref(),
            token,
            headers,
            true,
        )?;
        if let Some(last) = last {
            request = request.header("Last-Event-Id", last);
        }
        let (response, exchange) = self.send(request).await?;
        if response.status().as_u16() == 405 {
            unread(&response, &exchange);
            return Err(Error::ServerDoesNotSupportSse);
        }
        if let Err(error) = auth_error(&response) {
            unread(&response, &exchange);
            return Err(error);
        }
        if let Err(error) = response.error_for_status_ref() {
            unread(&response, &exchange);
            return Err(Error::Client(error));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        if !content_type.as_deref().is_some_and(|v| {
            v.starts_with("text/event-stream") || v.starts_with("application/json")
        }) {
            unread(&response, &exchange);
            return Err(Error::UnexpectedContentType(content_type));
        }
        Ok(sse(response, exchange, max, self.fatal.clone()))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        let request = self.request(
            reqwest::Method::DELETE,
            &uri,
            Some(&session),
            token,
            headers,
            false,
        )?;
        let (response, exchange) = self.send(request).await?;
        unread(&response, &exchange);
        if response.status().as_u16() == 405 {
            return Ok(());
        }
        response.error_for_status_ref().map_err(Error::Client)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use stravia_runtime_contract::wire_debug::{WireEvent, WireSink};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<WireEvent>>>);
    impl WireSink for Sink {
        fn record(&self, event: WireEvent) {
            self.0.lock().push(event);
        }
        fn gap(&self, _: &'static str) {}
    }

    async fn fixture(
        status: &str,
        content_type: &str,
        body: &[u8],
    ) -> (Arc<str>, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri: Arc<str> = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let body = body.to_vec();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head_bytes = Vec::new();
            while !head_bytes.ends_with(b"\r\n\r\n") {
                head_bytes.push(stream.read_u8().await.unwrap());
            }
            let length = String::from_utf8(head_bytes)
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut request_body = vec![0; length];
            stream.read_exact(&mut request_body).await.unwrap();
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            request_body
        });
        (uri, server)
    }

    fn ping() -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"})).unwrap()
    }

    #[tokio::test]
    async fn spawned_mcp_worker_keeps_capture_and_raw_sse_before_parsing() {
        let raw = b": original comment\r\nid: wire-id\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\n";
        let (uri, server) = fixture("200 OK", "text/event-stream", raw).await;
        let sink = Sink::default();
        let client = wire_debug::scope(Some(Capture::new(sink.clone())), async {
            Client::new(reqwest::Client::builder().no_proxy().build().unwrap())
        })
        .await;
        let worker = tokio::spawn(async move {
            assert!(wire_debug::current().is_none());
            let result = client
                .post_message(uri, ping(), None, None, HashMap::new())
                .await
                .unwrap();
            let StreamableHttpPostResponse::Sse(mut stream, _) = result else {
                panic!("expected SSE")
            };
            assert!(stream.next().await.unwrap().is_ok());
            assert!(stream.next().await.is_none());
        });
        worker.await.unwrap();
        let sent = server.await.unwrap();
        let events = sink.0.lock();
        let request = events
            .iter()
            .find(|event| {
                event.direction == "upstream_request" && event.message_type == "body_chunk"
            })
            .unwrap();
        assert_eq!(request.payload.as_ref(), sent);
        let received = events
            .iter()
            .filter(|event| {
                event.direction == "upstream_response" && event.message_type == "body_chunk"
            })
            .flat_map(|event| event.payload.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(received, raw);
    }

    #[tokio::test]
    async fn mcp_error_body_keeps_original_binary_and_sse_bound_rejects_large_events() {
        let raw = b"\xff\0upstream failure";
        let (uri, server) = fixture("503 Unavailable", "application/octet-stream", raw).await;
        let sink = Sink::default();
        let result = wire_debug::scope(Some(Capture::new(sink.clone())), async {
            Client::new(reqwest::Client::builder().no_proxy().build().unwrap())
                .post_message(uri, ping(), None, None, HashMap::new())
                .await
        })
        .await;
        assert!(result.is_err());
        server.await.unwrap();
        let received = sink
            .0
            .lock()
            .iter()
            .filter(|event| {
                event.direction == "upstream_response" && event.message_type == "body_chunk"
            })
            .flat_map(|event| event.payload.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(received, raw);

        let (uri, server) =
            fixture("200 OK", "text/event-stream", b"data: oversized\r\n\r\n").await;
        let client = Client::new(reqwest::Client::builder().no_proxy().build().unwrap());
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = cancelled.clone();
        client.cancel_on_event_limit(move || {
            observed.store(true, Ordering::Release);
        });
        let response = client
            .post_message_with_max_sse_event_size(uri, ping(), None, None, HashMap::new(), 4)
            .await
            .unwrap();
        let StreamableHttpPostResponse::Sse(mut stream, _) = response else {
            panic!("expected SSE")
        };
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        assert!(cancelled.load(Ordering::Acquire));
        // rmcp 可能在观察到取消前立即重连；必须在打开新 socket 前拒绝。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri: Arc<str> = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
        let reconnect = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.get_stream(uri, None, Some("prior-event".into()), None, HashMap::new()),
        )
        .await
        .expect("fatal reconnect must resolve without waiting for HTTP");
        assert!(matches!(reconnect, Err(Error::UnexpectedServerResponse(_))));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ordinary_http_failure_does_not_disable_subsequent_requests() {
        let client = Client::new(reqwest::Client::builder().no_proxy().build().unwrap());
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = cancelled.clone();
        client.cancel_on_event_limit(move || {
            observed.store(true, Ordering::Release);
        });
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_uri: Arc<str> = format!("http://{}/mcp", closed.local_addr().unwrap()).into();
        drop(closed);
        assert!(client
            .post_message(closed_uri, ping(), None, None, HashMap::new())
            .await
            .is_err());
        assert!(!cancelled.load(Ordering::Acquire));
        let (uri, server) = fixture("503 Unavailable", "text/plain", b"ordinary failure").await;
        assert!(client
            .post_message(uri, ping(), None, None, HashMap::new())
            .await
            .is_err());
        server.await.unwrap();
        assert!(!cancelled.load(Ordering::Acquire));
        let (uri, server) = fixture(
            "200 OK",
            "application/json",
            br#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        )
        .await;
        assert!(matches!(
            client
                .post_message(uri, ping(), None, None, HashMap::new())
                .await,
            Ok(StreamableHttpPostResponse::Json(..))
        ));
        server.await.unwrap();
    }
}

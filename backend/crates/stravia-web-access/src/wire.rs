use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use stravia_runtime_contract::wire_debug::{Capture, WireEvent};

/// 单次远程交互显式归属调用请求，避免 MCP 后台 worker 丢失 task-local。
#[derive(Clone)]
pub struct Exchange {
    capture: Capture,
    url: String,
    protocol: &'static str,
}

pub(crate) fn headers<'a>(values: impl Iterator<Item = (&'a str, &'a [u8])>) -> Value {
    let mut headers = serde_json::Map::new();
    for (name, value) in values {
        let value =
            std::str::from_utf8(value).map_or_else(|_| json!({"bytes": value}), |text| json!(text));
        match headers.entry(name.to_owned()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(value);
            }
            serde_json::map::Entry::Occupied(mut entry) => {
                if let Value::Array(values) = entry.get_mut() {
                    values.push(value);
                } else {
                    let first = entry.insert(Value::Null);
                    entry.insert(Value::Array(vec![first, value]));
                }
            }
        }
    }
    Value::Object(headers)
}

impl Exchange {
    pub(crate) fn new(capture: Option<Capture>, url: &str, protocol: &'static str) -> Option<Self> {
        let protocol = match protocol {
            "http" => "web-access/http",
            protocol => protocol,
        };
        capture.map(|capture| Self {
            capture,
            url: url.to_owned(),
            protocol,
        })
    }

    pub(crate) fn event(
        &self,
        direction: &'static str,
        kind: &'static str,
        status: Option<u16>,
        headers: Value,
        payload: Bytes,
    ) {
        let direction = match direction {
            "request" => "upstream_request",
            "response" => "upstream_response",
            direction => direction,
        };
        let kind = match (direction, kind) {
            ("upstream_request", "headers") => "request",
            ("upstream_response", "headers") => "response_headers",
            (_, kind) => kind,
        };
        self.capture.record(WireEvent {
            direction,
            transport: "http",
            protocol: self.protocol,
            message_type: kind,
            url: self.url.clone(),
            status_code: status,
            headers,
            payload,
        });
    }

    pub(crate) fn chunk(&self, bytes: Bytes) {
        self.event("response", "body_chunk", None, Value::Null, bytes);
    }

    pub(crate) fn gap(&self, reason: &'static str) {
        self.capture.gap(reason);
    }
}

pub async fn send(
    builder: reqwest::RequestBuilder,
    capture: Option<Capture>,
    protocol: &'static str,
) -> Result<(reqwest::Response, Option<Exchange>), reqwest::Error> {
    let (client, request) = builder.build_split();
    let request = request?;
    let mut exchange = Exchange::new(capture, request.url().as_str(), protocol);
    if let Some(exchange) = &exchange {
        exchange.event(
            "request",
            "headers",
            None,
            headers(
                request
                    .headers()
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.as_bytes())),
            ),
            Bytes::new(),
        );
        if let Some(body) = request.body() {
            if let Some(bytes) = body.as_bytes() {
                exchange.event(
                    "request",
                    "body_chunk",
                    None,
                    Value::Null,
                    Bytes::copy_from_slice(bytes),
                );
            } else {
                exchange.gap("http_request_stream_body_unavailable");
            }
        }
    }
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            if let Some(exchange) = &exchange {
                exchange.gap("http_request_transport_failed");
            }
            return Err(error);
        }
    };
    if let Some(exchange) = &mut exchange {
        if response.url().as_str() != exchange.url {
            exchange.gap("reqwest_automatic_redirect_intermediate_exchange_unavailable");
            exchange.url = response.url().as_str().to_owned();
        }
        exchange.event(
            "response",
            "headers",
            Some(response.status().as_u16()),
            headers(
                response
                    .headers()
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.as_bytes())),
            ),
            Bytes::new(),
        );
    }
    Ok((response, exchange))
}

pub fn stream(
    response: reqwest::Response,
    exchange: Option<Exchange>,
) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send {
    struct Pending(Option<Exchange>);
    impl Drop for Pending {
        fn drop(&mut self) {
            if let Some(exchange) = &self.0 {
                exchange.gap("http_response_body_not_fully_consumed");
            }
        }
    }
    futures::stream::unfold(
        (response, Pending(exchange)),
        |(mut response, mut pending)| async move {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    if let Some(exchange) = &pending.0 {
                        exchange.chunk(bytes.clone());
                    }
                    Some((Ok(bytes), (response, pending)))
                }
                Ok(None) => {
                    drop(pending.0.take());
                    None
                }
                Err(error) => {
                    if let Some(exchange) = pending.0.take() {
                        exchange.gap("http_response_body_read_failed");
                    }
                    Some((Err(error), (response, pending)))
                }
            }
        },
    )
}

pub async fn body(
    response: reqwest::Response,
    exchange: Option<Exchange>,
) -> Result<Vec<u8>, reqwest::Error> {
    let stream = stream(response, exchange);
    futures::pin_mut!(stream);
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use stravia_runtime_contract::wire_debug::{self, WireSink};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<WireEvent>>>);

    impl WireSink for Sink {
        fn record(&self, event: WireEvent) {
            self.0.lock().push(event);
        }
        fn gap(&self, _: &'static str) {}
    }

    async fn fixture() -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/wire", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            let mut body = [0; 4];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(body, [0, 255, 1, 128]);
            socket.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 4\r\nX-Wire: original\r\nConnection: close\r\n\r\n").await.unwrap();
            socket.write_all(&[255, 0, 128, 1]).await.unwrap();
            request
        });
        (url, server)
    }

    async fn exchange(url: String) -> Vec<u8> {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (response, exchange) = send(
            client
                .post(url)
                .header("x-request", "unchanged")
                .body(vec![0, 255, 1, 128]),
            wire_debug::current(),
            "web-access/http",
        )
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), 503);
        body(response, exchange).await.unwrap()
    }

    #[tokio::test]
    async fn loopback_preserves_binary_error_and_request_bytes_with_isolated_capture() {
        let (first_url, first_server) = fixture().await;
        let (second_url, second_server) = fixture().await;
        let first = Sink::default();
        let second = Sink::default();
        let (first_body, second_body) = tokio::join!(
            wire_debug::scope(
                Some(Capture::new(first.clone())),
                exchange(first_url.clone())
            ),
            wire_debug::scope(
                Some(Capture::new(second.clone())),
                exchange(second_url.clone())
            ),
        );
        assert_eq!(first_body, [255, 0, 128, 1]);
        assert_eq!(second_body, first_body);
        for (sink, url) in [(&first, &first_url), (&second, &second_url)] {
            let events = sink.0.lock();
            assert!(events.iter().all(|event| event.url == *url));
            assert!(events
                .iter()
                .any(|event| event.direction == "upstream_request"
                    && event.payload.as_ref() == [0, 255, 1, 128]));
            assert!(events.iter().any(|event| event.status_code == Some(503)
                && event.headers.to_string().contains("original")));
            let received = events
                .iter()
                .filter(|event| {
                    event.direction == "upstream_response" && event.message_type == "body_chunk"
                })
                .flat_map(|event| event.payload.iter().copied())
                .collect::<Vec<_>>();
            assert_eq!(received, first_body);
        }
        first_server.await.unwrap();
        second_server.await.unwrap();
    }

    #[tokio::test]
    async fn loopback_debug_disabled_has_no_exchange() {
        let (url, server) = fixture().await;
        wire_debug::scope(None, async {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let (response, capture) = send(
                client.post(url).body(vec![0, 255, 1, 128]),
                wire_debug::current(),
                "web-access/http",
            )
            .await
            .unwrap();
            assert!(capture.is_none());
            assert_eq!(body(response, capture).await.unwrap(), [255, 0, 128, 1]);
        })
        .await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn interrupted_body_does_not_record_local_error_as_remote_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/wire", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n\xff")
                .await
                .unwrap();
        });
        let sink = Sink::default();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (response, exchange) = send(
            client.get(url),
            Some(Capture::new(sink.clone())),
            "web-access/http",
        )
        .await
        .unwrap();
        assert!(body(response, exchange).await.is_err());
        server.await.unwrap();
        let events = sink.0.lock();
        assert!(events
            .iter()
            .all(|event| event.message_type != "transport_error"));
        assert!(events
            .iter()
            .filter(|event| event.direction == "upstream_response")
            .all(|event| event.payload.is_empty() || event.payload.as_ref() == [255]));
    }
}

//! Executes a component built from the immutable 1.0.0 WIT against this host.
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, LogLevel, OperationScope,
    RuntimeEvent, VendorRuntime, WebSocketMessage,
};
use stravia_vendor_sdk::{
    AiRequest, AiResponse, ErrorKind, OperationInput, OperationOutput, ProviderSnapshot,
    net::{ConnectRequest, Field, Options, Request},
};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use tungstenite_deflate::{Message, WebSocket};

fn failure(error: impl std::fmt::Display) -> HostFailure {
    HostFailure::new(ErrorKind::Invalid, error.to_string())
}

struct HttpData {
    status: u16,
    headers: Vec<Field>,
    body: Vec<u8>,
}
struct LiveResponse {
    task: AsyncMutex<Option<tokio::task::JoinHandle<Result<HttpData, HostFailure>>>>,
    data: OnceCell<HttpData>,
    consumed: AsyncMutex<bool>,
}
impl LiveResponse {
    async fn data(&self) -> Result<&HttpData, HostFailure> {
        self.data
            .get_or_try_init(|| async {
                self.task
                    .lock()
                    .await
                    .take()
                    .expect("one HTTP task")
                    .await
                    .map_err(failure)?
            })
            .await
    }
}
#[async_trait]
impl HostHttpResponse for LiveResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.data().await?.status)
    }
    async fn headers(&self) -> Result<Vec<Field>, HostFailure> {
        Ok(self.data().await?.headers.clone())
    }
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        let mut consumed = self.consumed.lock().await;
        if *consumed {
            return Ok(None);
        }
        *consumed = true;
        Ok(Some(self.data().await?.body.clone()))
    }
}
struct LiveSocket {
    socket: Mutex<WebSocket<TcpStream>>,
    status: u16,
    headers: Vec<Field>,
}
#[async_trait]
impl HostWebSocket for LiveSocket {
    fn is_fresh_handshake(&self) -> bool {
        true
    }
    fn handshake_status(&self) -> u16 {
        self.status
    }
    fn handshake_headers(&self) -> Vec<Field> {
        self.headers.clone()
    }
    async fn send(&self, message: WebSocketMessage) -> Result<(), HostFailure> {
        let WebSocketMessage::Text(text) = message else {
            return Err(failure("expected text"));
        };
        self.socket
            .lock()
            .unwrap()
            .send(Message::Text(text.into()))
            .map_err(failure)
    }
    async fn next(&self) -> Result<Option<WebSocketMessage>, HostFailure> {
        match self.socket.lock().unwrap().read().map_err(failure)? {
            Message::Text(text) => Ok(Some(WebSocketMessage::Text(text.to_string()))),
            other => Err(failure(format!("unexpected frame {other:?}"))),
        }
    }
    async fn close(&self, _: Option<String>) -> Result<(), HostFailure> {
        self.socket.lock().unwrap().close(None).map_err(failure)
    }
}
struct CompatCell(Mutex<stravia_vendor_runtime::host::StateSnapshot>);
#[async_trait]
impl stravia_vendor_runtime::host::HostStateCell for CompatCell {
    async fn load(
        &self,
    ) -> Result<stravia_vendor_runtime::host::StateSnapshot, stravia_vendor_runtime::host::StateError>
    {
        Ok(self.0.lock().unwrap().clone())
    }
    async fn compare_exchange(
        &self,
        expected: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, stravia_vendor_runtime::host::StateError> {
        let mut snapshot = self.0.lock().unwrap();
        if snapshot.revision != expected {
            return Ok(false);
        }
        snapshot.revision += 1;
        snapshot.payload = payload;
        Ok(true)
    }
}
struct LiveServices {
    address: std::net::SocketAddr,
    client: reqwest::Client,
    cell: Arc<CompatCell>,
}
#[async_trait]
impl HostServices for LiveServices {
    fn http_start(
        &self,
        request: Request,
        options: Options,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        assert_eq!(
            options.iter().collect::<Vec<_>>(),
            vec![("http.version", "http1")]
        );
        assert_eq!(request.url, format!("http://{}/compat", self.address));
        let client = self.client.clone();
        let task = tokio::spawn(async move {
            let mut builder = client
                .request(request.method.parse().map_err(failure)?, request.url)
                .body(request.body);
            for field in request.headers.iter() {
                builder = builder.header(field.name.as_str(), field.bytes.as_slice());
            }
            let response = builder.send().await.map_err(failure)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| Field {
                    name: name.to_string(),
                    bytes: value.as_bytes().to_vec(),
                })
                .collect();
            let body = response.bytes().await.map_err(failure)?.to_vec();
            Ok(HttpData {
                status,
                headers,
                body,
            })
        });
        Ok(Arc::new(LiveResponse {
            task: AsyncMutex::new(Some(task)),
            data: OnceCell::new(),
            consumed: AsyncMutex::new(false),
        }))
    }
    async fn ws_connect(
        &self,
        request: ConnectRequest,
        options: Options,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        assert_eq!(
            options.iter().collect::<Vec<_>>(),
            vec![("http.version", "http1")]
        );
        assert_eq!(request.url, format!("ws://{}/socket", self.address));
        assert!(request.headers.iter().next().is_none());
        let stream = TcpStream::connect(self.address).map_err(failure)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(failure)?;
        let (socket, response) =
            tungstenite_deflate::client(request.url.as_str(), stream).map_err(failure)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| Field {
                name: name.to_string(),
                bytes: value.as_bytes().to_vec(),
            })
            .collect();
        Ok(Arc::new(LiveSocket {
            socket: Mutex::new(socket),
            status,
            headers,
        }))
    }
    async fn open_state(
        &self,
        scope: stravia_vendor_runtime::host::StateScope,
        key: String,
    ) -> Result<
        Arc<dyn stravia_vendor_runtime::host::HostStateCell>,
        stravia_vendor_runtime::host::StateError,
    > {
        assert!(matches!(
            scope,
            stravia_vendor_runtime::host::StateScope::Provider
        ));
        assert_eq!(key, "released");
        Ok(self.cell.clone())
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(None)
    }
    async fn write_private_state(&self, _: Vec<u8>) -> Result<(), HostFailure> {
        Err(failure("unexpected state write"))
    }
    async fn emit_event(&self, _: RuntimeEvent) -> Result<(), HostFailure> {
        Ok(())
    }
    fn log(&self, _: LogLevel, _: &str) {}
    fn generation_is_current(&self, generation: u64) -> bool {
        generation == 0
    }
}

#[tokio::test]
async fn released_net_guest_executes_http_and_websocket() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/vendor-test-fixtures/net-1.0-compat.wasm");
    let bytes = std::fs::read(fixture).expect("build vendor test fixtures first");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (http, _) = listener.accept().unwrap();
        http.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(http);
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert_eq!(first, "POST /compat HTTP/1.1\r\n");
        let mut length = None;
        let mut marker = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(!line.is_empty(), "truncated HTTP request");
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.trim_end().split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
            if name.eq_ignore_ascii_case("x-compat") {
                marker = value.trim() == "snapshot";
            }
        }
        let mut body = vec![0; length.expect("content length")];
        reader.read_exact(&mut body).unwrap();
        assert_eq!(body, b"released-http");
        assert!(marker);
        reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nX-Compat-Response: http-ok\r\nConnection: close\r\n\r\nhttp-response").unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut socket = tungstenite_deflate::accept(stream).unwrap();
        assert_eq!(
            socket.read().unwrap().into_text().unwrap().as_str(),
            "released-ws"
        );
        let response =
            serde_json::to_string(&AiResponse::new("released-ws-response", "compat-model"))
                .unwrap();
        socket.send(Message::Text(response.into())).unwrap();
        assert!(matches!(socket.read().unwrap(), Message::Close(_)));
    });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&bytes)
        .await
        .expect("load released WIT component");
    let provider = ProviderSnapshot {
        provider_id: "fixture.net-1-0-compat".into(),
        channel: "default".into(),
        base_url: format!("http://{address}"),
        protocol: "fixture-compat".into(),
        options: BTreeMap::from([(
            "wsUrl".into(),
            serde_json::json!(format!("ws://{address}/socket")),
        )]),
        credentials: BTreeMap::new(),
        model: None,
        model_metadata: None,
        client_headers: vec![],
        operation_metadata: BTreeMap::new(),
    };
    let input = OperationInput::Infer {
        provider,
        request: AiRequest::new("compat-model", vec![]),
    };
    let (operation, input) = input.encode_for_host().unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth();
    let client = reqwest::Client::builder()
        .no_proxy()
        .tls_backend_preconfigured(tls)
        .build()
        .unwrap();
    let scope = OperationScope::new(
        Arc::new(LiveServices {
            address,
            client,
            cell: Arc::new(CompatCell(Mutex::new(
                stravia_vendor_runtime::host::StateSnapshot {
                    revision: 0,
                    payload: None,
                },
            ))),
        }),
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(30)),
        0,
    );
    let output = runtime
        .execute(&plugin, "default", operation, input, scope)
        .await
        .expect("execute released WIT guest");
    let OperationOutput::Infer(response) = output else {
        panic!("expected inference response");
    };
    assert_eq!(response.id, "released-ws-response");
    server.join().unwrap();
}

use super::*;
use crate::db::models::CreateProviderRecord;
use crate::plugin::store::InstalledPlugin;
use crate::storage::{DynStorage, MemoryStorage};
use futures::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

const POLICY: &str = r#"{"domains":["cookie.test","other.cookie.test"],"names":["sid"]}"#;

struct LocalResolver(std::net::SocketAddr);
impl transport::Resolver for LocalResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        _: u16,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send + 'a>,
    > {
        Box::pin(async move {
            if matches!(host, "cookie.test" | "other.cookie.test") {
                Ok(vec![self.0])
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "nonlocal test host",
                ))
            }
        })
    }
}

struct Fixture {
    storage: DynStorage,
    network: VendorNetwork,
    requests: mpsc::UnboundedReceiver<String>,
    server: tokio::task::JoinHandle<()>,
    base: String,
    provider_id: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

async fn serve(
    socket: TcpStream,
    acceptor: TlsAcceptor,
    requests: mpsc::UnboundedSender<String>,
    port: u16,
) {
    let mut socket = acceptor.accept(socket).await.unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await.unwrap());
        assert!(bytes.len() < 32 * 1024);
    }
    let head = String::from_utf8(bytes).unwrap();
    let path = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    requests.send(head.clone()).unwrap();
    if path == "/socket" {
        let accept = tungstenite_deflate::handshake::derive_accept_key(
            header(&head, "sec-websocket-key").unwrap().as_bytes(),
        );
        socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\nSet-Cookie: sid=handshake; Path=/; Secure\r\n\r\n").as_bytes()).await.unwrap();
        let mut socket = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
            socket,
            tungstenite_deflate::protocol::Role::Server,
            None,
        )
        .await;
        while let Some(Ok(message)) = socket.next().await {
            if let tungstenite_deflate::Message::Text(text) = message {
                socket
                    .send(tungstenite_deflate::Message::Text(
                        format!("ack:{text}").into(),
                    ))
                    .await
                    .unwrap();
            }
        }
        return;
    }
    let (status, extra) = match path.as_str() {
        "/redirect" => (
            "307 Temporary Redirect",
            "Location: /after\r\nSet-Cookie: sid=redirect; Path=/; Secure\r\n".to_owned(),
        ),
        "/cross" => (
            "307 Temporary Redirect",
            format!(
                "Location: https://other.cookie.test:{port}/after\r\nSet-Cookie: sid=shared; Domain=cookie.test; Path=/; Secure\r\n"
            ),
        ),
        "/reject" => (
            "401 Unauthorized",
            "Set-Cookie: sid=rejected; Path=/; Secure\r\n".to_owned(),
        ),
        "/refresh" => (
            "200 OK",
            "Set-Cookie: sid=fresh; Path=/; Secure\r\n".to_owned(),
        ),
        _ => (
            "200 OK",
            "Set-Cookie: sid=http; Path=/; Secure\r\n".to_owned(),
        ),
    };
    socket
        .write_all(
            format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n{extra}\r\nok")
                .as_bytes(),
        )
        .await
        .unwrap();
    socket.shutdown().await.unwrap();
}

fn provider(name: &str) -> CreateProviderRecord {
    CreateProviderRecord {
        name: name.into(),
        vendor: Some("cookie-wire".into()),
        protocol: "openai-compatible".into(),
        base_url: "https://cookie.test".into(),
        preset_key: None,
        channel: Some("default".into()),
        models_source: None,
        static_models: None,
        api_key: String::new(),
        adapter_credentials: "{}".into(),
        vendor_options: "{}".into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
    }
}

async fn fixture() -> anyhow::Result<Fixture> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["cookie.test".into(), "other.cookie.test".into()])?;
    let crypto = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let key = rustls::pki_types::PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(
        signing_key.serialize_der(),
    ));
    let mut config = rustls::ServerConfig::builder_with_provider(crypto.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone())?;
    let verifier =
        rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), crypto)
            .build()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let port = address.port();
    let (tx, requests) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.unwrap();
                    connections.spawn(serve(socket, acceptor.clone(), tx.clone(), port));
                }
                joined = connections.join_next(), if !connections.is_empty() => { joined.unwrap().unwrap(); }
            }
        }
    });
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    store.attach_volatile_store(
        crate::plugin::volatile_state::VolatileStore::for_installation(
            &uuid::Uuid::new_v4().to_string(),
        ),
    );
    store
        .install(
            &InstalledPlugin {
                vendor_id: "cookie-wire".into(),
                version: "test".into(),
                source: "local".into(),
                descriptor: "{}".into(),
                component: Bytes::new(),
                digest: "test".into(),
                revision: 1,
                data_epoch: 1,
                installed_at: 1,
            },
            None,
            &[],
        )
        .await?;
    let provider = storage.providers().create(provider("first")).await?;
    let fence = store
        .open_atomic_provider_fence("cookie-wire", &provider.id, 1)
        .await?;
    let network = VendorNetwork::new(
        Arc::new(transport::VendorTransport::new()),
        super::VendorNetworkConfig {
            shapes: Arc::new(ShapeRegistry::builtin_with_verifier(verifier)?),
            resolver: Arc::new(LocalResolver(address)),
        },
        OutboundProxySettings::default(),
        NetworkPolicy::compile(["https", "wss"].into_iter().flat_map(|scheme| {
            ["cookie.test", "other.cookie.test"].map(move |host| {
                super::super::permissions::NetworkOrigin::Exact(format!("{scheme}://{host}:{port}"))
            })
        }))?,
        crate::plugin::lifecycle::VendorOperationTracker::default().begin("cookie-wire")?,
        CancellationToken::new(),
        "open-responses".into(),
    )
    .with_cookie_scope(
        store.clone(),
        "cookie-wire".into(),
        provider.id.clone(),
        1,
        fence,
    )
    .with_websocket_pool(
        Arc::new(VendorWebSocketPool::default()),
        "cookie-wire".into(),
        Some("conversation".into()),
    );
    Ok(Fixture {
        storage,
        network,
        requests,
        server,
        base: format!("https://cookie.test:{port}"),
        provider_id: provider.id,
    })
}

fn options() -> net::Options {
    let mut options = net::Options::new();
    options.http_version(net::HttpVersion::Http1).unwrap();
    options.set(net::keys::COOKIES, POLICY).unwrap();
    options
}
fn fields() -> net::Fields {
    let mut fields = net::Fields::new();
    fields.append("X-Before", b"one".to_vec());
    fields.slot("Cookie");
    fields.append("X-After", b"two".to_vec());
    fields
}
async fn get(network: &VendorNetwork, url: &str, fields: net::Fields) {
    let response = network
        .http_start(net::Request::new("GET", url).fields(fields), options())
        .unwrap();
    assert_eq!(response.status().await.unwrap(), 200);
    let mut body = Vec::new();
    while let Some(chunk) = response.read_body().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    assert_eq!(body, b"ok");
}
async fn captured(fixture: &mut Fixture) -> String {
    tokio::time::timeout(Duration::from_secs(5), fixture.requests.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn round_trip(socket: Arc<dyn HostWebSocket>, text: &str) {
    socket
        .send(WebSocketMessage::Text(text.into()))
        .await
        .unwrap();
    assert!(
        matches!(socket.next().await.unwrap(), Some(WebSocketMessage::Text(answer)) if answer == format!("ack:{text}"))
    );
    socket.close(None).await.unwrap();
}

#[tokio::test]
async fn cookies_share_http_redirects_rejected_upgrades_and_only_fresh_handshakes()
-> anyhow::Result<()> {
    let mut f = fixture().await?;
    get(&f.network, &format!("{}/redirect", f.base), fields()).await;
    assert_eq!(header(&captured(&mut f).await, "cookie"), None);
    let after = captured(&mut f).await;
    assert_eq!(header(&after, "cookie"), Some("sid=redirect"));
    assert!(!after.to_ascii_lowercase().contains("accept:"));
    assert!(after.find("X-Before:").unwrap() < after.find("Cookie:").unwrap());
    assert!(after.find("Cookie:").unwrap() < after.find("X-After:").unwrap());
    let ws = f.base.replacen("https:", "wss:", 1);
    let rejected = f
        .network
        .ws_connect(
            net::ConnectRequest::new(format!("{ws}/reject")).fields(fields()),
            options(),
        )
        .await;
    assert!(matches!(
        rejected,
        Err(HostFailure {
            upstream_status: Some(401),
            ..
        })
    ));
    assert_eq!(header(&captured(&mut f).await, "cookie"), Some("sid=http"));
    let socket = f
        .network
        .ws_connect(
            net::ConnectRequest::new(format!("{ws}/socket")).fields(fields()),
            options(),
        )
        .await
        .unwrap();
    assert!(socket.is_fresh_handshake());
    assert_eq!(
        header(&captured(&mut f).await, "cookie"),
        Some("sid=rejected")
    );
    round_trip(socket, "first").await;
    get(&f.network, &format!("{}/refresh", f.base), fields()).await;
    assert_eq!(
        header(&captured(&mut f).await, "cookie"),
        Some("sid=handshake")
    );
    let mut reuse = options();
    reuse.reuse_only(true)?;
    let reused = f
        .network
        .ws_connect(
            net::ConnectRequest::new(format!("{ws}/socket")).fields(fields()),
            reuse.clone(),
        )
        .await
        .unwrap();
    assert!(!reused.is_fresh_handshake());
    round_trip(reused, "reused").await;
    assert!(matches!(
        f.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    f.network
        .websocket_pool
        .as_ref()
        .unwrap()
        .idle
        .lock()
        .clear();
    let missing = f
        .network
        .ws_connect(
            net::ConnectRequest::new(format!("{ws}/socket")).fields(fields()),
            reuse,
        )
        .await;
    assert!(matches!(
        missing,
        Err(HostFailure {
            kind: ErrorKind::ContinuationUnavailable,
            ..
        })
    ));
    assert!(matches!(
        f.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let mut current = fields();
    current.append("X-Turn", b"current".to_vec());
    let fresh = f
        .network
        .ws_connect(
            net::ConnectRequest::new(format!("{ws}/socket")).fields(current),
            options(),
        )
        .await
        .unwrap();
    assert!(fresh.is_fresh_handshake());
    let head = captured(&mut f).await;
    assert_eq!(header(&head, "cookie"), Some("sid=fresh"));
    assert_eq!(header(&head, "x-turn"), Some("current"));
    round_trip(fresh, "fresh").await;
    Ok(())
}

#[tokio::test]
async fn cookies_preserve_cross_origin_credentials_and_provider_isolation() -> anyhow::Result<()> {
    let mut f = fixture().await?;
    let mut request_fields = fields();
    request_fields.append("Authorization", b"Bearer synthetic".to_vec());
    get(&f.network, &format!("{}/cross", f.base), request_fields).await;
    assert_eq!(
        header(&captured(&mut f).await, "authorization"),
        Some("Bearer synthetic")
    );
    let cross = captured(&mut f).await;
    assert_eq!(header(&cross, "cookie"), Some("sid=shared"));
    assert_eq!(header(&cross, "authorization"), None);
    let mut explicit = fields();
    explicit.append("cookie", b"sid=explicit".to_vec());
    get(&f.network, &format!("{}/after", f.base), explicit).await;
    assert_eq!(
        header(&captured(&mut f).await, "cookie"),
        Some("sid=explicit")
    );
    let second = f.storage.providers().create(provider("second")).await?;
    let store = f.storage.vendor_plugins();
    let fence = store
        .open_atomic_provider_fence("cookie-wire", &second.id, 1)
        .await?;
    let isolated = f.network.clone().with_cookie_scope(
        store.clone(),
        "cookie-wire".into(),
        second.id,
        1,
        fence,
    );
    get(&isolated, &format!("{}/after", f.base), fields()).await;
    assert_eq!(header(&captured(&mut f).await, "cookie"), None);
    f.storage.providers().delete(&f.provider_id).await?;
    let stale = f
        .network
        .http_start(
            net::Request::new("GET", format!("{}/after", f.base)).fields(fields()),
            options(),
        )
        .unwrap();
    assert!(matches!(
        stale.status().await,
        Err(HostFailure {
            kind: ErrorKind::Cancelled,
            ..
        })
    ));
    assert!(matches!(
        f.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

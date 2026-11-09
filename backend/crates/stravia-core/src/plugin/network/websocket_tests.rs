use super::*;
use crate::{
    outbound_proxy::OutboundProxySettings,
    plugin::network::transport::{
        H2Settings, HttpVersionPref, Route, SystemResolver, UpgradeResponse, VendorTransport,
    },
};
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;
use stravia_vendor_sdk::net::Field;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};
use tungstenite_deflate::Message as EngineMessage;
use url::Url;

type ServerSocket = tokio_tungstenite_deflate::WebSocketStream<TcpStream>;
const OFFER: &str = "permessage-deflate; client_max_window_bits";

async fn server(extension: Option<&'static str>) -> (Url, tokio::task::JoinHandle<ServerSocket>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("ws://{}/socket", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
            assert!(request.len() < 16 * 1024);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /socket HTTP/1.1\r\n"));
        let key = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .unwrap();
        let accept = derive_accept_key(key.as_bytes());
        let extensions = extension
            .map(|value| format!("Sec-WebSocket-Extensions: {value}\r\n"))
            .unwrap_or_default();
        stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n{extensions}\r\n").as_bytes()).await.unwrap();
        // Invalid negotiation tests never use the returned engine.
        let compression = extension.and_then(|value| validate_extension(value).ok());
        tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
            stream,
            Role::Server,
            Some(options(compression)),
        )
        .await
    });
    (url, task)
}

fn preferred() -> WsCompression {
    WsCompression::Preferred
}
fn required() -> WsCompression {
    WsCompression::Required
}
fn field(name: &str, bytes: &[u8]) -> Field {
    Field {
        name: name.into(),
        bytes: bytes.to_vec(),
    }
}

fn route() -> Route {
    let tls = Arc::new(
        ClientConfig::builder_with_provider(
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth(),
    );
    Route {
        shape_identity: "websocket-test".into(),
        tls_session_policy: crate::plugin::network::shape::SessionPolicy::Standard,
        tls: Arc::clone(&tls),
        proxy_tls: tls,
        proxy: OutboundProxySettings::default(),
        resolver: Arc::new(SystemResolver),
        version: HttpVersionPref::Http1,
        h2: H2Settings::default(),
        omit_h2_content_length: false,
    }
}

async fn finish(handshake: Handshake) -> Result<WebSocket, HostFailure> {
    match handshake
        .send(&VendorTransport::new(), route(), None)
        .await
        .unwrap()
    {
        UpgradeResponse::Upgraded(head, io) => handshake.finish(&head, io).await,
        UpgradeResponse::Rejected(_) => panic!("test server rejected upgrade"),
    }
}

async fn connect(url: &Url, compression: &WsCompression) -> Result<WebSocket, HostFailure> {
    let fields = if matches!(compression, WsCompression::Disabled) {
        vec![]
    } else {
        vec![field("Sec-WebSocket-Extensions", OFFER.as_bytes())]
    };
    finish(Handshake::new(url, fields, *compression).unwrap()).await
}

#[tokio::test]
async fn legacy_handshake_does_not_offer_compression_and_exchanges_application_bytes() {
    let (url, server) = server(None).await;
    let handshake = Handshake::new(
        &url,
        vec![field("Accept-Encoding", b"gzip")],
        WsCompression::Disabled,
    )
    .unwrap();
    assert!(
        !handshake
            .headers()
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("sec-websocket-extensions"))
    );
    assert!(
        handshake
            .headers()
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("accept-encoding") && f.bytes == b"gzip")
    );
    let mut client = finish(handshake).await.unwrap();
    let mut peer = server.await.unwrap();
    client.send(Message::Text("legacy".into())).await.unwrap();
    assert_eq!(
        peer.next().await.unwrap().unwrap(),
        EngineMessage::Text("legacy".into())
    );
    peer.send(EngineMessage::Text("reply".into()))
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Text(text) if text == "reply")
    );
}

#[tokio::test]
async fn handshake_preserves_guest_fields_and_immediately_buffered_websocket_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "ws://{}/socket?x=1",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
            assert!(request.len() < 16 * 1024);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /socket?x=1 HTTP/1.1\r\n"));
        assert!(request.contains("X-Trace: first\r\n"));
        assert!(request.contains("x-other: middle\r\n"));
        assert!(request.contains("X-Trace: second\r\n"));
        assert!(request.find("X-Trace: first").unwrap() < request.find("x-other: middle").unwrap());
        assert!(
            request.find("x-other: middle").unwrap() < request.find("X-Trace: second").unwrap()
        );
        assert!(!request.to_ascii_lowercase().contains("accept-encoding:"));
        let key = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .unwrap();
        let accept = derive_accept_key(key.as_bytes());
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\nX-Origin: peer\r\n\r\n"
        );
        let mut packet = response.into_bytes();
        packet.extend_from_slice(&[0x81, 0x05, b'h', b'e', b'l', b'l', b'o']);
        stream.write_all(&packet).await.unwrap();
        let mut socket = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
            stream,
            Role::Server,
            Some(options(None)),
        )
        .await;
        assert!(
            matches!(socket.next().await.unwrap().unwrap(), EngineMessage::Text(text) if text.as_str() == "reply")
        );
        request
    });
    let fields = vec![
        field("X-Trace", b"first"),
        field("x-other", b"middle"),
        field("X-Trace", b"second"),
    ];
    let handshake = Handshake::new(&url, fields, WsCompression::Disabled).unwrap();
    let (capture_tx, capture_rx) = tokio::sync::oneshot::channel();
    let (head, io) = match handshake
        .send(
            &VendorTransport::new(),
            route(),
            Some(Box::new(move |head| {
                let _ = capture_tx.send(head);
            })),
        )
        .await
        .unwrap()
    {
        UpgradeResponse::Upgraded(head, io) => (head, io),
        UpgradeResponse::Rejected(_) => panic!("test server rejected upgrade"),
    };
    assert_eq!(head.status, 101);
    assert!(
        head.fields
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("x-origin") && f.bytes == b"peer")
    );
    let mut client = handshake.finish(&head, io).await.unwrap();
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Text(text) if text == "hello")
    );
    client.send(Message::Text("reply".into())).await.unwrap();
    let request = server.await.unwrap();
    let capture = capture_rx.await.unwrap();
    for field in &capture.fields {
        let line = format!(
            "{}: {}\r\n",
            field.name,
            std::str::from_utf8(&field.bytes).unwrap()
        );
        assert!(
            request.contains(&line),
            "captured field missing on wire: {line:?}"
        );
    }
    let names: Vec<_> = capture
        .fields
        .iter()
        .filter(|f| {
            f.name.eq_ignore_ascii_case("x-trace") || f.name.eq_ignore_ascii_case("x-other")
        })
        .map(|f| (f.name.as_str(), f.bytes.as_slice()))
        .collect();
    assert_eq!(
        names,
        [
            ("X-Trace", &b"first"[..]),
            ("x-other", &b"middle"[..]),
            ("X-Trace", &b"second"[..])
        ]
    );
}

// Captures: codex 0.167.0 and Bun 1.3.14 loopback captures in
// /tmp/net1-capture-results/{codex-ws-default-final1,bun-ws-1.3.14-final1}.json.
// Redacted values are replaced with deterministic non-secret examples below.
// The other entries are representative header sets derived from client source,
// not claimed captures: tungstenite 0.28 client.rs::generate_request;
// OpenAI tungstenite fork 4fffad3 client.rs::request_with_compression;
// Python websockets client.py::ClientProtocol.connect; gorilla/websocket 1.5.3
// client.go::DialContext; undici 6.21.3 lib/web/websocket/connection.js
// (https://github.com/nodejs/undici/blob/v6.21.3/lib/web/websocket/connection.js).
// For source-derived cases, optional client/fetch headers are deliberately absent.
#[tokio::test]
async fn representative_client_handshakes_replay_ordered_fields_on_wire() {
    const BASIC: &[(&str, &str)] = &[
        ("Host", "$host"),
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", ""),
    ];
    const OPENAI: &[(&str, &str)] = &[
        ("Host", "$host"),
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", ""),
        (
            "sec-websocket-extensions",
            "permessage-deflate; client_max_window_bits",
        ),
    ];
    const BUN: &[(&str, &str)] = &[
        ("Host", "$host"),
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        (
            "Sec-WebSocket-Extensions",
            "permessage-deflate; client_max_window_bits",
        ),
        ("Sec-WebSocket-Key", ""),
    ];
    const CODEX: &[(&str, &str)] = &[
        ("Host", "$host"),
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", ""),
        ("authorization", "Bearer fixture"),
        (
            "user-agent",
            "codex_exec/0.167.0 (NixOS 26.5.0; x86_64) dumb (codex_exec; 0.167.0)",
        ),
        ("originator", "codex_exec"),
        ("openai-beta", "responses_websockets=2026-02-06"),
        ("x-codex-turn-metadata", "fixture"),
        ("x-codex-beta-features", "remote_compaction_v2"),
        ("x-client-request-id", "fixture"),
        ("session-id", "fixture"),
        ("thread-id", "fixture"),
        ("x-codex-window-id", "fixture"),
        (
            "sec-websocket-extensions",
            "permessage-deflate; client_max_window_bits",
        ),
    ];
    const PYTHON: &[(&str, &str)] = &[
        ("Host", "$host"),
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Key", ""),
        ("Sec-WebSocket-Version", "13"),
    ];
    // Go net/http writes Host first and sorts the remaining header names.
    const GORILLA: &[(&str, &str)] = &[
        ("Host", "$host"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Key", ""),
        ("Sec-WebSocket-Version", "13"),
        ("Upgrade", "websocket"),
    ];
    // Undici adds its transport-owned Upgrade and Connection separately; this
    // representative ordered input exercises the source's key/version/offer.
    const UNDICI: &[(&str, &str)] = &[
        ("Host", "$host"),
        ("Connection", "Upgrade"),
        ("Upgrade", "websocket"),
        ("sec-websocket-key", ""),
        ("sec-websocket-version", "13"),
        (
            "sec-websocket-extensions",
            "permessage-deflate; client_max_window_bits",
        ),
    ];
    for (name, fixture) in [
        ("tokio-tungstenite-0.28", BASIC),
        ("openai-fork", OPENAI),
        ("codex", CODEX),
        ("bun", BUN),
        ("undici", UNDICI),
        ("python-websockets", PYTHON),
        ("gorilla", GORILLA),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = Url::parse(&format!("ws://{address}/socket")).unwrap();
        let authority = address.to_string();
        let fields: Vec<_> = fixture
            .iter()
            .map(|(name, value)| {
                field(
                    name,
                    if *value == "$host" {
                        authority.as_bytes()
                    } else {
                        value.as_bytes()
                    },
                )
            })
            .collect();
        let compression = if fields
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("sec-websocket-extensions"))
        {
            WsCompression::Preferred
        } else {
            WsCompression::Disabled
        };
        let handshake = Handshake::new(&url, fields, compression).unwrap();
        let key = handshake
            .headers()
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case("sec-websocket-key"))
            .unwrap()
            .bytes
            .clone();
        let expected: Vec<_> = fixture
            .iter()
            .map(|(name, value)| {
                let bytes = if name.eq_ignore_ascii_case("sec-websocket-key") {
                    key.clone()
                } else if *value == "$host" {
                    authority.as_bytes().to_vec()
                } else {
                    value.as_bytes().to_vec()
                };
                ((*name).to_owned(), bytes)
            })
            .collect();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(socket.read_u8().await.unwrap());
                assert!(bytes.len() < 16 * 1024);
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.starts_with("GET /socket HTTP/1.1\r\n"));
            let fields: Vec<_> = request
                .lines()
                .skip(1)
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let (name, value) = line.split_once(": ").unwrap();
                    (name.to_owned(), value.as_bytes().to_vec())
                })
                .collect();
            assert_eq!(fields, expected, "{name}");
            let key = fields
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("sec-websocket-key"))
                .unwrap();
            let accept = derive_accept_key(&key.1);
            socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
        });
        let _client = finish(handshake).await.unwrap();
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn http_proxy_forwards_websocket_upgrade_and_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
            assert!(request.len() < 16 * 1024);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET http://websocket.example.invalid/socket HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic dxnlcjpwyxnz\r\n")
        );
        let key = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .unwrap();
        let accept = derive_accept_key(key.as_bytes());
        stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
        let mut socket = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
            stream,
            Role::Server,
            Some(options(None)),
        )
        .await;
        assert!(
            matches!(socket.next().await.unwrap().unwrap(), EngineMessage::Text(text) if text.as_str() == "through http proxy")
        );
        socket
            .send(EngineMessage::Text("http proxy reply".into()))
            .await
            .unwrap();
    });
    let url = Url::parse("ws://websocket.example.invalid/socket").unwrap();
    let handshake = Handshake::new(&url, vec![], WsCompression::Disabled).unwrap();
    let mut proxy_route = route();
    proxy_route.proxy.url = format!("http://user:pass@{proxy}");
    let (head, io) = match handshake
        .send(&VendorTransport::new(), proxy_route, None)
        .await
        .unwrap()
    {
        UpgradeResponse::Upgraded(head, io) => (head, io),
        UpgradeResponse::Rejected(_) => panic!("proxy rejected upgrade"),
    };
    let mut client = handshake.finish(&head, io).await.unwrap();
    client
        .send(Message::Text("through http proxy".into()))
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Text(text) if text == "http proxy reply")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn socks5h_proxy_carries_websocket_handshake_and_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut greeting = [0; 3];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 1, 0]);
        stream.write_all(&[5, 0]).await.unwrap();
        let mut prefix = [0; 5];
        stream.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix[..4], &[5, 1, 0, 3]);
        let mut host = vec![0; prefix[4] as usize];
        stream.read_exact(&mut host).await.unwrap();
        assert_eq!(host, b"websocket.example.invalid");
        let mut port = [0; 2];
        stream.read_exact(&mut port).await.unwrap();
        assert_eq!(u16::from_be_bytes(port), 80);
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
            assert!(request.len() < 16 * 1024);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /socket HTTP/1.1\r\n"));
        let key = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .unwrap();
        let accept = derive_accept_key(key.as_bytes());
        stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
        let mut socket = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
            stream,
            Role::Server,
            Some(options(None)),
        )
        .await;
        assert!(
            matches!(socket.next().await.unwrap().unwrap(), EngineMessage::Text(text) if text.as_str() == "through proxy")
        );
        socket
            .send(EngineMessage::Text("proxied reply".into()))
            .await
            .unwrap();
    });
    let url = Url::parse("ws://websocket.example.invalid/socket").unwrap();
    let handshake = Handshake::new(&url, vec![], WsCompression::Disabled).unwrap();
    let mut proxy_route = route();
    proxy_route.proxy.url = format!("socks5h://{proxy}");
    let (head, io) = match handshake
        .send(&VendorTransport::new(), proxy_route, None)
        .await
        .unwrap()
    {
        UpgradeResponse::Upgraded(head, io) => (head, io),
        UpgradeResponse::Rejected(_) => panic!("proxy rejected upgrade"),
    };
    let mut client = handshake.finish(&head, io).await.unwrap();
    client
        .send(Message::Text("through proxy".into()))
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Text(text) if text == "proxied reply")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn negotiated_compression_round_trips_both_directions_and_retains_dictionary_state() {
    for extension in [
        "permessage-deflate",
        "permessage-deflate; client_no_context_takeover; server_no_context_takeover",
        "permessage-deflate; client_max_window_bits=12; server_max_window_bits=12",
    ] {
        let (url, server) = server(Some(extension)).await;
        let mut client = connect(&url, &required()).await.unwrap();
        let mut peer = server.await.unwrap();
        assert!(client.compressed());
        for _ in 0..3 {
            let payload = "压缩-message-".repeat(4096);
            client.send(Message::Text(payload.clone())).await.unwrap();
            assert_eq!(
                peer.next()
                    .await
                    .unwrap()
                    .unwrap()
                    .into_text()
                    .unwrap()
                    .as_str(),
                payload
            );
            let bytes = Bytes::from(vec![0xab; 128 * 1024]);
            peer.send(EngineMessage::Binary(bytes.clone()))
                .await
                .unwrap();
            assert!(
                matches!(client.next().await.unwrap().unwrap(), Message::Binary(actual) if actual == bytes)
            );
        }
    }
}

#[tokio::test]
async fn preferred_absent_extension_keeps_original_connection_and_required_refuses_it() {
    let (url, server) = server(None).await;
    let mut client = connect(&url, &preferred()).await.unwrap();
    let mut peer = server.await.unwrap();
    assert!(!client.compressed());
    client
        .send(Message::Text("same connection".into()))
        .await
        .unwrap();
    assert_eq!(
        peer.next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap()
            .as_str(),
        "same connection"
    );

    let (url, server) = self::server(None).await;
    assert!(connect(&url, &required()).await.is_err());
    let mut peer = server.await.unwrap();
    // No business data is ever sent on a failed required handshake.
    assert!(peer.next().await.is_none_or(|result| result.is_err()));
}

#[tokio::test]
async fn malformed_unoffered_and_duplicate_extensions_fail_before_business_send() {
    for extension in [
        "permessage-deflate; unknown=1",
        "permessage-deflate; server_no_context_takeover=1",
        "permessage-deflate; server_no_context_takeover; server_no_context_takeover",
        "permessage-deflate; server_max_window_bits",
        "permessage-deflate; client_max_window_bits",
        "permessage-deflate; client_max_window_bits=7",
        "permessage-deflate; client_max_window_bits=16",
        "permessage-deflate; client_max_window_bits=012",
        "permessage-deflate; server_max_window_bits=\"09\"",
        "permessage-deflate; client_max_window_bits_xyz=15",
        "permessage-deflate; client_max_window_bits=15=garbage",
        "permessage-deflate, permessage-deflate",
        "some-other-extension",
    ] {
        let (url, server) = server(Some(extension)).await;
        assert!(connect(&url, &preferred()).await.is_err(), "{extension}");
        drop(server.await.unwrap());
    }
    let (url, server) = server(Some("permessage-deflate")).await;
    assert!(connect(&url, &WsCompression::Disabled).await.is_err());
    drop(server.await.unwrap());
}

#[test]
fn handshake_rejects_duplicate_owned_slots_and_nonempty_client_key() {
    let url = Url::parse("ws://localhost/socket").unwrap();
    for name in [
        "Connection",
        "Upgrade",
        "Sec-WebSocket-Key",
        "Sec-WebSocket-Version",
        "Sec-WebSocket-Extensions",
        "Sec-WebSocket-Protocol",
    ] {
        let fields = vec![field(name, b""), field(&name.to_ascii_lowercase(), b"")];
        assert!(
            Handshake::new(&url, fields, WsCompression::Preferred).is_err(),
            "{name}"
        );
    }
    assert!(
        Handshake::new(
            &url,
            vec![field("Sec-WebSocket-Key", b"guest-key")],
            WsCompression::Disabled
        )
        .is_err()
    );
    let handshake = Handshake::new(
        &url,
        vec![
            field("X-Before", b"a"),
            field("sEc-WebSocket-Key", b""),
            field("X-After", b"b"),
        ],
        WsCompression::Disabled,
    )
    .unwrap();
    let fields = handshake.headers();
    assert_eq!(fields[0].name, "X-Before");
    assert_eq!(fields[1].name, "sEc-WebSocket-Key");
    assert!(!fields[1].bytes.is_empty());
    assert_eq!(fields[2].name, "X-After");
}

#[tokio::test]
async fn selected_client_window_cannot_exceed_the_offered_limit() {
    let (url, first_server) = server(Some("permessage-deflate; client_max_window_bits=13")).await;
    let handshake = Handshake::new(
        &url,
        vec![field(
            "Sec-WebSocket-Extensions",
            b"permessage-deflate; client_max_window_bits=12",
        )],
        WsCompression::Preferred,
    )
    .unwrap();
    assert!(finish(handshake).await.is_err());
    drop(first_server.await.unwrap());
    let (url, server) = server(Some("permessage-deflate; client_max_window_bits=12")).await;
    let handshake = Handshake::new(
        &url,
        vec![field(
            "Sec-WebSocket-Extensions",
            b"permessage-deflate; client_max_window_bits=12",
        )],
        WsCompression::Preferred,
    )
    .unwrap();
    let client = finish(handshake).await.unwrap();
    assert!(client.compressed());
    drop(client);
    drop(server.await.unwrap());
}

#[test]
fn quoted_window_bits_and_context_flags_map_to_actual_engine_configuration() {
    let config = validate_extension("permessage-deflate; client_max_window_bits=\"12\"; server_max_window_bits=10; client_no_context_takeover").unwrap();
    assert_eq!(config.client_max_window_bits().get(), 12);
    assert_eq!(config.server_max_window_bits().get(), 10);
    assert!(config.client_no_context_takeover);
    assert!(!config.server_no_context_takeover);
}

#[test]
fn response_integrity_rejects_nonce_protocol_and_duplicate_accept() {
    let handshake = Handshake::new(
        &Url::parse("ws://localhost/socket").unwrap(),
        vec![
            field("Sec-WebSocket-Protocol", b"expected"),
            field("Sec-WebSocket-Extensions", OFFER.as_bytes()),
        ],
        preferred(),
    )
    .unwrap();
    let mut head = crate::plugin::network::transport::Head {
        status: 101,
        fields: vec![
            field("Connection", b"Upgrade"),
            field("Upgrade", b"websocket"),
            field("Sec-WebSocket-Accept", handshake.accept.as_bytes()),
        ],
    };
    assert!(handshake.validate(&head).is_ok());
    head.status = 403;
    assert!(handshake.validate(&head).is_err());
    head.status = 101;
    head.fields
        .push(field("Sec-WebSocket-Protocol", b"unoffered"));
    assert!(handshake.validate(&head).is_err());
    head.fields.pop();
    head.fields
        .push(field("Sec-WebSocket-Protocol", b"expected"));
    assert!(handshake.validate(&head).is_ok());
    head.fields.pop();
    head.fields.push(field("Sec-WebSocket-Version", b"12"));
    assert!(handshake.validate(&head).is_err());
    head.fields.pop();
    head.fields.push(field("Sec-WebSocket-Accept", b"wrong"));
    assert!(handshake.validate(&head).is_err());
}

#[tokio::test]
async fn fragmented_message_allows_interleaved_uncompressed_ping() {
    use tungstenite_deflate::protocol::frame::{
        Frame,
        coding::{Data, OpCode},
    };
    let (url, server) = server(Some("permessage-deflate")).await;
    let mut client = connect(&url, &preferred()).await.unwrap();
    let mut peer = server.await.unwrap();
    peer.send(EngineMessage::Frame(Frame::message(
        b"hel".to_vec(),
        OpCode::Data(Data::Text),
        false,
    )))
    .await
    .unwrap();
    peer.send(EngineMessage::Ping(Bytes::from_static(b"probe")))
        .await
        .unwrap();
    peer.send(EngineMessage::Frame(Frame::message(
        b"lo".to_vec(),
        OpCode::Data(Data::Continue),
        true,
    )))
    .await
    .unwrap();
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Ping(bytes) if bytes == b"probe"[..])
    );
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Text(text) if text == "hello")
    );
    client.socket.flush().await.unwrap();
    assert!(
        matches!(peer.next().await.unwrap().unwrap(), EngineMessage::Pong(bytes) if bytes == b"probe"[..])
    );
}

#[tokio::test]
async fn rfc7692_compressed_fragments_allow_interleaved_control_frames() {
    let (url, server) = server(Some("permessage-deflate")).await;
    let mut client = connect(&url, &required()).await.unwrap();
    let mut peer = server.await.unwrap();
    // RFC 7692 section 7.2.3.1's "Hello" example, split at 3 compressed
    // octets, with an uncompressed Ping between the two data fragments.
    peer.get_mut()
        .write_all(&[
            0x41, 0x03, 0xf2, 0x48, 0xcd, 0x89, 0x01, b'?', 0x80, 0x04, 0xc9, 0xc9, 0x07, 0x00,
        ])
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Ping(bytes) if bytes == b"?"[..])
    );
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Text(text) if text == "Hello")
    );
    client.socket.flush().await.unwrap();
    assert!(
        matches!(peer.next().await.unwrap().unwrap(), EngineMessage::Pong(bytes) if bytes == b"?"[..])
    );
}

#[tokio::test]
async fn malformed_frames_fail_strictly_and_end_the_socket() {
    for bytes in [
        &[0xc9, 0][..], // Control frames cannot carry RSV1, even with compression negotiated.
        &[0x81, 1, 0xff][..], // Text must be UTF-8.
        &[0x88, 1, 0][..], // A close status cannot contain only one byte.
        &[0xc2, 3, 0xff, 0xff, 0xff][..], // Corrupt deflate input.
    ] {
        let (url, server) = server(Some("permessage-deflate")).await;
        let mut client = connect(&url, &preferred()).await.unwrap();
        let mut peer = server.await.unwrap();
        peer.get_mut().write_all(bytes).await.unwrap();
        assert!(client.next().await.unwrap().is_err(), "{bytes:?}");
        assert!(client.next().await.is_none());
    }
}

#[tokio::test]
async fn compressed_frame_without_negotiation_is_rejected() {
    let (url, server) = server(None).await;
    let mut client = connect(&url, &WsCompression::Disabled).await.unwrap();
    let mut peer = server.await.unwrap();
    peer.get_mut()
        .write_all(&[0xc1, 0x07, 0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00])
        .await
        .unwrap();
    assert!(client.next().await.unwrap().is_err());
}

#[tokio::test]
async fn large_messages_remain_supported_and_inflation_enforces_its_budget() {
    let (url, server) = server(Some("permessage-deflate")).await;
    let mut client = connect(&url, &required()).await.unwrap();
    let mut peer = server.await.unwrap();
    let payload = Bytes::from(vec![b'x'; 17 * 1024 * 1024]);
    peer.send(EngineMessage::Binary(payload.clone()))
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap().unwrap(), Message::Binary(actual) if actual == payload)
    );

    // Exercise the same production configuration with a reduced test budget;
    // the malicious expansion must fail in the engine, not in a post-read check.
    let (left, right) = tokio::io::duplex(64 * 1024);
    let mut config = options(Some(DeflateConfig::new()));
    config.max_message_size = Some(1024);
    let mut receiver = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
        left,
        Role::Client,
        Some(config),
    )
    .await;
    let mut sender = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
        right,
        Role::Server,
        Some(options(Some(DeflateConfig::new()))),
    )
    .await;
    sender
        .send(EngineMessage::Binary(Bytes::from(vec![0; 128 * 1024])))
        .await
        .unwrap();
    assert!(receiver.next().await.unwrap().is_err());
}

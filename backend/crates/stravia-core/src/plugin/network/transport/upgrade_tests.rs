//! End-to-end upgrade regressions against loopback HTTP/1 and HTTP/2 peers.

use std::{io::Cursor, ops::ControlFlow, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use rama_http_types::proto::h2::hpack::Decoder;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

use super::{
    Field, H2Settings, HttpVersionPref, RequestPlan, Route, SystemResolver, TransportError,
    UpgradeResponse, VendorTransport,
};
use crate::outbound_proxy::OutboundProxySettings;

fn route(version: HttpVersionPref) -> Route {
    let tls = Arc::new(super::test_support::untrusted_tls());
    Route {
        shape_identity: "upgrade-test".into(),
        tls_session_policy: crate::plugin::network::shape::SessionPolicy::Standard,
        tls: Arc::clone(&tls),
        proxy_tls: tls,
        proxy: OutboundProxySettings::default(),
        resolver: Arc::new(SystemResolver),
        version,
        h2: H2Settings::default(),
        omit_h2_content_length: false,
    }
}

fn plan(uri: String) -> RequestPlan {
    RequestPlan {
        method: "GET".into(),
        uri,
        fields: vec![
            Field {
                name: "Connection".into(),
                bytes: b"Upgrade".to_vec(),
            },
            Field {
                name: "Upgrade".into(),
                bytes: b"websocket".to_vec(),
            },
            Field {
                name: "Sec-WebSocket-Key".into(),
                bytes: b"dGhlIHNhbXBsZSBub25jZQ==".to_vec(),
            },
        ],
        body: Bytes::new(),
        observer: None,
    }
}

#[tokio::test]
async fn http1_upgrade_preserves_bytes_read_with_response_and_supports_writes() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = super::test_support::read_headers(&mut socket).await;
            assert!(request.starts_with(b"GET /socket HTTP/1.1\r\n"));
            // One write deliberately puts the first upgraded bytes immediately after the head.
            socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nserver-first").await.unwrap();
            let mut client_data = [0; 12];
            socket.read_exact(&mut client_data).await.unwrap();
            assert_eq!(&client_data, b"client-reply");
        });

        let UpgradeResponse::Upgraded(head, mut io) = VendorTransport::new()
            .upgrade(plan(format!("ws://{address}/socket")), route(HttpVersionPref::Http1))
            .await.unwrap() else { panic!("expected a successful HTTP/1 upgrade") };
        assert_eq!(head.status, 101);
        let mut first = [0; 12];
        io.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"server-first");
        io.write_all(b"client-reply").await.unwrap();
        io.flush().await.unwrap();
        server.await.unwrap();
    }).await.expect("HTTP/1 upgrade timed out");
}

#[tokio::test]
async fn http1_upgrade_rejection_preserves_status_ordered_headers_and_body() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = super::test_support::read_headers(&mut socket).await;
            assert!(request.starts_with(b"GET /socket HTTP/1.1\r\n"));
            socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Basic realm=first\r\nx-reason: denied\r\nwww-authenticate: Bearer realm=second\r\ncontent-length: 13\r\n\r\nauth required").await.unwrap();
        });

        let UpgradeResponse::Rejected(mut response) = VendorTransport::new()
            .upgrade(plan(format!("ws://{address}/socket")), route(HttpVersionPref::Http1))
            .await.unwrap() else { panic!("expected an HTTP/1 upgrade rejection") };
        assert_eq!(response.head.status, 401);
        let fields: Vec<_> = response.head.fields.iter()
            .map(|field| (field.name.as_str(), field.bytes.as_slice()))
            .collect();
        assert_eq!(fields, vec![
            ("www-authenticate", b"Basic realm=first".as_slice()),
            ("x-reason", b"denied".as_slice()),
            ("www-authenticate", b"Bearer realm=second".as_slice()),
            ("content-length", b"13".as_slice()),
        ]);
        let mut body = Vec::new();
        while let Some(chunk) = response.body.next_chunk().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"auth required");
        server.await.unwrap();
    }).await.expect("HTTP/1 upgrade rejection timed out");
}

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

async fn read_frame(socket: &mut TcpStream) -> Frame {
    let mut header = [0; 9];
    socket.read_exact(&mut header).await.unwrap();
    let len =
        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
    let mut payload = vec![0; len];
    socket.read_exact(&mut payload).await.unwrap();
    Frame {
        kind: header[3],
        flags: header[4],
        stream: u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fff_ffff,
        payload,
    }
}

async fn h2_preface(socket: &mut TcpStream, enable_connect: bool) {
    let mut preface = [0; 24];
    socket.read_exact(&mut preface).await.unwrap();
    assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    if enable_connect {
        // SETTINGS_ENABLE_CONNECT_PROTOCOL (0x8) = 1, RFC 8441.
        socket
            .write_all(&[0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 1])
            .await
            .unwrap();
    } else {
        socket
            .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn h2_extended_connect_negotiates_and_transfers_both_directions() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            h2_preface(&mut socket, true).await;
            let (stream, mut headers) = loop {
                let frame = read_frame(&mut socket).await;
                if frame.kind == 1 {
                    assert_ne!(frame.stream, 0);
                    assert_eq!(
                        frame.flags & 1,
                        0,
                        "CONNECT request must leave its stream open"
                    );
                    assert_eq!(frame.flags & 0x28, 0, "unexpected padding or priority");
                    let mut headers = BytesMut::from(frame.payload.as_slice());
                    let mut flags = frame.flags;
                    while flags & 4 == 0 {
                        let next = read_frame(&mut socket).await;
                        assert_eq!((next.kind, next.stream), (9, frame.stream));
                        headers.extend_from_slice(&next.payload);
                        flags = next.flags;
                    }
                    break (frame.stream, headers);
                }
            };
            let mut fields = Vec::new();
            Decoder::new(4096)
                .decode(&mut Cursor::new(&mut headers), |header| {
                    fields.push((
                        header.name().as_slice().to_vec(),
                        header.value_slice().to_vec(),
                    ));
                    ControlFlow::Continue(())
                })
                .unwrap();
            assert!(
                fields
                    .iter()
                    .any(|(name, value)| name == b":method" && value == b"CONNECT")
            );
            assert!(
                fields
                    .iter()
                    .any(|(name, value)| name == b":protocol" && value == b"websocket")
            );
            assert!(
                fields
                    .iter()
                    .any(|(name, value)| name == b":path" && value == b"/socket")
            );
            assert!(!fields.iter().any(|(name, _)| name == b"sec-websocket-key"));

            // HEADERS with indexed :status 200, then DATA without END_STREAM.
            let id = stream.to_be_bytes();
            socket
                .write_all(&[0, 0, 1, 1, 4, id[0], id[1], id[2], id[3], 0x88])
                .await
                .unwrap();
            socket
                .write_all(&[0, 0, 12, 0, 0, id[0], id[1], id[2], id[3]])
                .await
                .unwrap();
            socket.write_all(b"server-first").await.unwrap();
            let mut data = Vec::new();
            while data.len() < 12 {
                let frame = read_frame(&mut socket).await;
                if frame.kind == 0 && frame.stream == stream {
                    data.extend_from_slice(&frame.payload);
                }
            }
            assert_eq!(data, b"client-reply");
        });

        let UpgradeResponse::Upgraded(head, mut io) = VendorTransport::new()
            .upgrade(
                plan(format!("http://{address}/socket")),
                route(HttpVersionPref::Http2),
            )
            .await
            .unwrap()
        else {
            panic!("expected a successful HTTP/2 upgrade")
        };
        assert_eq!(head.status, 200);
        let mut first = [0; 12];
        io.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"server-first");
        io.write_all(b"client-reply").await.unwrap();
        io.flush().await.unwrap();
        server.await.unwrap();
    })
    .await
    .expect("HTTP/2 extended CONNECT timed out");
}

#[tokio::test]
async fn h2_upgrade_rejects_peer_without_extended_connect_setting() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (done, mut finished) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            h2_preface(&mut socket, false).await;
            loop {
                tokio::select! {
                    result = &mut finished => { result.unwrap(); break; }
                    frame = read_frame(&mut socket) => {
                        assert_ne!(frame.kind, 1, "CONNECT must not be sent without negotiation");
                    }
                }
            }
        });
        let transport = VendorTransport::new();
        let result = transport.upgrade(plan(format!("http://{address}/socket")), route(HttpVersionPref::Http2)).await;
        assert!(matches!(result, Err(TransportError::Protocol(message)) if message.contains("does not enable extended CONNECT")));
        done.send(()).unwrap();
        server.await.unwrap();
    }).await.expect("HTTP/2 extended CONNECT rejection timed out");
}

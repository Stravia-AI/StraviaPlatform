//! End-to-end WebSocket frames over a TLS-negotiated HTTP/2 extended CONNECT.

use std::{io::Cursor, ops::ControlFlow, sync::Arc, time::Duration};

use bytes::BytesMut;
use rama_http_types::proto::h2::hpack::Decoder;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use url::Url;

use super::super::transport::{
    H2Settings, HttpVersionPref, Route, SystemResolver, UpgradeResponse, VendorTransport,
};
use super::{Handshake, Message};
use crate::outbound_proxy::OutboundProxySettings;
use stravia_vendor_sdk::net::{Field, WsCompression};

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

async fn read_frame(socket: &mut (impl AsyncRead + Unpin)) -> Frame {
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

#[tokio::test]
async fn tls_h2_extended_connect_exchanges_websocket_frames() {
    timeout(Duration::from_secs(10), async {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let mut client_tls = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions().unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"h2".to_vec()];
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let mut server_tls = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions().unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key).unwrap();
        server_tls.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server_tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (done, finished) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = acceptor.accept(socket).await.unwrap();
            assert_eq!(socket.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            let mut preface = [0; 24];
            socket.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            // SETTINGS_ENABLE_CONNECT_PROTOCOL = 1 (RFC 8441).
            socket.write_all(&[0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 1]).await.unwrap();
            let (stream, mut headers) = loop {
                let frame = read_frame(&mut socket).await;
                if frame.kind == 1 {
                    assert_ne!(frame.stream, 0);
                    assert_eq!(frame.flags & 1, 0, "CONNECT must leave the stream open");
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
            Decoder::new(4096).decode(&mut Cursor::new(&mut headers), |header| {
                fields.push((header.name().as_slice().to_vec(), header.value_slice().to_vec()));
                ControlFlow::Continue(())
            }).unwrap();
            for (name, value) in [
                (b":method".as_slice(), b"CONNECT".as_slice()),
                (b":protocol".as_slice(), b"websocket".as_slice()),
                (b":scheme".as_slice(), b"https".as_slice()),
                (b":path".as_slice(), b"/socket?test=1".as_slice()),
                (b"sec-websocket-version".as_slice(), b"13".as_slice()),
            ] {
                assert!(fields.iter().any(|(n, v)| n == name && v == value), "missing {name:?}: {value:?}");
            }
            assert!(fields.iter().any(|(n, v)| n == b":authority" && v == address.to_string().as_bytes()));
            assert!(!fields.iter().any(|(n, _)| n == b"connection" || n == b"upgrade"));

            // Indexed :status 200; keep the HTTP/2 stream open for WebSocket frames.
            let id = stream.to_be_bytes();
            socket.write_all(&[0, 0, 1, 1, 4, id[0], id[1], id[2], id[3], 0x88]).await.unwrap();
            let mut data = Vec::new();
            while data.len() < 6 + b"client message".len() {
                let frame = read_frame(&mut socket).await;
                if frame.kind == 0 && frame.stream == stream {
                    assert_eq!(frame.flags & 1, 0);
                    data.extend_from_slice(&frame.payload);
                }
            }
            // Client-to-server WebSocket frames must be masked, even over H2.
            assert_eq!(data[0], 0x81);
            assert_eq!(data[1], 0x80 | b"client message".len() as u8);
            assert_eq!(data.len(), 6 + b"client message".len());
            let decoded: Vec<u8> = data[6..].iter().enumerate().map(|(i, byte)| byte ^ data[2 + i % 4]).collect();
            assert_eq!(decoded, b"client message");
            // An unmasked server text frame inside an HTTP/2 DATA frame.
            let payload = b"\x81\x0eserver message";
            socket.write_all(&[0, 0, payload.len() as u8, 0, 0, id[0], id[1], id[2], id[3]]).await.unwrap();
            socket.write_all(payload).await.unwrap();
            socket.flush().await.unwrap();
            finished.await.unwrap();
        });

        let tls = Arc::new(client_tls);
        let route = Route {
            shape_identity: "websocket-h2-tls-test".into(),
            tls_session_policy: crate::plugin::network::shape::SessionPolicy::Standard,
            tls: Arc::clone(&tls),
            proxy_tls: tls,
            proxy: OutboundProxySettings::default(),
            resolver: Arc::new(SystemResolver),
            version: HttpVersionPref::Http2,
            h2: H2Settings::default(),
            omit_h2_content_length: false,
        };
        let url = Url::parse(&format!("wss://{address}/socket?test=1")).unwrap();
        let handshake = Handshake::new(&url, vec![Field {
            name: "Host".into(), bytes: address.to_string().into_bytes(),
        }], WsCompression::Disabled).unwrap();
        let transport = VendorTransport::new();
        let (head, io) = match handshake.send(&transport, route, None).await.unwrap() {
            UpgradeResponse::Upgraded(head, io) => (head, io),
            UpgradeResponse::Rejected(response) => panic!("extended CONNECT rejected: {}", response.head.status),
        };
        assert_eq!(head.status, 200);
        let mut client = handshake.finish(&head, io).await.unwrap();
        client.send(Message::Text("client message".into())).await.unwrap();
        assert!(matches!(client.next().await.unwrap().unwrap(), Message::Text(text) if text == "server message"));
        done.send(()).unwrap();
        server.await.unwrap();
    }).await.expect("TLS HTTP/2 WebSocket exchange timed out");
}

use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn antigravity_two_transport_handshakes_preserve_disabled_resumption() {
    use super::{H2Settings, HttpVersionPref, RequestPlan, Route, SystemResolver, VendorTransport};
    use crate::{outbound_proxy::OutboundProxySettings, plugin::network::shape::ShapeRegistry};
    use bytes::Bytes;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{
        RootCertStore, ServerConfig,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    };
    use tokio_rustls::TlsAcceptor;

    timeout(Duration::from_secs(10), async {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .unwrap();
        let registry = ShapeRegistry::builtin_with_verifier(verifier).unwrap();
        let shape = registry.get("antigravity").unwrap();
        let route = Route {
            shape_identity: Arc::clone(&shape.identity),
            tls: shape.tls_for("direct", false),
            tls_session_policy: shape.session_policy(false),
            proxy_tls: registry.proxy_tls(),
            proxy: OutboundProxySettings::default(),
            resolver: Arc::new(SystemResolver),
            version: HttpVersionPref::Http1,
            h2: H2Settings::default(),
            omit_h2_content_length: false,
        };
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let server_tls = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut hellos = Vec::new();
            let mut streams = Vec::new();
            for _ in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut peek = vec![0; 65540];
                let hello = loop {
                    socket.readable().await.unwrap();
                    let count = socket.peek(&mut peek).await.unwrap();
                    assert_ne!(count, 0);
                    if count >= 5 {
                        let length = 5 + u16::from_be_bytes([peek[3], peek[4]]) as usize;
                        if count >= length {
                            break peek[..length].to_vec();
                        }
                    }
                    tokio::task::yield_now().await;
                };
                hellos.push(hello);
                let mut stream = acceptor.accept(socket).await.unwrap();
                super::test_support::read_headers(&mut stream).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                    .await
                    .unwrap();
                streams.push(stream);
            }
            for stream in &mut streams {
                stream.write_all(b"x").await.unwrap();
            }
            hellos
        });
        let transport = VendorTransport::new();
        let plan = || RequestPlan {
            method: "GET".into(),
            uri: format!("https://localhost:{}/", addr.port()),
            fields: Vec::new(),
            body: Bytes::new(),
            observer: None,
        };
        let mut first = transport.send(plan(), route.clone()).await.unwrap();
        let mut second = transport.send(plan(), route).await.unwrap();
        assert_eq!(
            first.body.next_chunk().await.unwrap().unwrap(),
            Bytes::from_static(b"x")
        );
        assert_eq!(
            second.body.next_chunk().await.unwrap().unwrap(),
            Bytes::from_static(b"x")
        );
        let hellos = server.await.unwrap();
        for hello in hellos {
            crate::plugin::network::shape::tests::assert_wire_matches_fixture(
                "antigravity reconnect",
                &hello,
                include_str!("../shape/fixtures/antigravity-inference.json"),
                false,
            );
        }
    })
    .await
    .expect("two transport TLS handshakes timed out");
}

#[tokio::test]
async fn forced_http1_filters_shape_alpn_in_actual_clienthello() {
    use super::{H2Settings, HttpVersionPref, RequestPlan, Route, SystemResolver, VendorTransport};
    use crate::outbound_proxy::OutboundProxySettings;
    use bytes::Bytes;

    timeout(Duration::from_secs(5), async {
        for (offered, expected) in [
            (vec![], None),
            (
                vec![b"h2".to_vec(), b"http/1.1".to_vec()],
                Some(vec![
                    0, 9, 8, b'h', b't', b't', b'p', b'/', b'1', b'.', b'1',
                ]),
            ),
            (vec![b"h2".to_vec()], None),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut header = [0; 5];
                socket.read_exact(&mut header).await.unwrap();
                assert_eq!(header[0], 22);
                let mut body = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
                socket.read_exact(&mut body).await.unwrap();
                body
            });
            let mut tls = super::test_support::untrusted_tls();
            tls.alpn_protocols = offered;
            let tls = Arc::new(tls);
            let route = Route {
                shape_identity: "wire-alpn".into(),
                tls_session_policy: crate::plugin::network::shape::SessionPolicy::Standard,
                tls: tls.clone(),
                proxy_tls: tls,
                proxy: OutboundProxySettings::default(),
                version: HttpVersionPref::Http1,
                h2: H2Settings::default(),
                omit_h2_content_length: false,
                resolver: Arc::new(SystemResolver),
            };
            let request = tokio::spawn(async move {
                VendorTransport::new()
                    .send(
                        RequestPlan {
                            method: "GET".into(),
                            uri: format!("https://{addr}/"),
                            fields: Vec::new(),
                            body: Bytes::new(),
                            observer: None,
                        },
                        route,
                    )
                    .await
                    .is_err()
            });
            let hello = server.await.unwrap();
            assert!(request.await.unwrap()); // The capture server intentionally closes before TLS completes.
            assert_eq!(hello[0], 1);
            let mut pos = 4 + 2 + 32;
            pos += 1 + hello[pos] as usize;
            let suites_len = u16::from_be_bytes([hello[pos], hello[pos + 1]]) as usize;
            pos += 2 + suites_len;
            pos += 1 + hello[pos] as usize;
            let extensions_len = u16::from_be_bytes([hello[pos], hello[pos + 1]]) as usize;
            pos += 2;
            let end = pos + extensions_len;
            let mut alpn = None;
            while pos < end {
                let kind = u16::from_be_bytes([hello[pos], hello[pos + 1]]);
                let len = u16::from_be_bytes([hello[pos + 2], hello[pos + 3]]) as usize;
                pos += 4;
                if kind == 16 {
                    alpn = Some(hello[pos..pos + len].to_vec());
                }
                pos += len;
            }
            assert_eq!(pos, end);
            assert_eq!(alpn, expected);
        }
    })
    .await
    .expect("transport ClientHello capture timed out");
}

use rama_core::ServiceInput;
use rama_http_core::client::conn::http1;
use rama_http_types::{HeaderName, HeaderValue, Request, body::Body};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

#[tokio::test]
async fn http1_preserves_interleaved_duplicate_header_order_and_casing_on_wire() {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0; 1024];
            while !received.ends_with(b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "connection closed before complete request headers");
                received.extend_from_slice(&buf[..n]);
                assert!(received.len() <= 8192, "request headers exceeded test limit");
            }
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            received
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, connection) = http1::handshake(ServiceInput::new(stream)).await.unwrap();
        let connection = tokio::spawn(connection);

        let mut request = Request::builder().uri("/wire").body(Body::empty()).unwrap();
        for (name, value) in [
            ("Host", "localhost"),
            ("X-Mix", "first"),
            ("x-Other", "middle"),
            ("x-mIX", "second"),
            ("X-OTHER", "last"),
        ] {
            request.headers_mut().append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 204);
        let received = server.await.unwrap();
        assert_eq!(
            received,
            b"GET /wire HTTP/1.1\r\nHost: localhost\r\nX-Mix: first\r\nx-Other: middle\r\nx-mIX: second\r\nX-OTHER: last\r\n\r\n"
        );
        drop(sender);
        connection.await.unwrap().unwrap();
    })
    .await
    .expect("loopback HTTP/1 request timed out");
}

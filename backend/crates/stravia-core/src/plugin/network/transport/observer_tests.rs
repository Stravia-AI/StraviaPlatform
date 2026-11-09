use std::{
    io::Cursor,
    ops::ControlFlow,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use rama_http_types::proto::h2::{PseudoHeader, PseudoHeaderOrder, hpack::Decoder};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

use super::{
    Field, H2Settings, HttpVersionPref, RequestPlan, Route, SystemResolver, VendorTransport,
};
use crate::outbound_proxy::OutboundProxySettings;

fn route(version: HttpVersionPref, h2: H2Settings) -> Route {
    let tls = Arc::new(super::test_support::untrusted_tls());
    Route {
        shape_identity: "observer-test".into(),
        tls_session_policy: crate::plugin::network::shape::SessionPolicy::Standard,
        tls: Arc::clone(&tls),
        proxy_tls: tls,
        proxy: OutboundProxySettings::default(),
        resolver: Arc::new(SystemResolver),
        version,
        h2,
        omit_h2_content_length: false,
    }
}

fn field(name: &str, value: &[u8]) -> Field {
    Field {
        name: name.into(),
        bytes: value.to_vec(),
    }
}

#[tokio::test]
async fn http1_observer_matches_wire_before_response_io_error() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let headers = super::test_support::read_headers(&mut socket).await;
            // Do not close the connection until the observer has run. The client
            // then encounters an I/O error instead of receiving a response.
            observed_rx.await.unwrap();
            headers
        });
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let result = VendorTransport::new()
            .send(
                RequestPlan {
                    method: "POST".into(),
                    uri: format!("http://{address}/wire"),
                    fields: vec![
                        field("Host", b"example.test"),
                        field("X-Mix", b"first"),
                        field("x-Other", b"middle"),
                        field("x-mIX", b"second"),
                        field("X-OTHER", b"last"),
                    ],
                    body: Bytes::from_static(b"payload"),
                    observer: Some(Box::new(move |head| {
                        captured.lock().unwrap().push(head);
                        observed_tx.send(()).unwrap();
                    })),
                },
                route(HttpVersionPref::Http1, H2Settings::default()),
            )
            .await;
        assert!(result.is_err(), "server closed without a response");
        let wire = server.await.unwrap();
        let mut heads = observed.lock().unwrap();
        assert_eq!(heads.len(), 1);
        let head = heads.pop().unwrap();
        let mut observed_headers = Vec::new();
        for field in head.fields {
            observed_headers.extend_from_slice(field.name.as_bytes());
            observed_headers.extend_from_slice(b": ");
            observed_headers.extend_from_slice(&field.bytes);
            observed_headers.extend_from_slice(b"\r\n");
        }
        observed_headers.extend_from_slice(b"\r\n");
        let start = wire.windows(2).position(|bytes| bytes == b"\r\n").unwrap() + 2;
        assert_eq!(observed_headers, wire[start..]);
        assert!(
            wire.windows(b"content-length: 7\r\n".len())
                .any(|bytes| bytes.eq_ignore_ascii_case(b"content-length: 7\r\n"))
        );
        assert!(
            wire.windows(
                b"X-Mix: first\r\nx-Other: middle\r\nx-mIX: second\r\nX-OTHER: last\r\n".len()
            )
            .any(|bytes| bytes
                == b"X-Mix: first\r\nx-Other: middle\r\nx-mIX: second\r\nX-OTHER: last\r\n")
        );
    })
    .await
    .expect("HTTP/1 observer capture timed out");
}

async fn read_frame(stream: &mut TcpStream) -> (u8, u8, u32, Vec<u8>) {
    let mut header = [0; 9];
    stream.read_exact(&mut header).await.unwrap();
    let len =
        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await.unwrap();
    (
        header[3],
        header[4],
        u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fff_ffff,
        payload,
    )
}

#[tokio::test]
async fn http2_observer_matches_configured_pseudo_header_order_on_wire() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut preface = [0; 24];
            socket.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            socket
                .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let mut headers = BytesMut::new();
            let mut continuing = false;
            loop {
                let (kind, flags, stream, payload) = read_frame(&mut socket).await;
                if continuing {
                    assert_eq!((kind, stream), (9, 1));
                } else if kind != 1 {
                    continue;
                } else {
                    assert_eq!(stream, 1);
                    assert_eq!(flags & 0x28, 0, "unexpected padding or priority");
                }
                headers.extend_from_slice(&payload);
                continuing = flags & 4 == 0;
                if !continuing {
                    break;
                }
            }
            observed_rx.await.unwrap();
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
            fields
        });
        let pseudo_header_order: PseudoHeaderOrder = [
            PseudoHeader::Path,
            PseudoHeader::Authority,
            PseudoHeader::Method,
            PseudoHeader::Scheme,
        ]
        .into_iter()
        .collect();
        let settings = H2Settings {
            pseudo_header_order: Some(pseudo_header_order),
            ..Default::default()
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let result = VendorTransport::new()
            .send(
                RequestPlan {
                    method: "POST".into(),
                    uri: format!("http://{address}/wire?x=1"),
                    fields: vec![field("X-Mix", b"first"), field("x-mIX", b"second")],
                    body: Bytes::from_static(b"payload"),
                    observer: Some(Box::new(move |head| {
                        captured.lock().unwrap().push(head);
                        observed_tx.send(()).unwrap();
                    })),
                },
                route(HttpVersionPref::Http2, settings),
            )
            .await;
        assert!(result.is_err(), "server closed without a response");
        let wire = server.await.unwrap();
        let mut heads = observed.lock().unwrap();
        assert_eq!(heads.len(), 1);
        let head = heads.pop().unwrap();
        let fields: Vec<_> = head
            .fields
            .into_iter()
            .map(|f| (f.name.into_bytes(), f.bytes))
            .collect();
        assert_eq!(fields, wire);
        let pseudo: Vec<_> = wire
            .iter()
            .filter(|(name, _)| name.starts_with(b":"))
            .map(|(name, _)| name.as_slice())
            .collect();
        assert_eq!(
            pseudo,
            vec![b":path".as_slice(), b":authority", b":method", b":scheme"]
        );
    })
    .await
    .expect("HTTP/2 observer capture timed out");
}

//! Wire-level coverage for HTTP/2 request content-length policy.

use std::{io::Cursor, ops::ControlFlow, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use rama_http_types::proto::h2::hpack::Decoder;
use rustls::{ClientConfig, RootCertStore};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

use super::{H2Settings, HttpVersionPref, RequestPlan, Route, SystemResolver, VendorTransport};
use crate::outbound_proxy::OutboundProxySettings;

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

async fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0; 9];
    stream.read_exact(&mut header).await.unwrap();
    let len =
        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await.unwrap();
    Frame {
        kind: header[3],
        flags: header[4],
        stream: u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fff_ffff,
        payload,
    }
}

async fn capture_request(omit_h2_content_length: bool) -> (Vec<Vec<u8>>, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, released) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut preface = [0; 24];
        stream.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        stream
            .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
            .await
            .unwrap();

        let mut headers = BytesMut::new();
        let mut body = Vec::new();
        let mut continuing = false;
        let mut headers_complete = false;
        loop {
            let frame = read_frame(&mut stream).await;
            if continuing {
                assert_eq!(frame.kind, 9, "expected CONTINUATION");
                assert_eq!(frame.stream, 1);
                headers.extend_from_slice(&frame.payload);
                continuing = frame.flags & 4 == 0;
                headers_complete = !continuing;
                continue;
            }
            match frame.kind {
                1 => {
                    assert_eq!(frame.stream, 1);
                    assert!(!headers_complete, "unexpected additional headers");
                    assert_eq!(frame.flags & 0x28, 0, "unexpected padding or priority");
                    assert_eq!(frame.flags & 1, 0, "nonempty request ended in headers");
                    headers.extend_from_slice(&frame.payload);
                    continuing = frame.flags & 4 == 0;
                    headers_complete = !continuing;
                }
                0 => {
                    assert_eq!(frame.stream, 1);
                    assert!(headers_complete);
                    assert_eq!(frame.flags & 8, 0, "unexpected DATA padding");
                    body.extend_from_slice(&frame.payload);
                    if frame.flags & 1 != 0 {
                        break;
                    }
                }
                _ => {}
            }
        }

        let mut lengths = Vec::new();
        Decoder::new(4096)
            .decode(&mut Cursor::new(&mut headers), |header| {
                if header.name().as_slice() == b"content-length" {
                    lengths.push(header.value_slice().to_vec());
                }
                ControlFlow::Continue(())
            })
            .unwrap();
        // Indexed static-table :status 200, END_HEADERS | END_STREAM.
        stream
            .write_all(&[0, 0, 1, 1, 5, 0, 0, 0, 1, 0x88])
            .await
            .unwrap();
        let _ = released.await;
        (lengths, body)
    });

    let tls = Arc::new(
        ClientConfig::builder_with_provider(
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth(),
    );
    let route = Route {
        shape_identity: "h2-length-test".into(),
        tls_session_policy: crate::plugin::network::shape::SessionPolicy::Standard,
        tls: Arc::clone(&tls),
        proxy_tls: tls,
        proxy: OutboundProxySettings::default(),
        version: HttpVersionPref::Http2,
        h2: H2Settings::default(),
        resolver: Arc::new(SystemResolver),
        omit_h2_content_length,
    };
    let response = VendorTransport::new()
        .send(
            RequestPlan {
                method: "POST".into(),
                uri: format!("http://{address}/length"),
                fields: if omit_h2_content_length {
                    vec![super::Field {
                        name: "Content-Length".into(),
                        bytes: b"21".to_vec(),
                    }]
                } else {
                    Vec::new()
                },
                body: Bytes::from_static(b"nonempty request body"),
                observer: None,
            },
            route,
        )
        .await
        .unwrap();
    assert_eq!(response.head.status, 200);
    release.send(()).unwrap();
    server.await.unwrap()
}

#[tokio::test]
async fn h2_content_length_policy_preserves_request_body() {
    timeout(Duration::from_secs(10), async {
        let (lengths, body) = capture_request(false).await;
        assert_eq!(lengths, vec![b"21".to_vec()]);
        assert_eq!(body, b"nonempty request body");

        let (lengths, body) = capture_request(true).await;
        assert!(
            lengths.is_empty(),
            "omit policy must not send content-length"
        );
        assert_eq!(body, b"nonempty request body");
    })
    .await
    .expect("HTTP/2 content-length wire capture timed out");
}

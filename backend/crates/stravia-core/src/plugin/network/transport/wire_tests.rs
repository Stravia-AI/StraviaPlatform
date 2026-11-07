use std::time::Duration;

use rama_core::ServiceInput;
use rama_http_core::client::conn::http1;
use rama_http_types::{body::Body, HeaderName, HeaderValue, Request};
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

//! Verify that an unresponsive HTTP/2 peer fails an outstanding request.

use std::time::Duration;

use bytes::Bytes;
use rama_core::svc_input::ServiceInput;
use rama_http_types::{Request, body::util::Full};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    time::timeout,
};

use super::h2::{H2Settings, connect};

#[tokio::test]
async fn unanswered_keepalive_ping_fails_pending_response() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut preface = [0; 24];
            socket.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            // Advertise an empty initial SETTINGS, but never respond to PING.
            socket.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap();

            let mut saw_headers = false;
            let mut saw_ping = false;
            while !saw_headers || !saw_ping {
                let mut header = [0; 9];
                socket.read_exact(&mut header).await.unwrap();
                let len = (usize::from(header[0]) << 16)
                    | (usize::from(header[1]) << 8)
                    | usize::from(header[2]);
                let mut payload = vec![0; len];
                socket.read_exact(&mut payload).await.unwrap();
                let stream = u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fff_ffff;
                if header[3] == 1 && stream != 0 {
                    saw_headers = true;
                }
                if header[3] == 6 && header[4] & 1 == 0 {
                    assert_eq!(stream, 0);
                    assert_eq!(len, 8);
                    saw_ping = true;
                }
            }
            observed_tx.send(()).unwrap();
            // Keep the connection open until after the client reports the error;
            // a peer EOF must not be mistaken for a keepalive timeout.
            let _ = release_rx.await;
            drop(socket);
        });

        let settings = H2Settings {
            keep_alive_interval: Some(Duration::from_millis(50)),
            keep_alive_timeout: Duration::from_millis(100),
            keep_alive_while_idle: false,
            ..Default::default()
        };
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut sender, _peer_settings) = connect(ServiceInput::new(socket), settings).await.unwrap();
        sender.ready().await.unwrap();
        let request = Request::builder()
            .uri("http://example.test/keepalive")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = tokio::spawn(async move { sender.send_request(request).await });

        observed_rx.await.expect("server closed before observing request and PING");
        let result = timeout(Duration::from_secs(3), response)
            .await
            .expect("pending response did not fail after unanswered PING")
            .expect("request task panicked");
        assert!(result.is_err(), "unanswered keepalive PING must fail the request");
        release_tx.send(()).unwrap();
        server.await.unwrap();
    })
    .await
    .expect("HTTP/2 keepalive test timed out");
}

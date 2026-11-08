use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

use super::connector::{self, Proxy, Route, SystemResolver};

const TARGET_PORT: u16 = 8443;

async fn socks_exchange(scheme: &str, target: &str, remote_dns: bool) {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let expected_target = target.to_owned();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            socket.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            socket.write_all(&[5, 0]).await.unwrap();

            let mut prefix = [0; 4];
            socket.read_exact(&mut prefix).await.unwrap();
            assert_eq!(&prefix[..3], &[5, 1, 0]);
            match prefix[3] {
                3 if remote_dns => {
                    let mut length = [0];
                    socket.read_exact(&mut length).await.unwrap();
                    let mut name = vec![0; usize::from(length[0])];
                    socket.read_exact(&mut name).await.unwrap();
                    assert_eq!(name, expected_target.as_bytes());
                }
                1 if !remote_dns => {
                    let mut octets = [0; 4];
                    socket.read_exact(&mut octets).await.unwrap();
                    assert!(IpAddr::V4(Ipv4Addr::from(octets)).is_loopback());
                }
                4 if !remote_dns => {
                    let mut octets = [0; 16];
                    socket.read_exact(&mut octets).await.unwrap();
                    assert_eq!(Ipv6Addr::from(octets), Ipv6Addr::LOCALHOST);
                }
                address_type => panic!(
                    "unexpected SOCKS address type {address_type} (remote DNS: {remote_dns})"
                ),
            }
            let mut port = [0; 2];
            socket.read_exact(&mut port).await.unwrap();
            assert_eq!(u16::from_be_bytes(port), TARGET_PORT);
            socket
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            let mut payload = [0; 4];
            socket.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"ping");
            socket.write_all(b"pong").await.unwrap();
        });

        let route = Route::Proxy(Proxy::from_url(&format!("{scheme}://{proxy_addr}")).unwrap());
        let mut stream = connector::connect(target, TARGET_PORT, &route, None, &SystemResolver)
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut payload = [0; 4];
        stream.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"pong");
        server.await.unwrap();
    })
    .await
    .expect("SOCKS loopback exchange timed out");
}

#[tokio::test]
async fn socks5h_sends_domain_to_proxy_for_remote_dns() {
    socks_exchange("socks5h", "unresolvable.example.invalid", true).await;
}

#[tokio::test]
async fn socks5_resolves_domain_locally_before_connecting_to_proxy() {
    socks_exchange("socks5", "localhost", false).await;
}

#[tokio::test]
async fn http_connect_sends_target_and_proxy_authorization() {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = super::test_support::read_headers(&mut socket).await;
            assert_eq!(request, b"CONNECT target.example:8443 HTTP/1.1\r\nHost: target.example:8443\r\nProxy-Authorization: Basic dXNlcjpwYXNz\r\n\r\n");
            socket.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\npong").await.unwrap();
            let mut payload = [0; 4];
            socket.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"ping");
        });
        let route = Route::Proxy(Proxy::from_url(&format!("http://user:pass@{proxy_addr}")).unwrap());
        let mut stream = connector::connect("target.example", TARGET_PORT, &route, None, &SystemResolver).await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut payload = [0; 4];
        stream.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"pong");
        server.await.unwrap();
    }).await.expect("HTTP CONNECT loopback exchange timed out");
}

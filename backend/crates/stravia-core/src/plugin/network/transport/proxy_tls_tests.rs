use std::{sync::Arc, time::Duration};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

use super::{
    connector::{self, BoxStream, Proxy, Route, SystemResolver},
    test_support::read_headers,
};

fn loopback_tls() -> (Arc<ClientConfig>, TlsAcceptor) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned(), "localhost".to_owned()]).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let client = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let server = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    (Arc::new(client), TlsAcceptor::from(Arc::new(server)))
}

async fn tls_request_through_proxy(https_proxy: bool) {
    timeout(Duration::from_secs(10), async {
        let (target_client, target_acceptor) = loopback_tls();
        let (proxy_client, proxy_acceptor) = loopback_tls();
        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let target = tokio::spawn(async move {
            let (socket, _) = target_listener.accept().await.unwrap();
            let mut tls = target_acceptor.accept(socket).await.unwrap();
            assert_eq!(
                read_headers(&mut tls).await,
                b"GET /through-proxy HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            );
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .await
                .unwrap();
            tls.shutdown().await.unwrap();
        });
        let proxy = tokio::spawn(async move {
            let (socket, _) = proxy_listener.accept().await.unwrap();
            let mut inbound: BoxStream = if https_proxy {
                Box::new(proxy_acceptor.accept(socket).await.unwrap())
            } else {
                Box::new(socket)
            };
            let headers = read_headers(&mut inbound).await;
            let request = std::str::from_utf8(&headers).unwrap();
            assert_eq!(
                request.lines().next().unwrap(),
                format!("CONNECT {target_addr} HTTP/1.1")
            );
            let mut outbound = TcpStream::connect(target_addr).await.unwrap();
            inbound
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut inbound, &mut outbound)
                .await
                .unwrap();
        });

        let scheme = if https_proxy { "https" } else { "http" };
        let route = Route::Proxy(Proxy::from_url(&format!("{scheme}://{proxy_addr}")).unwrap());
        let proxy_tls = https_proxy.then_some(proxy_client);
        let (mut stream, _) = connector::connect_tls(
            "127.0.0.1",
            target_addr.port(),
            &route,
            proxy_tls,
            target_client,
            &SystemResolver,
        )
        .await
        .unwrap();
        stream
            .write_all(
                b"GET /through-proxy HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let expected = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
        let mut response = vec![0; expected.len()];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(response, expected);
        stream.shutdown().await.unwrap();
        drop(stream);
        target.await.unwrap();
        proxy.await.unwrap();
    })
    .await
    .expect("TLS request through CONNECT proxy timed out");
}

#[tokio::test]
async fn tls_request_through_http_connect_proxy() {
    tls_request_through_proxy(false).await;
}

#[tokio::test]
async fn tls_request_through_https_connect_proxy() {
    tls_request_through_proxy(true).await;
}

async fn tls_request_through_socks_proxy(remote_dns: bool) {
    timeout(Duration::from_secs(10), async {
        let (target_client, target_acceptor) = loopback_tls();
        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let target = tokio::spawn(async move {
            let (socket, _) = target_listener.accept().await.unwrap();
            let mut tls = target_acceptor.accept(socket).await.unwrap();
            assert_eq!(
                read_headers(&mut tls).await,
                b"GET /socks HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            );
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .await
                .unwrap();
            tls.shutdown().await.unwrap();
        });
        let proxy = tokio::spawn(async move {
            let (mut inbound, _) = proxy_listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            inbound.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            inbound.write_all(&[5, 0]).await.unwrap();
            let mut request = [0; 4];
            inbound.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..3], &[5, 1, 0]);
            match (remote_dns, request[3]) {
                (true, 3) => {
                    let len = inbound.read_u8().await.unwrap() as usize;
                    let mut hostname = vec![0; len];
                    inbound.read_exact(&mut hostname).await.unwrap();
                    assert_eq!(hostname, b"localhost");
                }
                (false, 1) => {
                    let mut address = [0; 4];
                    inbound.read_exact(&mut address).await.unwrap();
                    assert!(std::net::Ipv4Addr::from(address).is_loopback());
                }
                (false, 4) => {
                    let mut address = [0; 16];
                    inbound.read_exact(&mut address).await.unwrap();
                    assert!(std::net::Ipv6Addr::from(address).is_loopback());
                }
                _ => panic!("unexpected SOCKS destination address type"),
            }
            assert_eq!(inbound.read_u16().await.unwrap(), target_addr.port());
            let mut outbound = TcpStream::connect(target_addr).await.unwrap();
            inbound
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut inbound, &mut outbound)
                .await
                .unwrap();
        });

        let scheme = if remote_dns { "socks5h" } else { "socks5" };
        let route = Route::Proxy(Proxy::from_url(&format!("{scheme}://{proxy_addr}")).unwrap());
        let (mut stream, _) = connector::connect_tls(
            "localhost",
            target_addr.port(),
            &route,
            None,
            target_client,
            &SystemResolver,
        )
        .await
        .unwrap();
        stream
            .write_all(b"GET /socks HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let expected = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
        let mut response = vec![0; expected.len()];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(response, expected);
        stream.shutdown().await.unwrap();
        drop(stream);
        target.await.unwrap();
        proxy.await.unwrap();
    })
    .await
    .expect("TLS request through SOCKS proxy timed out");
}

#[tokio::test]
async fn tls_request_through_socks5_proxy() {
    tls_request_through_socks_proxy(false).await;
}

#[tokio::test]
async fn tls_request_through_socks5h_proxy() {
    tls_request_through_socks_proxy(true).await;
}

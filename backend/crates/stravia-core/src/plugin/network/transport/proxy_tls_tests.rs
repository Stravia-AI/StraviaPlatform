use std::{sync::Arc, time::Duration};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{ClientConfig, RootCertStore, ServerConfig, pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer}};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

use super::connector::{self, BoxStream, Proxy, Route, SystemResolver};

fn loopback_tls() -> (Arc<ClientConfig>, TlsAcceptor) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let client = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions().unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let server = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions().unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key).unwrap();
    (Arc::new(client), TlsAcceptor::from(Arc::new(server)))
}

async fn read_headers(stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < 8192, "HTTP headers exceeded limit");
        headers.push(stream.read_u8().await.unwrap());
    }
    headers
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
            assert_eq!(read_headers(&mut tls).await, b"GET /through-proxy HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK").await.unwrap();
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
            assert_eq!(request.lines().next().unwrap(), format!("CONNECT {target_addr} HTTP/1.1"));
            let mut outbound = TcpStream::connect(target_addr).await.unwrap();
            inbound.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
            tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await.unwrap();
        });

        let scheme = if https_proxy { "https" } else { "http" };
        let route = Route::Proxy(Proxy::from_url(&format!("{scheme}://{proxy_addr}")).unwrap());
        let proxy_tls = https_proxy.then_some(proxy_client);
        let (mut stream, _) = connector::connect_tls(
            "127.0.0.1", target_addr.port(), &route, proxy_tls, target_client, &SystemResolver,
        ).await.unwrap();
        stream.write_all(b"GET /through-proxy HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.unwrap();
        let expected = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
        let mut response = vec![0; expected.len()];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(response, expected);
        stream.shutdown().await.unwrap();
        drop(stream);
        target.await.unwrap();
        proxy.await.unwrap();
    }).await.expect("TLS request through CONNECT proxy timed out");
}

#[tokio::test]
async fn tls_request_through_http_connect_proxy() {
    tls_request_through_proxy(false).await;
}

#[tokio::test]
async fn tls_request_through_https_connect_proxy() {
    tls_request_through_proxy(true).await;
}

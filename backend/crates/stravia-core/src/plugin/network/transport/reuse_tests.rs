use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinSet,
    time::timeout,
};

use super::{H2Settings, HttpVersionPref, RequestPlan, Route, SystemResolver, VendorTransport};
use crate::outbound_proxy::OutboundProxySettings;

async fn serve_connection(mut socket: TcpStream, id: usize) {
    loop {
        let mut headers = Vec::new();
        loop {
            let mut byte = [0];
            let n = socket.read(&mut byte).await.unwrap();
            if n == 0 {
                assert!(headers.is_empty(), "connection closed mid-request");
                return;
            }
            headers.push(byte[0]);
            assert!(headers.len() <= 8192, "request headers exceeded test limit");
            if headers.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let body = id.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    }
}

async fn request(transport: &VendorTransport, uri: &str, route: Route) -> usize {
    let mut response = transport
        .send(
            RequestPlan {
                method: "GET".into(),
                uri: uri.into(),
                fields: Vec::new(),
                body: Bytes::new(),
                observer: None,
            },
            route,
        )
        .await
        .unwrap();
    assert_eq!(response.head.status, 200);
    let mut body = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    // Returning an HTTP/1 lease to the pool is handled by a spawned task.
    tokio::task::yield_now().await;
    std::str::from_utf8(&body).unwrap().parse().unwrap()
}

#[tokio::test]
async fn http1_reuses_only_matching_shape_revision_and_proxy_identity() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, mut stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let mut next_id = 0;
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (socket, _) = result.unwrap();
                        next_id += 1;
                        connections.spawn(serve_connection(socket, next_id));
                    }
                    result = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap().unwrap();
                    }
                    _ = &mut stopped => break,
                }
            }
        });

        let tls = Arc::new(super::test_support::untrusted_tls());
        let route = Route {
            shape_identity: "plugin:component-a:reuse-test:1".into(),
            tls_session_policy: crate::plugin::network::shape::SessionPolicy::Standard,
            tls: Arc::clone(&tls),
            proxy_tls: tls,
            proxy: OutboundProxySettings::default(),
            version: HttpVersionPref::Http1,
            h2: H2Settings::default(),
            omit_h2_content_length: false,
            resolver: Arc::new(SystemResolver),
        };
        let transport = VendorTransport::new();
        let uri = format!("http://{addr}/reuse");

        let original = request(&transport, &uri, route.clone()).await;
        assert_eq!(
            request(&transport, &uri, route.clone()).await,
            original,
            "matching shape and revision should reuse the connection"
        );

        let mut revised = route.clone();
        revised.shape_identity = "plugin:component-a:reuse-test:2".into();
        let revision_connection = request(&transport, &uri, revised).await;
        assert_ne!(
            revision_connection, original,
            "a new revision must not reuse the old connection"
        );

        let mut another_component = route.clone();
        another_component.shape_identity = "plugin:component-b:reuse-test:1".into();
        let other_connection = request(&transport, &uri, another_component).await;
        assert_ne!(
            other_connection, original,
            "equal local names and revisions from different components must not share a connection"
        );
        assert_eq!(request(&transport, &uri, route.clone()).await, original);

        // Both proxy identities dial the same loopback socket, so a different
        // connection demonstrates credential isolation rather than routing to a
        // different server. The proxy responds directly to the forward request.
        let mut proxy_a = route.clone();
        proxy_a.proxy.url = format!("http://alice:one@{addr}");
        let mut proxy_b = proxy_a.clone();
        proxy_b.proxy.url = format!("http://bob:two@{addr}");
        let first_proxy = request(&transport, &uri, proxy_a.clone()).await;
        assert_ne!(
            first_proxy, original,
            "a proxy must not reuse a direct connection"
        );
        let second_proxy = request(&transport, &uri, proxy_b).await;
        assert_ne!(
            second_proxy, first_proxy,
            "different proxy identities must not share a connection"
        );
        assert_eq!(
            request(&transport, &uri, proxy_a).await,
            first_proxy,
            "the original proxy identity should retain its own connection"
        );

        drop(transport);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    })
    .await
    .expect("loopback HTTP/1 reuse test timed out");
}

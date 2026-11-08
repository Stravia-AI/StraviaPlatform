use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncReadExt};

// Used when TLS is unused or intentionally fails before certificate verification.
pub(super) fn untrusted_tls() -> ClientConfig {
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth()
}

// Read exactly the HTTP head, leaving any following body or upgraded bytes untouched.
pub(super) async fn read_headers(stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < 8192, "HTTP headers exceeded test limit");
        headers.push(stream.read_u8().await.unwrap());
    }
    headers
}

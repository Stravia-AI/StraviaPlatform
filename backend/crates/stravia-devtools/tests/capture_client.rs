use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio_rustls::rustls::{
    self, ClientConnection, RootCertStore, StreamOwned,
    pki_types::{CertificateDer, ServerName, pem::PemObject},
};

struct Capture {
    child: Child,
    dir: tempfile::TempDir,
    port: u16,
}

impl Capture {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_stravia-tools"))
            .args(["capture-client", "--port", "0", "--ca"])
            .arg(dir.path().join("ca.pem"))
            .arg("--output")
            .arg(dir.path().join("capture.json"))
            .args([
                "--client",
                "integration-client",
                "--client-version",
                "test-version",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .split_whitespace()
            .nth(1)
            .expect("capture did not print LISTEN address");
        let port = address.rsplit(':').next().unwrap().parse().unwrap();
        Self { child, dir, port }
    }

    fn connect(&self) -> StreamOwned<ClientConnection, TcpStream> {
        self.connect_with_alpn(b"http/1.1")
    }

    fn connect_with_alpn(&self, protocol: &[u8]) -> StreamOwned<ClientConnection, TcpStream> {
        let pem = fs::read(self.dir.path().join("ca.pem")).unwrap();
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(&pem).unwrap())
            .unwrap();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![protocol.to_vec()];
        let connection =
            ClientConnection::new(Arc::new(config), ServerName::try_from("localhost").unwrap())
                .unwrap();
        let socket = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        StreamOwned::new(connection, socket)
    }

    fn finish(mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "capture process failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "capture process did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        serde_json::from_slice(&fs::read(self.dir.path().join("capture.json")).unwrap()).unwrap()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct FragmentHello {
    socket: TcpStream,
    first_write: bool,
}

impl Read for FragmentHello {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.socket.read(buf)
    }
}

impl Write for FragmentHello {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.first_write {
            self.first_write = false;
            assert!(
                buf.len() >= 6 && buf[0] == 22,
                "expected TLS handshake record"
            );
            let length = u16::from_be_bytes([buf[3], buf[4]]) as usize;
            assert!(
                length > 1 && buf.len() >= length + 5,
                "expected complete ClientHello record"
            );
            let split = length / 2;
            self.socket
                .write_all(&[22, buf[1], buf[2], (split >> 8) as u8, split as u8])?;
            self.socket.write_all(&buf[5..5 + split])?;
            let rest = length - split;
            self.socket
                .write_all(&[22, buf[1], buf[2], (rest >> 8) as u8, rest as u8])?;
            self.socket.write_all(&buf[5 + split..])?;
            Ok(buf.len())
        } else {
            self.socket.write(buf)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

fn response_headers(stream: &mut impl Read) -> String {
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        assert!(response.len() < 8192, "response headers too large");
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
    }
    String::from_utf8(response).unwrap()
}

fn assert_sanitized(capture: &Value, secrets: &[&str]) {
    let text = capture.to_string();
    for secret in secrets {
        assert!(!text.contains(secret), "capture leaked {secret}");
    }
    assert_eq!(capture["provenance"]["client"], "integration-client");
    assert_eq!(capture["provenance"]["version"], "test-version");
}

#[test]
fn captures_fragmented_clienthello_and_completes_tls_request() {
    let capture = Capture::start();
    let stream = capture.connect();
    let mut stream = StreamOwned::new(
        stream.conn,
        FragmentHello {
            socket: stream.sock,
            first_write: true,
        },
    );
    stream
        .write_all(b"GET /fragmented HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    assert!(response_headers(&mut stream).starts_with("HTTP/1.1 401 "));
    drop(stream);
    let result = capture.finish();
    assert!(result["clienthello"].is_object());
    assert_eq!(result["h1"]["method"], "GET");
    assert_sanitized(&result, &["/fragmented"]);
}

#[test]
fn captures_http1_header_order_without_secrets() {
    let capture = Capture::start();
    let mut stream = capture.connect();
    stream.write_all(b"POST /private-path?token=secret-query HTTP/1.1\r\nHost: localhost\r\nUser-Agent: example-client/1.2\r\nAccept: application/json\r\nX-First: private-first\r\nAuthorization:\tBearer secret-auth  \r\nCookie: secret-cookie\r\nX-First: private-second\r\nContent-Length: 0\r\n\r\n").unwrap();
    let response = response_headers(&mut stream);
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    drop(stream);
    let result = capture.finish();
    assert_eq!(result["h1"]["method"], "POST");
    assert_eq!(result["h1"]["version"], "HTTP/1.1");
    assert_eq!(result["h1"]["target"], "<redacted>");
    assert!(
        result["h1"]["raw_head"]
            .as_str()
            .unwrap()
            .contains("Authorization:\t<redacted>  \r\n")
    );
    assert!(
        result["h1"]["raw_head"]
            .as_str()
            .unwrap()
            .starts_with("POST <redacted> HTTP/1.1\r\n")
    );
    assert_eq!(
        result["h1"]["headers"],
        json!([
            {"name":"Host", "value":"<redacted>"},
            {"name":"User-Agent", "value":"example-client/1.2"},
            {"name":"Accept", "value":"application/json"},
            {"name":"X-First", "value":"<redacted>"},
            {"name":"Authorization", "value":"<redacted>"},
            {"name":"Cookie", "value":"<redacted>"},
            {"name":"X-First", "value":"<redacted>"},
            {"name":"Content-Length", "value":"<redacted>"}
        ])
    );
    assert_sanitized(
        &result,
        &[
            "private-path",
            "secret-query",
            "private-first",
            "private-second",
            "secret-auth",
            "secret-cookie",
        ],
    );
}

#[test]
fn captures_http2_settings_windows_and_pseudoheader_order() {
    let capture = Capture::start();
    let mut stream = capture.connect_with_alpn(b"h2");
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .unwrap();
    // SETTINGS_INITIAL_WINDOW_SIZE = 65535, followed by a connection window update.
    stream
        .write_all(&[0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4, 0, 0, 255, 255])
        .unwrap();
    stream
        .write_all(&[0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 100])
        .unwrap();
    // HPACK static-table indexes :method GET, :scheme https, :path /.
    stream
        .write_all(&[0, 0, 3, 1, 5, 0, 0, 0, 1, 0x82, 0x87, 0x84])
        .unwrap();
    stream.flush().unwrap();
    let mut server_settings = [0; 9];
    stream.read_exact(&mut server_settings).unwrap();
    assert_eq!(server_settings[3], 4);
    let result = capture.finish();
    let frames = result["h2"]["frames"].as_array().unwrap();
    assert_eq!(frames[0]["settings"], json!([{"id": 4, "value": 65535}]));
    assert_eq!(frames[1]["increment"], 100);
    assert_eq!(
        frames[2]["headers"],
        json!([
            {"name": ":method", "value": "GET"},
            {"name": ":scheme", "value": "https"},
            {"name": ":path", "value": "<redacted>"}
        ])
    );
    assert_eq!(
        frames[2]["normalizations"],
        json!([{"header_index": 2, "reason": "unclassified"}])
    );
}

#[test]
fn websocket_upgrade_precedes_frame_and_captures_order_without_payload() {
    let capture = Capture::start();
    let mut stream = capture.connect();
    stream.write_all(b"GET /secret-websocket HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nAuthorization: Bearer ws-secret\r\n\r\n").unwrap();
    let response = response_headers(&mut stream);
    assert!(response.starts_with("HTTP/1.1 101 "), "{response}");
    assert!(
        response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "{response}"
    );
    let payload = b"private-frame";
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4]),
    );
    stream.write_all(&frame).unwrap();
    drop(stream);
    let result = capture.finish();
    let names: Vec<_> = result["h1"]["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|header| header["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "Host",
            "Upgrade",
            "Connection",
            "Sec-WebSocket-Key",
            "Sec-WebSocket-Version",
            "Authorization"
        ]
    );
    assert_eq!(
        result["ws"]["first_frame"],
        json!({"fin":true, "rsv":0, "opcode":1, "masked":true, "length":13, "payload":"<redacted>"})
    );
    assert_sanitized(
        &result,
        &[
            "secret-websocket",
            "ws-secret",
            "dGhlIHNhbXBsZSBub25jZQ==",
            "private-frame",
        ],
    );
}

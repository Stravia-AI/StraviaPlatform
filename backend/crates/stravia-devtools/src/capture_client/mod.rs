mod h2;
pub(super) mod hello;

use std::{
    io::Cursor,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::Args;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

/// Listen only on loopback and save a sanitized observation of one connection.
#[derive(Args)]
pub struct CaptureArgs {
    /// Loopback port; zero chooses an ephemeral port.
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Path to write the temporary public CA certificate (PEM).
    #[arg(long)]
    ca: PathBuf,
    /// Path to write the sanitized JSON observation.
    #[arg(long)]
    output: PathBuf,
    /// Name and version of the actual client being observed.
    #[arg(long)]
    client: String,
    #[arg(long)]
    client_version: String,
}

struct Replay {
    prefix: Cursor<Vec<u8>>,
    socket: TcpStream,
}

impl AsyncRead for Replay {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if (self.prefix.position() as usize) < self.prefix.get_ref().len() {
            let pos = self.prefix.position() as usize;
            let n = buf.remaining().min(self.prefix.get_ref().len() - pos);
            buf.put_slice(&self.prefix.get_ref()[pos..pos + n]);
            self.prefix.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.socket).poll_read(cx, buf)
    }
}
impl AsyncWrite for Replay {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.socket).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}

pub async fn run(args: CaptureArgs) -> Result<()> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name = rcgen::DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Stravia local capture CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
    ];
    let ca_key = KeyPair::generate()?;
    let ca = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_key = KeyPair::generate()?;
    let mut leaf_params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])?;
    leaf_params.distinguished_name = rcgen::DistinguishedName::new();
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    leaf_params.use_authority_key_identifier_extension = true;
    let leaf = leaf_params.signed_by(&leaf_key, &issuer)?;
    let private_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![leaf.der().clone()], private_key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, args.port)).await?;
    std::fs::write(&args.ca, ca.pem()).context("writing CA certificate")?;
    println!(
        "LISTEN https://{} CA {}",
        listener.local_addr()?,
        args.ca.display()
    );
    let (mut socket, _) = listener.accept().await?;
    let (prefix, clienthello) = hello::read(&mut socket).await?;
    let mut stream = TlsAcceptor::from(Arc::new(config))
        .accept(Replay {
            prefix: Cursor::new(prefix),
            socket,
        })
        .await?;
    let is_h2 = stream.get_ref().1.alpn_protocol() == Some(b"h2");
    let mut result = serde_json::json!({
        "provenance": {"client": args.client, "version": args.client_version, "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "captured_at": chrono::Utc::now().to_rfc3339()},
        "clienthello": clienthello
    });
    if is_h2 {
        result["h2"] = h2::capture(&mut stream).await?;
    } else {
        let (h1, ws) = capture_h1(&mut stream).await?;
        result["h1"] = h1;
        if let Some(ws) = ws {
            result["ws"] = ws;
        }
    }
    std::fs::write(&args.output, serde_json::to_vec_pretty(&result)?)
        .context("writing sanitized observation")?;
    Ok(())
}

fn sanitize_header_value<'a>(name: &str, value: &'a str) -> (&'a str, Option<&'static str>) {
    let name = name.to_ascii_lowercase();
    if [
        "authorization",
        "cookie",
        "credential",
        "secret",
        "token",
        "api-key",
        "apikey",
        "session",
        "nonce",
        "request-id",
        "request_id",
        "idempotency",
        "retry-count",
        "trace",
        "baggage",
        "timestamp",
        "websocket-key",
    ]
    .iter()
    .any(|part| name.contains(part))
    {
        return ("<redacted>", Some("credential_or_dynamic"));
    }
    let preserve = matches!(
        name.as_str(),
        ":method"
            | ":scheme"
            | "accept"
            | "accept-encoding"
            | "accept-language"
            | "content-type"
            | "connection"
            | "upgrade"
            | "sec-websocket-version"
            | "sec-websocket-extensions"
            | "sec-websocket-protocol"
            | "user-agent"
            | "anthropic-version"
            | "anthropic-beta"
            | "anthropic-dangerous-direct-browser-access"
            | "openai-beta"
            | "openai-version"
            | "x-codex-beta-features"
            | "originator"
            | "x-app"
    ) || name.starts_with("x-stainless-")
        || name.starts_with("x-client-")
        || name.starts_with("x-sdk-");
    if preserve {
        (value, None)
    } else {
        ("<redacted>", Some("unclassified"))
    }
}

async fn capture_h1<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<(serde_json::Value, Option<serde_json::Value>)> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 65536 {
            bail!("HTTP headers exceed 64 KiB");
        }
        bytes.push(stream.read_u8().await?);
    }
    let text = std::str::from_utf8(&bytes).context("non-UTF-8 HTTP/1 header")?;
    let mut lines = text.split("\r\n");
    let request = lines.next().context("missing request line")?;
    let mut parts = request.split_whitespace();
    let method = parts.next().context("missing method")?;
    let _target = parts.next().context("missing request target")?;
    let version = parts.next().context("missing HTTP version")?;
    let target_start = request
        .find(char::is_whitespace)
        .context("invalid request line")?;
    let target_start =
        target_start + request[target_start..].len() - request[target_start..].trim_start().len();
    let target_end = target_start
        + request[target_start..]
            .find(char::is_whitespace)
            .context("invalid request line")?;
    let mut raw_head = format!(
        "{}<redacted>{}\r\n",
        &request[..target_start],
        &request[target_end..]
    );
    let mut headers = Vec::new();
    let mut normalizations = Vec::new();
    let mut websocket_key = None;
    let mut upgrade = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("malformed HTTP/1 header")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
        {
            bail!("invalid HTTP header name");
        }
        let raw_value = value;
        let value = raw_value.trim();
        if name.eq_ignore_ascii_case("sec-websocket-key") {
            websocket_key = Some(value.to_owned());
        }
        if name.eq_ignore_ascii_case("upgrade") && value.eq_ignore_ascii_case("websocket") {
            upgrade = true;
        }
        let (safe, reason) = sanitize_header_value(name, value);
        if let Some(reason) = reason {
            normalizations
                .push(serde_json::json!({"header_index": headers.len(), "reason": reason}));
        }
        raw_head.push_str(name);
        raw_head.push(':');
        if reason.is_some() {
            let leading = raw_value.len() - raw_value.trim_start().len();
            let end = raw_value.trim_end().len().max(leading);
            raw_head.push_str(&raw_value[..leading]);
            raw_head.push_str("<redacted>");
            raw_head.push_str(&raw_value[end..]);
        } else {
            raw_head.push_str(raw_value);
        }
        raw_head.push_str("\r\n");
        headers.push(serde_json::json!({"name": name, "value": safe}));
    }
    raw_head.push_str("\r\n");
    let h1 = serde_json::json!({"method": method, "target": "<redacted>", "version": version, "headers": headers, "normalizations": normalizations, "raw_head": raw_head});
    if upgrade {
        let key = websocket_key.context("websocket upgrade without key")?;
        let mut hash = Sha1::new();
        hash.update(key.as_bytes());
        hash.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let accept = STANDARD.encode(hash.finalize());
        stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await?;
        stream.flush().await?;
        let first = stream.read_u8().await?;
        let second = stream.read_u8().await?;
        let len = match second & 0x7f {
            126 => stream.read_u16().await? as u64,
            127 => stream.read_u64().await?,
            n => u64::from(n),
        };
        if len > 16 * 1024 * 1024 {
            bail!("websocket frame exceeds limit");
        }
        if second & 0x80 != 0 {
            let mut mask = [0; 4];
            stream.read_exact(&mut mask).await?;
        }
        let mut remaining = len;
        let mut scratch = [0u8; 4096];
        while remaining > 0 {
            let take = remaining.min(scratch.len() as u64) as usize;
            stream.read_exact(&mut scratch[..take]).await?;
            remaining -= take as u64;
        }
        Ok((
            h1,
            Some(
                serde_json::json!({"first_frame": {"fin": first & 0x80 != 0, "rsv": (first >> 4) & 7, "opcode": first & 15, "masked": second & 0x80 != 0, "length": len, "payload": "<redacted>"}}),
            ),
        ))
    } else {
        stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        stream.flush().await?;
        Ok((h1, None))
    }
}

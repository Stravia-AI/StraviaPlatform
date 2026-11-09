use anyhow::{Context, Result, bail, ensure};
use md5::Md5;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

// Bound both the amount buffered and the number of empty or tiny records.
const MAX_RECORD: usize = 18_432;
const MAX_HELLO: usize = 256 * 1024;
const MAX_RECORDS: usize = 256;

/// Read whole TLS records through the first ClientHello. The returned bytes must
/// be replayed unchanged, including any subsequent messages in the last record.
pub(super) async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(Vec<u8>, Value)> {
    read_checked(stream, None).await
}

pub(crate) async fn read_for_server_name<S: AsyncRead + Unpin>(
    stream: &mut S,
    server_name: &str,
) -> Result<(Vec<u8>, Value)> {
    read_checked(stream, Some(server_name)).await
}

async fn read_checked<S: AsyncRead + Unpin>(
    stream: &mut S,
    server_name: Option<&str>,
) -> Result<(Vec<u8>, Value)> {
    let mut replay = Vec::new();
    let mut handshake = Vec::new();
    for _ in 0..MAX_RECORDS {
        let mut header = [0; 5];
        stream
            .read_exact(&mut header)
            .await
            .context("reading TLS record header")?;
        ensure!(header[0] == 22, "expected TLS handshake record");
        let length = u16::from_be_bytes([header[3], header[4]]) as usize;
        ensure!(length <= MAX_RECORD, "TLS record exceeds size limit");
        ensure!(
            replay.len() + 5 + length <= MAX_HELLO,
            "ClientHello exceeds size limit"
        );
        replay.extend_from_slice(&header);
        let start = replay.len();
        replay.resize(start + length, 0);
        stream
            .read_exact(&mut replay[start..])
            .await
            .context("reading TLS record")?;
        handshake.extend_from_slice(&replay[start..]);
        if handshake.len() >= 4 {
            ensure!(handshake[0] == 1, "first TLS handshake is not ClientHello");
            let length = ((handshake[1] as usize) << 16)
                | ((handshake[2] as usize) << 8)
                | handshake[3] as usize;
            ensure!(length + 4 <= MAX_HELLO, "ClientHello exceeds size limit");
            if handshake.len() >= length + 4 {
                return Ok((replay, decode(&handshake[4..4 + length], server_name)?));
            }
        }
    }
    bail!("too many TLS records in ClientHello")
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("invalid TLS length")?;
        ensure!(end <= self.bytes.len(), "truncated ClientHello");
        let result = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(result)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn vec8(&mut self) -> Result<&'a [u8]> {
        let n = self.u8()? as usize;
        self.take(n)
    }
    fn vec16(&mut self) -> Result<&'a [u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }
    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }
    fn finish(&self) -> Result<()> {
        ensure!(self.done(), "invalid ClientHello vector length");
        Ok(())
    }
}

fn words(bytes: &[u8]) -> Result<Vec<u16>> {
    ensure!(bytes.len().is_multiple_of(2), "invalid TLS u16 vector");
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u16::from_be_bytes([v[0], v[1]]))
        .collect())
}
fn grease(n: u16) -> bool {
    n & 0x0f0f == 0x0a0a && (n >> 8) as u8 == n as u8
}
fn decimal(values: impl IntoIterator<Item = u16>) -> String {
    values
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("-")
}
fn hex_words(values: &[u16]) -> String {
    values
        .iter()
        .map(|v| format!("{v:04x}"))
        .collect::<Vec<_>>()
        .join(",")
}
fn hash12(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "000000000000".to_owned();
    }
    let digest = Sha256::digest(bytes);
    digest[..6].iter().map(|v| format!("{v:02x}")).collect()
}
fn version_code(version: u16) -> &'static str {
    match version {
        0x0304 => "13",
        0x0303 => "12",
        0x0302 => "11",
        0x0301 => "10",
        0x0300 => "s3",
        _ => "00",
    }
}
// ALPN identifiers are opaque peer input, so only recognized values are emitted.
fn safe_alpn(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        b"h2" => Some("h2"),
        b"http/1.1" => Some("http/1.1"),
        b"http/1.0" => Some("http/1.0"),
        b"h3" => Some("h3"),
        b"h3-29" => Some("h3-29"),
        b"acme-tls/1" => Some("acme-tls/1"),
        _ => None,
    }
}

// Only retain bodies whose complete structure is known not to carry opaque data.
fn safe_extension_body(kind: u16, payload: &[u8]) -> bool {
    match kind {
        // status_request with no responder IDs or request extensions.
        5 => payload == [1, 0, 0, 0, 0],
        10 | 11 | 13 | 27 | 43 | 50 => true, // parsed and length-checked below
        16 => {
            let mut outer = Cursor::new(payload);
            let Ok(list) = outer.vec16() else {
                return false;
            };
            if !outer.done() {
                return false;
            }
            let mut protocols = Cursor::new(list);
            while !protocols.done() {
                let Ok(protocol) = protocols.vec8() else {
                    return false;
                };
                if safe_alpn(protocol).is_none() {
                    return false;
                }
            }
            true
        }
        18 | 22 | 23 => payload.is_empty(),
        21 => payload.iter().all(|byte| *byte == 0),
        45 => {
            let mut modes = Cursor::new(payload);
            match modes.vec8() {
                Ok(values) => modes.done() && values.iter().all(|v| *v <= 1),
                Err(_) => false,
            }
        }
        // Initial renegotiation indication only, never dynamic verify data.
        65281 => payload == [0],
        _ => false,
    }
}

fn decode(bytes: &[u8], expected_server_name: Option<&str>) -> Result<Value> {
    let mut c = Cursor::new(bytes);
    let legacy_version = c.u16()?;
    c.take(32)?; // random: never expose
    c.vec8()?; // session ID: never expose
    let cipher_suites = words(c.vec16()?)?;
    c.vec8()?; // compression methods
    let mut extensions = Vec::new();
    let mut extension_details: Vec<Value> = Vec::new();
    let mut groups = Vec::new();
    let mut point_formats = Vec::new();
    let mut signature_algorithms = Vec::new();
    let mut supported_versions = Vec::new();
    let mut key_share_groups = Vec::new();
    let mut key_shares = Vec::new();
    let mut signature_algorithms_cert = Vec::new();
    let mut certificate_compression = Vec::new();
    let mut psk_modes = Vec::new();
    let mut matched_server_name = false;
    let mut alpn = Vec::new();
    let mut first_alpn = None;
    let mut sni_present = false;
    let mut padding_length = None;
    if !c.done() {
        let mut ext = Cursor::new(c.vec16()?);
        c.finish()?;
        while !ext.done() {
            let kind = ext.u16()?;
            let payload = ext.vec16()?;
            extensions.push(kind);
            let mut p = Cursor::new(payload);
            let mut decoded = None;
            match kind {
                0 => {
                    // Server names are sensitive, even for malformed or unknown name types.
                    sni_present = true;
                    let mut names = Cursor::new(p.vec16()?);
                    p.finish()?;
                    while !names.done() {
                        let kind = names.u8()?;
                        let name = names.vec16()?;
                        if kind == 0
                            && expected_server_name
                                .is_some_and(|expected| name == expected.as_bytes())
                        {
                            matched_server_name = true;
                        }
                    }
                    decoded = Some(json!({"server_name_present": true}));
                }
                10 => {
                    groups = words(p.vec16()?)?;
                    p.finish()?;
                    decoded = Some(json!({"groups": groups}));
                }
                11 => {
                    point_formats = p.vec8()?.to_vec();
                    p.finish()?;
                    decoded = Some(json!({"point_formats": point_formats}));
                }
                13 => {
                    signature_algorithms = words(p.vec16()?)?;
                    p.finish()?;
                    decoded = Some(json!({"algorithms": signature_algorithms}));
                }
                5 => {
                    decoded = Some(json!({"status_request": true}));
                }
                18 => {
                    decoded = Some(json!({"signed_certificate_timestamp": true}));
                }
                22 => {
                    decoded = Some(json!({"encrypt_then_mac": true}));
                }
                23 => {
                    decoded = Some(json!({"extended_master_secret": true}));
                }
                45 if safe_extension_body(kind, payload) => {
                    psk_modes = payload[1..].to_vec();
                    decoded = Some(json!({"psk_key_exchange_modes": psk_modes}));
                }
                65281 => {
                    decoded = Some(json!({"renegotiation_info": true}));
                }
                27 => {
                    certificate_compression = words(p.vec8()?)?;
                    p.finish()?;
                    decoded = Some(json!({"algorithms": certificate_compression}));
                }
                50 => {
                    signature_algorithms_cert = words(p.vec16()?)?;
                    p.finish()?;
                    decoded = Some(json!({"algorithms": signature_algorithms_cert}));
                }
                16 => {
                    let mut protocols = Cursor::new(p.vec16()?);
                    p.finish()?;
                    while !protocols.done() {
                        let protocol = protocols.vec8()?;
                        ensure!(!protocol.is_empty(), "empty ALPN identifier");
                        let safe = safe_alpn(protocol);
                        if first_alpn.is_none() {
                            first_alpn = Some(safe);
                        }
                        if let Some(name) = safe {
                            alpn.push(name);
                        }
                    }
                }
                21 => {
                    padding_length = Some(payload.len());
                    decoded = Some(json!({"padding_length": payload.len()}));
                }
                43 => {
                    supported_versions = words(p.vec8()?)?;
                    p.finish()?;
                    decoded = Some(json!({"versions": supported_versions}));
                }
                51 => {
                    let mut shares = Cursor::new(p.vec16()?);
                    p.finish()?;
                    while !shares.done() {
                        let group = shares.u16()?;
                        let key = shares.vec16()?; // key exchange bytes are never exposed
                        key_share_groups.push(group);
                        key_shares.push(json!({"group": group, "length": key.len()}));
                    }
                    decoded = Some(json!({"groups": key_share_groups, "shares": key_shares}));
                }
                _ => {} // Unknown extension data can contain secrets.
            }
            let safe_body = safe_extension_body(kind, payload);
            extension_details.push(json!({
                "type": kind,
                "length": payload.len(),
                "decoded": decoded,
                "body": if safe_body { Some(payload) } else { None },
                "payload_omitted": if safe_body { None } else { Some("extension payload may contain sensitive or unrecognized data") }
            }));
        }
    }
    ensure!(
        expected_server_name.is_none() || matched_server_name,
        "ClientHello SNI does not match server name"
    );
    let filtered_ciphers: Vec<_> = cipher_suites
        .iter()
        .copied()
        .filter(|n| !grease(*n))
        .collect();
    let filtered_ext: Vec<_> = extensions.iter().copied().filter(|n| !grease(*n)).collect();
    let ja3 = format!(
        "{},{},{},{},{}",
        legacy_version,
        decimal(filtered_ciphers.iter().copied()),
        decimal(filtered_ext.iter().copied()),
        decimal(groups.iter().copied().filter(|n| !grease(*n))),
        decimal(point_formats.iter().map(|v| *v as u16))
    );
    let ja3_hash = Md5::digest(ja3.as_bytes())
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let version = supported_versions
        .iter()
        .copied()
        .filter(|n| !grease(*n))
        .max()
        .unwrap_or(legacy_version);
    let alpn_code = match first_alpn.flatten() {
        Some(name) => {
            let b = name.as_bytes();
            format!("{}{}", b[0] as char, b[b.len() - 1] as char)
        }
        None => "00".to_owned(),
    };
    let mut sorted_ciphers = filtered_ciphers.clone();
    sorted_ciphers.sort_unstable();
    let mut sorted_ext: Vec<_> = filtered_ext
        .iter()
        .copied()
        .filter(|n| *n != 0 && *n != 16)
        .collect();
    sorted_ext.sort_unstable();
    let mut ext_input = hex_words(&sorted_ext);
    let ja4_signatures: Vec<_> = signature_algorithms
        .iter()
        .copied()
        .filter(|n| !grease(*n))
        .collect();
    if !ja4_signatures.is_empty() {
        ext_input.push('_');
        ext_input.push_str(&hex_words(&ja4_signatures));
    }
    let ja4 = format!(
        "t{}{}{:02}{:02}{}_{}_{}",
        version_code(version),
        if sni_present { 'd' } else { 'i' },
        filtered_ciphers.len().min(99),
        filtered_ext.len().min(99),
        alpn_code,
        hash12(hex_words(&sorted_ciphers).as_bytes()),
        hash12(ext_input.as_bytes())
    );
    Ok(json!({
        "legacy_version": legacy_version, "cipher_suites": cipher_suites,
        "extensions": extensions, "extension_details": extension_details,
        "supported_groups": groups, "ec_point_formats": point_formats,
        "signature_algorithms": signature_algorithms, "supported_versions": supported_versions,
        "key_share_groups": key_share_groups, "key_shares": key_shares,
        "signature_algorithms_cert": signature_algorithms_cert,
        "certificate_compression": certificate_compression, "psk_modes": psk_modes,
        "padding_length": padding_length,
        "sni_present": sni_present, "alpn": alpn, "ja3": ja3, "ja3_hash": ja3_hash, "ja4": ja4
    }))
}

#[cfg(test)]
mod tests {
    use super::read;

    fn record(data: &[u8]) -> Vec<u8> {
        let mut record = vec![22, 3, 1];
        record.extend_from_slice(&(data.len() as u16).to_be_bytes());
        record.extend_from_slice(data);
        record
    }

    #[tokio::test]
    async fn fragmented_hello_is_replayed_and_sensitive_values_are_not_reported() {
        let mut body = vec![3, 3];
        body.extend_from_slice(&[42; 32]);
        body.extend_from_slice(&[3, 1, 2, 3]); // session ID
        body.extend_from_slice(&[0, 4, 0x0a, 0x0a, 0x13, 0x01]);
        body.extend_from_slice(&[1, 0]);
        let mut extensions = Vec::new();
        // The host name, unknown extension, and key share must never be output.
        extensions.extend_from_slice(&[0, 0, 0, 12, 0, 10, 0, 0, 7]);
        extensions.extend_from_slice(b"example");
        extensions.extend_from_slice(&[0, 16, 0, 5, 0, 3, 2, b'h', b'2']);
        extensions.extend_from_slice(&[0, 43, 0, 3, 2, 3, 4]);
        extensions.extend_from_slice(&[0, 51, 0, 8, 0, 6, 0, 29, 0, 2, 0xaa, 0xbb]);
        extensions.extend_from_slice(&[0, 21, 0, 2, 0, 0]);
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let mut hello = vec![1, 0, 0, body.len() as u8];
        hello.extend_from_slice(&body);
        let mut wire = record(&hello[..2]);
        let mut last = hello[2..].to_vec();
        last.extend_from_slice(&[2, 0, 0, 0]); // another handshake in same record
        wire.extend_from_slice(&record(&last));
        let mut input = wire.as_slice();
        let (replay, facts) = read(&mut input).await.expect("valid ClientHello");
        assert_eq!(replay, wire);
        assert_eq!(facts["ja3"], "771,4865,0-16-43-51-21,,");
        assert_eq!(facts["supported_versions"], serde_json::json!([772]));
        assert_eq!(facts["key_share_groups"], serde_json::json!([29]));
        assert_eq!(facts["padding_length"], 2);
        assert_eq!(facts["alpn"], serde_json::json!(["h2"]));
        assert!(facts["ja4"].as_str().unwrap().starts_with("t13d0105h2_"));
        assert!(!facts.to_string().contains("example"));
        assert!(!facts.to_string().contains("aabb"));
    }

    #[tokio::test]
    async fn rejects_truncated_and_oversized_records() {
        let mut truncated = [22, 3, 1, 0, 2, 1].as_slice();
        assert!(read(&mut truncated).await.is_err());
        let mut oversized = [22, 3, 1, 0xff, 0xff].as_slice();
        assert!(read(&mut oversized).await.is_err());
    }
}

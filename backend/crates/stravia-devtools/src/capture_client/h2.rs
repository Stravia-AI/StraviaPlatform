use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const MAX_FRAME: usize = 16_384;
const MAX_HEADER_BLOCK: usize = 1_048_576;

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Frame> {
    let mut header = [0; 9];
    stream.read_exact(&mut header).await?;
    let length = ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | header[2] as usize;
    ensure!(
        length <= MAX_FRAME,
        "HTTP/2 frame exceeds advertised maximum"
    );
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    Ok(Frame {
        kind: header[3],
        flags: header[4],
        stream: u32::from_be_bytes([header[5], header[6], header[7], header[8]]) & 0x7fff_ffff,
        payload,
    })
}

async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    kind: u8,
    flags: u8,
    id: u32,
    payload: &[u8],
) -> Result<()> {
    let length = payload.len();
    let id = id.to_be_bytes();
    let header = [
        (length >> 16) as u8,
        (length >> 8) as u8,
        length as u8,
        kind,
        flags,
        id[0],
        id[1],
        id[2],
        id[3],
    ];
    stream.write_all(&header).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

fn priority(payload: &[u8]) -> Value {
    let dependency = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    json!({"exclusive": dependency & 0x8000_0000 != 0, "stream_dependency": dependency & 0x7fff_ffff, "weight": u16::from(payload[4]) + 1})
}

fn header_name(bytes: &[u8]) -> Result<&str> {
    let name = std::str::from_utf8(bytes)?;
    let token = name.strip_prefix(':').unwrap_or(name);
    ensure!(!token.is_empty() && token.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')), "invalid HTTP/2 header name");
    Ok(name)
}

/// Capture the first request and all frames leading up to its complete header block.
pub(super) async fn capture<S>(stream: &mut S) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut preface = [0; 24];
    stream.read_exact(&mut preface).await?;
    ensure!(&preface == PREFACE, "invalid HTTP/2 client preface");
    // An empty SETTINGS frame advertises the protocol defaults.
    write_frame(stream, 4, 0, 0, &[]).await?;
    let mut frames = Vec::new();
    let mut decoder = hpack::Decoder::new();
    let mut first = true;
    loop {
        let frame = read_frame(stream).await?;
        if first {
            ensure!(
                frame.kind == 4 && frame.stream == 0 && frame.flags & 1 == 0,
                "expected HTTP/2 client SETTINGS"
            );
            first = false;
        }
        match frame.kind {
            4 => {
                ensure!(frame.stream == 0, "SETTINGS on a stream");
                if frame.flags & 1 != 0 {
                    ensure!(frame.payload.is_empty(), "invalid SETTINGS acknowledgement");
                    frames.push(json!({"type": "settings_ack"}));
                } else {
                    ensure!(frame.payload.len().is_multiple_of(6), "invalid SETTINGS length");
                    let settings: Vec<_> = frame.payload.as_chunks::<6>().0.iter().map(|s| json!({"id": u16::from_be_bytes([s[0], s[1]]), "value": u32::from_be_bytes([s[2], s[3], s[4], s[5]])})).collect();
                    frames.push(json!({"type": "settings", "settings": settings}));
                    write_frame(stream, 4, 1, 0, &[]).await?;
                }
            }
            8 => {
                ensure!(frame.payload.len() == 4, "invalid WINDOW_UPDATE length");
                let increment = u32::from_be_bytes(frame.payload[..4].try_into()?) & 0x7fff_ffff;
                ensure!(increment != 0, "invalid WINDOW_UPDATE increment");
                frames.push(json!({"type": "window_update", "stream_id": frame.stream, "increment": increment}));
            }
            2 => {
                ensure!(frame.stream != 0 && frame.payload.len() == 5, "invalid PRIORITY frame");
                frames.push(json!({"type": "priority", "stream_id": frame.stream, "priority": priority(&frame.payload)}));
            }
            1 => {
                ensure!(frame.stream != 0, "HEADERS on connection stream");
                let mut start = 0;
                let padding = if frame.flags & 8 != 0 {
                    ensure!(!frame.payload.is_empty(), "missing HEADERS padding length");
                    start = 1;
                    frame.payload[0] as usize
                } else { 0 };
                let priority_value = if frame.flags & 0x20 != 0 {
                    ensure!(frame.payload.len() >= start + 5, "truncated HEADERS priority");
                    let value = priority(&frame.payload[start..start + 5]);
                    start += 5;
                    Some(value)
                } else { None };
                ensure!(padding <= frame.payload.len().saturating_sub(start), "invalid HEADERS padding");
                let mut block = frame.payload[start..frame.payload.len() - padding].to_vec();
                let mut continuations = Vec::new();
                let mut flags = frame.flags;
                while flags & 4 == 0 {
                    let next = read_frame(stream).await?;
                    ensure!(next.kind == 9 && next.stream == frame.stream, "expected CONTINUATION on the same stream");
                    ensure!(block.len() + next.payload.len() <= MAX_HEADER_BLOCK, "HTTP/2 header block too large");
                    block.extend_from_slice(&next.payload);
                    flags = next.flags;
                    continuations.push(json!({"type": "continuation", "stream_id": next.stream, "end_headers": flags & 4 != 0}));
                }
                let decoded = decoder.decode(&block).map_err(|_| anyhow::anyhow!("invalid HPACK header block"))?;
                let mut headers = Vec::with_capacity(decoded.len());
                let mut normalizations = Vec::new();
                for (name, value) in decoded {
                    let name = header_name(&name)?;
                    let (safe, reason) = std::str::from_utf8(&value).map(|v| super::sanitize_header_value(name, v)).unwrap_or(("<redacted>", Some("non_utf8")));
                    if let Some(reason) = reason { normalizations.push(json!({"header_index": headers.len(), "reason": reason})); }
                    headers.push(json!({"name": name, "value": safe}));
                }
                frames.push(json!({"type": "headers", "stream_id": frame.stream, "end_stream": frame.flags & 1 != 0, "end_headers": frame.flags & 4 != 0, "priority": priority_value, "headers": headers, "normalizations": normalizations}));
                frames.extend(continuations);
                // Indexed static-table :status 404, followed by END_STREAM. No request body is read or stored.
                write_frame(stream, 1, 5, frame.stream, &[0x8d]).await?;
                return Ok(json!({"frames": frames}));
            }
            6 => {
                ensure!(frame.stream == 0 && frame.payload.len() == 8, "invalid PING frame");
                frames.push(json!({"type": "ping", "ack": frame.flags & 1 != 0}));
                if frame.flags & 1 == 0 {
                    write_frame(stream, 6, 1, 0, &frame.payload).await?;
                }
            }
            9 => bail!("unexpected CONTINUATION frame"),
            7 => bail!("client closed HTTP/2 connection before request"),
            _ => frames.push(json!({"type": "other", "frame_type": frame.kind, "flags": frame.flags, "stream_id": frame.stream, "length": frame.payload.len()})),
        }
    }
}

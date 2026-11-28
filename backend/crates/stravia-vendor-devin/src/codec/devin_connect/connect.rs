//! Connect-RPC envelope framing for `application/connect+proto`.
//!
//! Frame layout: `[1 byte flags] [4 bytes big-endian length] [payload]`.
//! Flags: `0x01` gzip payload, `0x02` end-of-stream trailer (JSON),
//! `0x03` = both. Matches `dwgx/WindsurfAPI` `src/connect.js`.

use std::borrow::Cow;
use std::io::Read;

use anyhow::{Context, bail};

/// Wire and decompressed ceiling for a single frame. Bounding the inflated
/// size is what stops a high-ratio gzip frame from exhausting memory.
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const HEADER_SIZE: usize = 5;

const FLAG_COMPRESSED: u8 = 0x01;
const FLAG_END_STREAM: u8 = 0x02;

/// Devin CLI 3000.15.14 compresses Connect-RPC request frames with gzip when
/// the serialized protobuf reaches 8192 bytes (`0x2000`) and emits
/// `connect-content-encoding: gzip`. Smaller frames stay uncompressed.
pub const REQUEST_GZIP_THRESHOLD: usize = 8192;

#[derive(Debug, PartialEq, Eq)]
pub struct FramedRequest {
    pub body: Vec<u8>,
    pub compressed: bool,
}

fn gzip(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(payload).context("gzip request frame")?;
    encoder.finish().context("finish gzip request frame")
}

fn gunzip(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    decoder
        .take(MAX_FRAME_SIZE as u64 + 1)
        .read_to_end(&mut out)
        .context("decompress Connect frame")?;
    if out.len() > MAX_FRAME_SIZE {
        bail!("Connect frame exceeds {MAX_FRAME_SIZE} bytes after decompression");
    }
    Ok(out)
}

/// Wrap a serialized protobuf message in the single request envelope.
/// Matches Devin CLI 3000.15.14: payloads of at least [`REQUEST_GZIP_THRESHOLD`]
/// bytes are gzip-compressed (`flag = 0x01`); shorter payloads stay raw
/// (`flag = 0x00`). No end-of-stream frame is appended.
pub fn wrap_request(proto: &[u8]) -> anyhow::Result<FramedRequest> {
    let (flags, payload): (u8, Cow<'_, [u8]>) = if proto.len() >= REQUEST_GZIP_THRESHOLD {
        match gzip(proto) {
            Ok(compressed) => (FLAG_COMPRESSED, Cow::Owned(compressed)),
            Err(_) => (0u8, Cow::Borrowed(proto)),
        }
    } else {
        (0u8, Cow::Borrowed(proto))
    };
    let len = u32::try_from(payload.len()).context("Connect request frame exceeds u32 length")?;
    let mut body = Vec::with_capacity(HEADER_SIZE + payload.len());
    body.push(flags);
    body.extend_from_slice(&len.to_be_bytes());
    body.extend_from_slice(&payload);
    Ok(FramedRequest {
        body,
        compressed: flags & FLAG_COMPRESSED != 0,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConnectFrame<'a> {
    /// A protobuf `GetChatMessageResponse` payload.
    Data(Cow<'a, [u8]>),
    /// Terminal trailer; payload is JSON (`{}` on success, an error object
    /// otherwise).
    EndStream(Cow<'a, [u8]>),
}

/// Incremental splitter for the Connect byte stream. Handles headers and
/// payloads split arbitrarily across transport chunks; decompression happens
/// once a complete frame is present.
pub(crate) struct ConnectFrameReader {
    buffer: Vec<u8>,
    consumed: usize,
}

impl ConnectFrameReader {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            consumed: 0,
        }
    }

    /// Append bytes and yield every complete frame now available.
    pub(crate) fn push(&mut self, raw: &[u8]) -> anyhow::Result<Vec<ConnectFrame<'_>>> {
        self.buffer.drain(..self.consumed);
        self.consumed = 0;
        self.buffer.extend_from_slice(raw);
        let mut frames = Vec::new();
        loop {
            let pending = &self.buffer[self.consumed..];
            if pending.len() < HEADER_SIZE {
                break;
            }
            let len = u32::from_be_bytes(
                pending[1..HEADER_SIZE]
                    .try_into()
                    .expect("four-byte frame length"),
            ) as usize;
            if len > MAX_FRAME_SIZE {
                bail!("Connect frame size {len} exceeds {MAX_FRAME_SIZE}");
            }
            if pending.len() < HEADER_SIZE + len {
                break;
            }
            let flags = pending[0];
            let payload = &pending[HEADER_SIZE..HEADER_SIZE + len];
            self.consumed += HEADER_SIZE + len;
            let payload = if flags & FLAG_COMPRESSED != 0 {
                Cow::Owned(gunzip(payload)?)
            } else {
                Cow::Borrowed(payload)
            };
            frames.push(if flags & FLAG_END_STREAM != 0 {
                ConnectFrame::EndStream(payload)
            } else {
                ConnectFrame::Data(payload)
            });
        }
        Ok(frames)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buffer.len() == self.consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![flags];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn request_envelope_stays_uncompressed_below_threshold() {
        let payload = vec![b'a'; REQUEST_GZIP_THRESHOLD - 1];
        let framed = wrap_request(&payload).unwrap();
        assert!(!framed.compressed);
        assert_eq!(framed.body[0], 0);
        let len = u32::from_be_bytes(framed.body[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, framed.body.len() - HEADER_SIZE);
        assert_eq!(&framed.body[5..], payload.as_slice());
    }

    #[test]
    fn request_envelope_compresses_at_and_above_threshold() {
        let payload = vec![b'a'; REQUEST_GZIP_THRESHOLD];
        let framed = wrap_request(&payload).unwrap();
        assert!(framed.compressed);
        assert_eq!(framed.body[0], FLAG_COMPRESSED);
        let len = u32::from_be_bytes(framed.body[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, framed.body.len() - HEADER_SIZE);
        assert!(len < payload.len());
        let mut reader = ConnectFrameReader::new();
        assert_eq!(
            reader.push(&framed.body).unwrap(),
            vec![ConnectFrame::Data(Cow::Owned(payload))]
        );
    }

    #[test]
    fn reader_splits_frames_across_chunks() {
        let mut reader = ConnectFrameReader::new();
        let mut wire = raw_frame(0, b"abc");
        wire.extend_from_slice(&raw_frame(FLAG_END_STREAM, b"{}"));
        assert!(reader.push(&wire[..4]).unwrap().is_empty());
        assert!(reader.push(&wire[4..7]).unwrap().is_empty());
        assert_eq!(
            reader.push(&wire[7..]).unwrap(),
            vec![
                ConnectFrame::Data(b"abc".as_slice().into()),
                ConnectFrame::EndStream(b"{}".as_slice().into()),
            ]
        );
        assert!(reader.is_empty());
    }

    #[test]
    fn reader_decompresses_gzip_frames() {
        let payload = gzip(b"deflated").unwrap();
        let wire = raw_frame(FLAG_COMPRESSED, &payload);
        let mut reader = ConnectFrameReader::new();
        assert_eq!(
            reader.push(&wire).unwrap(),
            vec![ConnectFrame::Data(b"deflated".as_slice().into())]
        );
    }

    #[test]
    fn reader_rejects_oversized_frame_length() {
        let mut wire = vec![0u8];
        wire.extend_from_slice(&((MAX_FRAME_SIZE as u32) + 1).to_be_bytes());
        let mut reader = ConnectFrameReader::new();
        assert!(reader.push(&wire).is_err());
    }

    #[test]
    fn reader_keeps_partial_frame_buffered() {
        let wire = raw_frame(0, b"payload");
        let mut reader = ConnectFrameReader::new();
        assert!(reader.push(&wire[..6]).unwrap().is_empty());
        assert!(!reader.is_empty());
        assert_eq!(
            reader.push(&wire[6..]).unwrap(),
            vec![ConnectFrame::Data(b"payload".as_slice().into())]
        );
    }
}

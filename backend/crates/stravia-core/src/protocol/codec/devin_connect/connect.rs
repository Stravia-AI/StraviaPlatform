//! Connect-RPC envelope framing for `application/connect+proto`.
//!
//! Frame layout: `[1 byte flags] [4 bytes big-endian length] [payload]`.
//! Flags: `0x01` gzip payload, `0x02` end-of-stream trailer (JSON),
//! `0x03` = both. Matches `dwgx/WindsurfAPI` `src/connect.js`.

use std::io::Read;

use anyhow::{Context, bail};

/// Wire and decompressed ceiling for a single frame. Bounding the inflated
/// size is what stops a high-ratio gzip frame from exhausting memory.
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const HEADER_SIZE: usize = 5;

const FLAG_COMPRESSED: u8 = 0x01;
const FLAG_END_STREAM: u8 = 0x02;

#[cfg(test)]
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
/// The GetChatMessage path in the reference client sends the frame
/// uncompressed (`wrapEnvelope(proto, { compress: false })`, flag `0x00`)
/// and appends no end-of-stream frame.
pub(crate) fn wrap_request(proto: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(HEADER_SIZE + proto.len());
    frame.push(0u8);
    frame.extend_from_slice(&(proto.len() as u32).to_be_bytes());
    frame.extend_from_slice(proto);
    Ok(frame)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConnectFrame {
    /// A protobuf `GetChatMessageResponse` payload.
    Data(Vec<u8>),
    /// Terminal trailer; payload is JSON (`{}` on success, an error object
    /// otherwise).
    EndStream(Vec<u8>),
}

/// Incremental splitter for the Connect byte stream. Handles headers and
/// payloads split arbitrarily across transport chunks; decompression happens
/// once a complete frame is present.
pub(crate) struct ConnectFrameReader {
    buffer: Vec<u8>,
}

impl ConnectFrameReader {
    pub(crate) fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Append bytes and yield every complete frame now available.
    pub(crate) fn push(&mut self, raw: &[u8]) -> anyhow::Result<Vec<ConnectFrame>> {
        let mut frames = Vec::new();
        self.push_with(raw, |frame| {
            frames.push(frame);
            Ok(())
        })?;
        Ok(frames)
    }

    /// 诊断调用方可保留错误发生前的完整帧；执行解析仍使用原有原子返回接口。
    pub(crate) fn push_with(
        &mut self,
        raw: &[u8],
        mut visit: impl FnMut(ConnectFrame) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.buffer.extend_from_slice(raw);
        loop {
            if self.buffer.len() < HEADER_SIZE {
                break;
            }
            let len = u32::from_be_bytes(
                self.buffer[1..HEADER_SIZE]
                    .try_into()
                    .expect("four-byte frame length"),
            ) as usize;
            if len > MAX_FRAME_SIZE {
                bail!("Connect frame size {len} exceeds {MAX_FRAME_SIZE}");
            }
            if self.buffer.len() < HEADER_SIZE + len {
                break;
            }
            let flags = self.buffer[0];
            let payload: Vec<u8> = self
                .buffer
                .drain(..HEADER_SIZE + len)
                .skip(HEADER_SIZE)
                .collect();
            let payload = if flags & FLAG_COMPRESSED != 0 {
                gunzip(&payload)?
            } else {
                payload
            };
            visit(if flags & FLAG_END_STREAM != 0 {
                ConnectFrame::EndStream(payload)
            } else {
                ConnectFrame::Data(payload)
            })?;
        }
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buffer.is_empty()
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
    fn request_envelope_is_single_uncompressed_frame() {
        let frame = wrap_request(b"hello protobuf").unwrap();
        assert_eq!(frame[0], 0);
        let len = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - HEADER_SIZE);
        assert_eq!(&frame[5..], b"hello protobuf");
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
                ConnectFrame::Data(b"abc".to_vec()),
                ConnectFrame::EndStream(b"{}".to_vec()),
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
            vec![ConnectFrame::Data(b"deflated".to_vec())]
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
            vec![ConnectFrame::Data(b"payload".to_vec())]
        );
    }
}

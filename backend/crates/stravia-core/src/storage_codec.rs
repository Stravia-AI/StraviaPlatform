use std::cell::RefCell;

use anyhow::{Context, ensure};

const VERSION: u8 = 1;
const TRAILER_BYTES: usize = 14;
const RAW: u8 = 0;
const ZSTD: u8 = 1;
const MIN_COMPRESS_BYTES: usize = 128;

thread_local! {
    // 上下文在线程间不共享，避免热写入重复创建 zstd 工作区或争用全局锁。
    static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

/// 编码原始字节；不可压缩或小载荷保持原样。字典槽目前必须为零。
/// 固定尾部使 zstd 能直接生成最终缓冲，避免再复制整个压缩载荷。
pub(crate) fn encode(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (mut encoded, codec) = if bytes.len() >= MIN_COMPRESS_BYTES {
        let mut compressed = COMPRESSOR.with(|cell| -> anyhow::Result<Vec<u8>> {
            let mut state = cell.borrow_mut();
            if state.is_none() {
                let mut compressor = zstd::bulk::Compressor::new(3)?;
                compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
                *state = Some(compressor);
            }
            let capacity = zstd::zstd_safe::compress_bound(bytes.len())
                .checked_add(TRAILER_BYTES)
                .context("storage payload capacity overflow")?;
            let mut output = Vec::new();
            output
                .try_reserve_exact(capacity)
                .context("allocating encoded storage payload")?;
            state
                .as_mut()
                .context("missing storage compressor")?
                .compress_to_buffer(bytes, &mut output)
                .context("compressing storage payload")?;
            Ok(output)
        })?;
        if compressed.len() < bytes.len() {
            (compressed, ZSTD)
        } else {
            compressed.clear();
            compressed.extend_from_slice(bytes);
            (compressed, RAW)
        }
    } else {
        let mut output = Vec::new();
        output
            .try_reserve_exact(
                bytes
                    .len()
                    .checked_add(TRAILER_BYTES)
                    .context("storage payload capacity overflow")?,
            )
            .context("allocating encoded storage payload")?;
        output.extend_from_slice(bytes);
        (output, RAW)
    };
    encoded.push(codec);
    encoded.extend_from_slice(&0u32.to_le_bytes());
    encoded.extend_from_slice(&u64::try_from(bytes.len())?.to_le_bytes());
    encoded.push(VERSION);
    Ok(encoded)
}

/// 还原一条载荷；拒绝未知版本、字典、长度不一致与损坏帧。
pub(crate) fn decode(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(bytes.len() >= TRAILER_BYTES, "truncated storage payload");
    let (body, trailer) = bytes.split_at(bytes.len() - TRAILER_BYTES);
    ensure!(trailer[13] == VERSION, "unsupported storage codec version");
    ensure!(trailer[1..5] == [0; 4], "unsupported storage dictionary");
    let expected = usize::try_from(u64::from_le_bytes(trailer[5..13].try_into()?))?;
    match trailer[0] {
        RAW => {
            ensure!(body.len() == expected, "storage payload length mismatch");
            let mut decoded = Vec::new();
            decoded
                .try_reserve_exact(expected)
                .context("allocating decoded storage payload")?;
            decoded.extend_from_slice(body);
            Ok(decoded)
        }
        ZSTD => {
            // 写入端总是生成单帧并带原始长度；分配前验证帧长度，不信任尾部声明。
            let frame_size = zstd::zstd_safe::get_frame_content_size(body)
                .map_err(|_| anyhow::anyhow!("invalid storage compression frame"))?
                .context("storage compression frame has no content size")?;
            ensure!(
                frame_size == u64::try_from(expected)?,
                "storage frame length mismatch"
            );
            let frame_bytes = zstd::zstd_safe::find_frame_compressed_size(body)
                .map_err(|_| anyhow::anyhow!("invalid storage compression frame"))?;
            ensure!(
                frame_bytes == body.len(),
                "unexpected trailing storage frame data"
            );
            let decoded = DECOMPRESSOR.with(|cell| -> anyhow::Result<Vec<u8>> {
                let mut state = cell.borrow_mut();
                if state.is_none() {
                    *state = Some(zstd::bulk::Decompressor::new()?);
                }
                let mut decoded = Vec::new();
                decoded
                    .try_reserve_exact(expected)
                    .context("allocating decoded storage payload")?;
                state
                    .as_mut()
                    .context("missing storage decompressor")?
                    .decompress_to_buffer(body, &mut decoded)
                    .context("decompressing storage payload")?;
                Ok(decoded)
            })?;
            ensure!(decoded.len() == expected, "storage payload length mismatch");
            Ok(decoded)
        }
        _ => anyhow::bail!("unsupported storage codec"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_binary_and_large_unicode_contents() -> anyhow::Result<()> {
        for input in [
            Vec::new(),
            (0..=255).collect::<Vec<u8>>(),
            "工具结果\n😀\0".repeat(20_000).into_bytes(),
        ] {
            assert_eq!(decode(&encode(&input)?)?, input);
        }
        Ok(())
    }

    #[test]
    fn damaged_or_unknown_encoding_is_rejected() -> anyhow::Result<()> {
        let input = b"repeated canonical item contents".repeat(1000);
        let encoded = encode(&input)?;
        let mut damaged = encoded.clone();
        damaged[8] ^= 1;
        assert!(decode(&damaged).is_err());
        let mut wrong_length = encoded.clone();
        let length_offset = wrong_length.len() - 9;
        wrong_length[length_offset] ^= 1;
        assert!(decode(&wrong_length).is_err());
        let mut unknown = encoded;
        *unknown.last_mut().context("empty encoded contents")? = 255;
        assert!(decode(&unknown).is_err());
        assert!(decode(&[0; TRAILER_BYTES - 1]).is_err());
        Ok(())
    }
}

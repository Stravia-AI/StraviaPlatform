//! 每个分段自包含：重复元数据和 payload 引用本段已写入的内容，不使用压缩。
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

const CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Encoder {
    metadata: HashMap<[u8; 32], (Vec<u8>, u64)>,
    payloads: HashMap<[u8; 32], (Vec<u8>, u64)>,
    bytes: usize,
}

impl Encoder {
    pub(super) fn encode(&mut self, line: &[u8], offset: u64) -> io::Result<Vec<u8>> {
        let mut record: Value = serde_json::from_slice(line).map_err(io::Error::other)?;
        let object = record.as_object_mut().ok_or_else(invalid)?;
        let sequence = object.remove("sequence").ok_or_else(invalid)?;
        let recorded_at = object.remove("recorded_at").ok_or_else(invalid)?;
        let payload = object.remove("payload").ok_or_else(invalid)?;
        let metadata = serde_json::to_vec(&record).map_err(io::Error::other)?;
        let content = serde_json::to_vec(&payload).map_err(io::Error::other)?;
        if self
            .bytes
            .saturating_add(metadata.len())
            .saturating_add(content.len())
            > CACHE_BYTES
        {
            self.metadata.clear();
            self.payloads.clear();
            self.bytes = 0;
        }
        let mut stored =
            serde_json::json!({"trace_storage":1,"sequence":sequence,"recorded_at":recorded_at});
        if let Some(reference) = self.intern(true, metadata, offset) {
            stored["meta_ref"] = reference.into();
        } else {
            stored["meta"] = record;
        }
        if content.len() >= 256 {
            if let Some(reference) = self.intern(false, content, offset) {
                stored["payload_ref"] = reference.into();
            } else {
                stored["payload"] = payload;
            }
        } else {
            stored["payload"] = payload;
        }
        let mut bytes = serde_json::to_vec(&stored).map_err(io::Error::other)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn intern(&mut self, metadata: bool, bytes: Vec<u8>, offset: u64) -> Option<u64> {
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let cache = if metadata {
            &mut self.metadata
        } else {
            &mut self.payloads
        };
        if let Some((previous, reference)) = cache.get(&hash)
            && previous == &bytes
        {
            return Some(*reference);
        }
        // 哈希只筛选；碰撞不共享。超大单条内联，不扩大每个活跃 trace 的缓存。
        if self.bytes.saturating_add(bytes.len()) <= CACHE_BYTES && !cache.contains_key(&hash) {
            self.bytes += bytes.len();
            cache.insert(hash, (bytes, offset));
        }
        None
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid trace content reference",
    )
}

fn field(record: &Value, name: &str, file: &mut File, offset: u64) -> io::Result<Value> {
    let reference = record.get(format!("{name}_ref"));
    match (record.get(name), reference) {
        (Some(value), None) => Ok(value.clone()),
        (None, Some(reference)) => {
            let position = reference.as_u64().ok_or_else(invalid)?;
            if position >= offset {
                return Err(invalid());
            }
            file.seek(SeekFrom::Start(position))?;
            let mut line = Vec::new();
            io::BufReader::new(&mut *file).read_until(b'\n', &mut line)?;
            let target: Value = serde_json::from_slice(&line).map_err(io::Error::other)?;
            if target["trace_storage"].as_u64() != Some(1) {
                return Err(invalid());
            }
            // 引用只能指向内联定义，不允许递归链、循环或跨分段查找。
            target.get(name).cloned().ok_or_else(invalid)
        }
        _ => Err(invalid()),
    }
}

pub(super) fn decode(line: &[u8], file: &mut File, offset: u64) -> io::Result<Value> {
    let stored: Value = serde_json::from_slice(line).map_err(io::Error::other)?;
    if stored["trace_storage"].as_u64() != Some(1) {
        return Err(invalid());
    }
    let mut record = field(&stored, "meta", file, offset)?;
    let object = record.as_object_mut().ok_or_else(invalid)?;
    if object.contains_key("payload")
        || object.contains_key("sequence")
        || object.contains_key("recorded_at")
    {
        return Err(invalid());
    }
    object.insert(
        "sequence".into(),
        stored.get("sequence").cloned().ok_or_else(invalid)?,
    );
    object.insert(
        "recorded_at".into(),
        stored.get("recorded_at").cloned().ok_or_else(invalid)?,
    );
    object.insert("payload".into(), field(&stored, "payload", file, offset)?);
    Ok(record)
}

pub(super) fn visit(
    path: &Path,
    bytes: u64,
    mut visitor: impl FnMut(Value) -> io::Result<()>,
) -> io::Result<()> {
    let mut reader = io::BufReader::new(File::open(path)?.take(bytes));
    let mut references = File::open(path)?;
    let mut offset = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            return Err(invalid());
        }
        visitor(decode(&line, &mut references, offset)?)?;
        offset += read as u64;
    }
    Ok(())
}

/// 仅用于独占的离线副本；完整语义核验通过才替换单个分段。
pub(super) fn optimize_segment(path: &Path) -> io::Result<u64> {
    let original_bytes = path.metadata()?.len();
    let mut output = tempfile::NamedTempFile::new_in(path.parent().ok_or_else(invalid)?)?;
    let mut encoder = Encoder::default();
    let mut offset = 0;
    visit(path, original_bytes, |record| {
        let line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        let bytes = encoder.encode(&line, offset)?;
        output.write_all(&bytes)?;
        offset += bytes.len() as u64;
        Ok(())
    })?;
    output.as_file().sync_all()?;
    let mut encoded = io::BufReader::new(File::open(output.path())?);
    let mut references = File::open(output.path())?;
    let mut position = 0;
    visit(path, original_bytes, |original| {
        let mut line = Vec::new();
        let read = encoded.read_until(b'\n', &mut line)?;
        if decode(&line, &mut references, position)? != original {
            return Err(invalid());
        }
        position += read as u64;
        Ok(())
    })?;
    if position != offset {
        return Err(invalid());
    }
    drop(encoded);
    drop(references);
    if offset < original_bytes {
        output.persist(path).map_err(|error| error.error)?;
        Ok(offset)
    } else {
        Ok(original_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_segment_round_trips_and_legacy_records_are_rejected() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("segment-000001.jsonl");
        let payload = serde_json::json!({"text":"保留全部内容\n".repeat(2000)});
        let records = (0..5)
            .map(|sequence| {
                serde_json::json!({
                    "sequence":sequence,"recorded_at":sequence*11,"layer":"canonical",
                    "stage":"stage","payload":payload,"unknown":{"keep":true}
                })
            })
            .collect::<Vec<_>>();
        let mut encoder = Encoder::default();
        let mut file = File::create(&path)?;
        let mut offset = 0;
        for record in &records {
            let bytes = encoder.encode(&serde_json::to_vec(record)?, offset)?;
            offset += bytes.len() as u64;
            file.write_all(&bytes)?;
        }
        file.sync_all()?;
        drop(file);
        let mut restored = Vec::new();
        visit(&path, offset, |record| {
            restored.push(record);
            Ok(())
        })?;
        assert_eq!(restored, records);
        // 当前格式已是编码不动点；优化只校验还原并可能收缩，绝不放大。
        assert_eq!(optimize_segment(&path)?, offset);
        let corrupt = serde_json::json!({
            "trace_storage":1,"sequence":10,"recorded_at":10,"meta_ref":offset+1,"payload":null
        });
        assert!(
            decode(
                &serde_json::to_vec(&corrupt)?,
                &mut File::open(&path)?,
                offset
            )
            .is_err()
        );
        // 无 trace_storage 标记的旧版裸记录不再透传解码。
        assert!(
            decode(
                &serde_json::to_vec(&records[0])?,
                &mut File::open(&path)?,
                0
            )
            .is_err()
        );
        Ok(())
    }
}

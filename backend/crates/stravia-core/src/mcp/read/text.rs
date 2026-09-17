use std::{io::SeekFrom, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stravia_runtime_contract::{
    Principal,
    artifact::{
        ArtifactError, ArtifactReader, ArtifactRef, ArtifactSource, ArtifactStore,
        MAX_ARTIFACT_BYTES,
    },
    hook::PlatformToolError,
};
use stravia_web_access::fetch::ReadText;
use stravia_web_access_contract::read_path::{LineSelection, ReadOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub(super) const SNAPSHOT_MIME: &str = "application/vnd.stravia.read-snapshot";
pub(super) const PAGE_BYTES: usize = 32 * 1024;
pub(super) const PAGE_LINES: usize = 200;
const HEADER_LIMIT: usize = 16 * 1024;
const CURSOR_LIMIT: usize = 8192;
const RANGE_LIMIT: usize = 16;
const SCAN_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    representation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    source_truncated: bool,
    limitations: Vec<String>,
    text_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    v: u8,
    artifact_id: String,
    ranges: Vec<[usize; 2]>,
    range_index: usize,
    offset: usize,
}

struct Snapshot {
    reader: ArtifactReader,
    file: tokio::fs::File,
    header: Header,
    body_offset: u64,
    lines: u64,
}

fn invalid(message: &str) -> PlatformToolError {
    PlatformToolError::new(message)
}
fn io_error(error: impl std::fmt::Display) -> PlatformToolError {
    PlatformToolError::new(error.to_string())
}

pub(super) fn exceeds_page(text: &str) -> bool {
    text.len() > PAGE_BYTES || text.split_inclusive('\n').take(PAGE_LINES + 1).count() > PAGE_LINES
}

pub(super) async fn create(
    store: &dyn ArtifactStore,
    principal: &Principal,
    retention: Duration,
    text: ReadText,
    source_url: Option<String>,
    options: &ReadOptions,
) -> Result<Value, PlatformToolError> {
    let mut header = Header {
        version: 1,
        representation: text.representation,
        source_url,
        source_truncated: text.source_truncated,
        limitations: text.limitations,
        text_bytes: text.text.len() as u64,
    };
    let mut encoded = serde_json::to_vec(&header).map_err(io_error)?;
    if encoded.len() > HEADER_LIMIT && header.source_url.take().is_some() {
        header.limitations.push(
            "The source URL was omitted because snapshot metadata exceeded its size limit.".into(),
        );
        encoded = serde_json::to_vec(&header).map_err(io_error)?;
    }
    if encoded.len() > HEADER_LIMIT {
        return Err(invalid("Text snapshot metadata exceeds its size limit"));
    }
    let size = 4 + encoded.len() as u64 + header.text_bytes;
    if size > MAX_ARTIFACT_BYTES {
        return Err(invalid("Text snapshot exceeds the Artifact size limit"));
    }
    let prefix = Bytes::copy_from_slice(&(encoded.len() as u32).to_be_bytes());
    let chunks = [prefix, Bytes::from(encoded), Bytes::from(text.text)];
    let artifact = store
        .ingest(
            principal,
            SNAPSHOT_MIME,
            Some(size),
            Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))),
            retention,
        )
        .await
        .map_err(io_error)?;
    let reader = store
        .open(principal, &artifact.id)
        .await
        .map_err(io_error)?;
    read(reader, options).await
}

impl Snapshot {
    async fn open(reader: ArtifactReader) -> Result<Self, PlatformToolError> {
        if reader.artifact.mime_type != SNAPSHOT_MIME || reader.artifact.size > MAX_ARTIFACT_BYTES {
            return Err(invalid("Invalid text snapshot type or size"));
        }
        let ArtifactSource::LocalPath(path) = &reader.source else {
            return Err(invalid("Text snapshot is not locally readable"));
        };
        let mut file = tokio::fs::File::open(path).await.map_err(io_error)?;
        if file.metadata().await.map_err(io_error)?.len() != reader.artifact.size {
            return Err(invalid("Text snapshot size does not match its Artifact"));
        }
        let mut prefix = [0; 4];
        file.read_exact(&mut prefix).await.map_err(io_error)?;
        let header_size = u32::from_be_bytes(prefix) as usize;
        if header_size == 0 || header_size > HEADER_LIMIT {
            return Err(invalid("Invalid text snapshot header length"));
        }
        let mut encoded = vec![0; header_size];
        file.read_exact(&mut encoded).await.map_err(io_error)?;
        let header: Header = serde_json::from_slice(&encoded)
            .map_err(|_| invalid("Invalid text snapshot header"))?;
        let body_offset = 4 + header_size as u64;
        if header.version != 1
            || !matches!(header.representation.as_str(), "markdown" | "raw" | "text")
            || body_offset.checked_add(header.text_bytes) != Some(reader.artifact.size)
        {
            return Err(invalid(
                "Invalid text snapshot version, representation or body length",
            ));
        }
        // 私有 MIME 和可编辑游标都不证明来源；每次以有界内存校验全部正文。
        let mut remaining = header.text_bytes;
        let mut buffer = vec![0; SCAN_BYTES + 3];
        let mut carry = 0;
        let mut newlines = 0_u64;
        let mut last = None;
        while remaining != 0 {
            let count = (remaining as usize).min(SCAN_BYTES);
            file.read_exact(&mut buffer[carry..carry + count])
                .await
                .map_err(io_error)?;
            for &byte in &buffer[carry..carry + count] {
                newlines += u64::from(byte == b'\n');
                last = Some(byte);
            }
            remaining -= count as u64;
            let length = carry + count;
            carry = match std::str::from_utf8(&buffer[..length]) {
                Ok(_) => 0,
                Err(error) if error.error_len().is_none() => {
                    let valid = error.valid_up_to();
                    let remainder = length - valid;
                    buffer.copy_within(valid..length, 0);
                    remainder
                }
                Err(_) => return Err(invalid("Text snapshot body is not valid UTF-8")),
            };
        }
        if carry != 0 {
            return Err(invalid("Text snapshot body ends inside a UTF-8 character"));
        }
        let lines = newlines + u64::from(last.is_some_and(|byte| byte != b'\n'));
        Ok(Self {
            reader,
            file,
            header,
            body_offset,
            lines,
        })
    }

    async fn boundary(&mut self, offset: usize) -> Result<(), PlatformToolError> {
        if offset as u64 > self.header.text_bytes {
            return Err(invalid("Text cursor is outside the snapshot"));
        }
        if offset == 0 || offset as u64 == self.header.text_bytes {
            return Ok(());
        }
        self.file
            .seek(SeekFrom::Start(self.body_offset + offset as u64))
            .await
            .map_err(io_error)?;
        let byte = self.file.read_u8().await.map_err(io_error)?;
        if byte & 0xc0 == 0x80 {
            return Err(invalid("Text cursor must be on a UTF-8 boundary"));
        }
        Ok(())
    }

    async fn selections(
        &mut self,
        selections: &[LineSelection],
    ) -> Result<Vec<[usize; 2]>, PlatformToolError> {
        if selections.is_empty() || selections.len() > RANGE_LIMIT || self.lines == 0 {
            return Err(invalid("Line selection is outside the text snapshot"));
        }
        let mut lines = Vec::with_capacity(selections.len());
        for selection in selections {
            let (start, end) = match *selection {
                LineSelection::From(start) => (start, self.lines),
                LineSelection::Inclusive { start, end } => (start, end),
                LineSelection::Count { start, count } => {
                    let end = count
                        .checked_sub(1)
                        .and_then(|count| start.checked_add(count))
                        .ok_or_else(|| invalid("Line selection overflows"))?;
                    (start, end)
                }
                LineSelection::Last(count) if count != 0 => (
                    self.lines.saturating_sub(count).saturating_add(1),
                    self.lines,
                ),
                LineSelection::Last(_) => return Err(invalid("Line count must be positive")),
            };
            if start == 0 || start > self.lines || start > end {
                return Err(invalid("Line selection is outside the text snapshot"));
            }
            lines.push([start, end.min(self.lines)]);
        }
        lines.sort_unstable();
        let mut merged: Vec<[u64; 2]> = Vec::with_capacity(lines.len());
        for [start, end] in lines {
            if let Some(previous) = merged.last_mut()
                && start <= previous[1].saturating_add(1)
            {
                previous[1] = previous[1].max(end);
                continue;
            }
            merged.push([start, end]);
        }
        // 最多 32 个边界；不为每一行分配索引。
        let mut points: Vec<(u64, usize)> = merged
            .iter()
            .flat_map(|[start, end]| [(*start - 1, 0), (*end, self.header.text_bytes as usize)])
            .collect();
        points.sort_unstable_by_key(|point| point.0);
        self.file
            .seek(SeekFrom::Start(self.body_offset))
            .await
            .map_err(io_error)?;
        let mut buffer = vec![0; SCAN_BYTES];
        let mut offset = 0_usize;
        let mut completed = 0_u64;
        let mut point = 0;
        while point < points.len() && points[point].0 == 0 {
            points[point].1 = 0;
            point += 1;
        }
        while offset < self.header.text_bytes as usize && point < points.len() {
            let count = (self.header.text_bytes as usize - offset).min(buffer.len());
            self.file
                .read_exact(&mut buffer[..count])
                .await
                .map_err(io_error)?;
            for (index, byte) in buffer[..count].iter().enumerate() {
                if *byte == b'\n' {
                    completed += 1;
                    while point < points.len() && points[point].0 == completed {
                        points[point].1 = offset + index + 1;
                        point += 1;
                    }
                }
            }
            offset += count;
        }
        Ok(merged
            .into_iter()
            .map(|[start, end]| {
                let start = points
                    .iter()
                    .find(|point| point.0 == start - 1)
                    .expect("selected start")
                    .1;
                let end = points
                    .iter()
                    .find(|point| point.0 == end)
                    .expect("selected end")
                    .1;
                [start, end]
            })
            .collect())
    }
}

async fn cursor_for(
    snapshot: &mut Snapshot,
    options: &ReadOptions,
) -> Result<Cursor, PlatformToolError> {
    if let Some(encoded) = &options.cursor {
        if encoded.len() > CURSOR_LIMIT {
            return Err(invalid("Text cursor exceeds its size limit"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| invalid("Invalid text cursor encoding"))?;
        let cursor: Cursor =
            serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid text cursor"))?;
        if cursor.v != 1
            || cursor.artifact_id != snapshot.reader.artifact.id.as_str()
            || cursor.ranges.is_empty()
            || cursor.ranges.len() > RANGE_LIMIT
            || cursor.range_index >= cursor.ranges.len()
        {
            return Err(invalid("Text cursor does not identify this snapshot"));
        }
        let mut previous_end = 0;
        for &[start, end] in &cursor.ranges {
            if start < previous_end || start >= end || end as u64 > snapshot.header.text_bytes {
                return Err(invalid("Invalid text cursor range"));
            }
            snapshot.boundary(start).await?;
            snapshot.boundary(end).await?;
            previous_end = end;
        }
        let [start, end] = cursor.ranges[cursor.range_index];
        if cursor.offset < start || cursor.offset >= end {
            return Err(invalid("Invalid text cursor offset"));
        }
        snapshot.boundary(cursor.offset).await?;
        return Ok(cursor);
    }
    let ranges = if let Some(lines) = &options.lines {
        snapshot.selections(lines).await?
    } else if snapshot.header.text_bytes == 0 {
        Vec::new()
    } else {
        vec![[0, snapshot.header.text_bytes as usize]]
    };
    Ok(Cursor {
        v: 1,
        artifact_id: snapshot.reader.artifact.id.as_str().to_owned(),
        offset: ranges.first().map_or(0, |range| range[0]),
        ranges,
        range_index: 0,
    })
}

pub(super) async fn read(
    reader: ArtifactReader,
    options: &ReadOptions,
) -> Result<Value, PlatformToolError> {
    let mut snapshot = Snapshot::open(reader).await?;
    let mut cursor = cursor_for(&mut snapshot, options).await?;
    let mut content = String::with_capacity(PAGE_BYTES.min(snapshot.header.text_bytes as usize));
    let mut returned_ranges = Vec::with_capacity(cursor.ranges.len());
    let mut used_lines = 0;
    let mut last_line = None;
    let mut scanned_offset = 0;
    let mut scanned_newlines = 0_u64;
    let mut scan_buffer = vec![0; SCAN_BYTES];
    while cursor.range_index < cursor.ranges.len() && content.len() < PAGE_BYTES {
        let start = cursor.offset;
        let end = cursor.ranges[cursor.range_index][1];
        snapshot
            .file
            .seek(SeekFrom::Start(
                snapshot.body_offset + scanned_offset as u64,
            ))
            .await
            .map_err(io_error)?;
        while scanned_offset < start {
            let count = (start - scanned_offset).min(scan_buffer.len());
            snapshot
                .file
                .read_exact(&mut scan_buffer[..count])
                .await
                .map_err(io_error)?;
            scanned_newlines += scan_buffer[..count]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count() as u64;
            scanned_offset += count;
        }
        let start_line = scanned_newlines + 1;
        let count = (end - start).min(PAGE_BYTES - content.len());
        let mut bytes = vec![0; count];
        snapshot
            .file
            .read_exact(&mut bytes)
            .await
            .map_err(io_error)?;
        let valid = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(invalid("Text snapshot changed while reading")),
        };
        let mut length = 0;
        let mut line = start_line;
        for &byte in &bytes[..valid] {
            if last_line != Some(line) {
                if used_lines == PAGE_LINES {
                    break;
                }
                used_lines += 1;
                last_line = Some(line);
            }
            length += 1;
            if byte == b'\n' {
                line += 1;
            }
        }
        if length == 0 {
            break;
        }
        content.push_str(
            std::str::from_utf8(&bytes[..length])
                .map_err(|_| invalid("Invalid page UTF-8 boundary"))?,
        );
        let finish = start + length;
        let end_line = if bytes[length - 1] == b'\n' {
            line - 1
        } else {
            line
        };
        returned_ranges.push(json!({"start_line":start_line,"end_line":end_line,"start_byte":start,"end_byte":finish}));
        scanned_newlines = line - 1;
        scanned_offset = finish;
        cursor.offset = finish;
        if finish == end {
            cursor.range_index += 1;
            if let Some(range) = cursor.ranges.get(cursor.range_index) {
                cursor.offset = range[0];
            }
        }
        if length < valid || valid < count {
            break;
        }
    }
    let has_more = cursor.range_index < cursor.ranges.len();
    let read_path = snapshot.reader.artifact.reference();
    let mut value = json!({
        "content":content,
        "read_path":read_path,
        "representation":snapshot.header.representation,
        "returned_ranges":returned_ranges,
        "has_more":has_more,
        "source_truncated":snapshot.header.source_truncated,
        "limitations":snapshot.header.limitations,
    });
    if let Some(source) = snapshot.header.source_url {
        value["source_url"] = json!(source);
    }
    if options.question.is_some() {
        value["question_applied"] = json!(false);
    }
    if has_more {
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).map_err(io_error)?);
        if encoded.len() > CURSOR_LIMIT {
            return Err(invalid("Generated text cursor exceeds its size limit"));
        }
        value["next_path"] = json!(format!("{read_path}?cursor={encoded}"));
    }
    Ok(value)
}

pub(super) async fn export(
    store: &dyn ArtifactStore,
    principal: &Principal,
    retention: Duration,
    reader: ArtifactReader,
) -> Result<ArtifactRef, PlatformToolError> {
    let mut snapshot = Snapshot::open(reader).await?;
    let size = snapshot.header.text_bytes;
    snapshot
        .file
        .seek(SeekFrom::Start(snapshot.body_offset))
        .await
        .map_err(io_error)?;
    // guard 随流一起存活；导出不分配或复制完整正文。
    let stream =
        futures::stream::try_unfold((snapshot, size), |(mut snapshot, remaining)| async move {
            if remaining == 0 {
                return Ok(None);
            }
            let count = remaining.min(SCAN_BYTES as u64) as usize;
            let mut bytes = vec![0; count];
            snapshot
                .file
                .read_exact(&mut bytes)
                .await
                .map_err(|error| ArtifactError::Storage(error.to_string()))?;
            Ok(Some((
                Bytes::from(bytes),
                (snapshot, remaining - count as u64),
            )))
        });
    store
        .ingest(
            principal,
            "text/plain; charset=utf-8",
            Some(size),
            Box::pin(stream),
            retention,
        )
        .await
        .map_err(io_error)
}

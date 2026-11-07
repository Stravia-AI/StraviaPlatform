//! 共享正文存储：Turn Chain、History Marker 与 Interaction Observation 平级使用
//! 同一张按 Principal 隔离、内容寻址的正文表。
//!
//! 内容只按引用判活，本模块不理解任何一方的语义，也不保存到期时间。内容身份是
//! `SHA-256(域前缀 || format || raw_bytes)`：文本单元保存原始 UTF-8，JSON 单元保存
//! 紧凑 serde JSON；`format` 计入摘要，字节相同的文本与 JSON 不会合并。`body` 经
//! `storage_codec` 编码，压缩只发生在这一层。
//!
//! 引用方删除引用时，必须在同一事务内调用 [`enqueue_unreferenced`]，把失去最后引用的
//! 内容登记进 `content_gc`；最终删除只发生在 [`sweep_sqlite`] / [`sweep_postgres`]，
//! 它在锁协议内重检历史、Observation 与 journal 的反向引用；旧迁移阶段尚无 journal
//! 表时跳过该项。写入方没有固定先后角色：先到的一方插入正文，其余
//! 方按摘要命中已有行、只写引用。摘要命中只是提示，事务内重检。

use std::collections::HashMap;

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, SqlitePool};

use stravia_runtime_contract::protocol::ir::content::{ContentFormat, ContentUnitBody};

mod descriptor;
#[cfg(test)]
mod shared_tests;
pub(crate) mod sql;
#[cfg(test)]
mod tests;

pub(crate) use descriptor::{
    Descriptor, Slot, decode_descriptor, encode_descriptor, restore_slots, validate_slots,
};

// 对外稳定名称：content_store::sqlite::… 与 content_store::postgres::…。
pub(crate) mod sqlite {
    pub(crate) use super::sql::read_ahead_unreferenced;
    pub(crate) use super::sql::sqlite::*;
}
pub(crate) mod postgres {
    pub(crate) use super::sql::postgres::*;
}

/// SQLite 内容回收入口；调用方已持有写租约语义。
pub(crate) async fn sweep_sqlite(pool: &SqlitePool) -> anyhow::Result<u64> {
    sql::sqlite::sweep(pool).await
}

/// PostgreSQL 内容回收入口。
pub(crate) async fn sweep_postgres(pool: &PgPool) -> anyhow::Result<u64> {
    sql::postgres::sweep(pool).await
}

/// 内容身份域前缀；与旧 `turn-content.v1` 不共享身份。
const CONTENT_DIGEST_DOMAIN: &[u8] = b"stravia.content.v1\0";

pub(crate) type ContentDigest = [u8; 32];

/// 内容身份：`SHA-256(域前缀 || format_byte || raw_bytes)`。
pub(crate) fn content_digest(format: ContentFormat, raw: &[u8]) -> ContentDigest {
    Sha256::new()
        .chain_update(CONTENT_DIGEST_DOMAIN)
        .chain_update([format_code(format)])
        .chain_update(raw)
        .finalize()
        .into()
}

/// 正文格式的持久化编码：同时是摘要前缀字节与 `contents.format` 列的取值，
/// 数值一经写入不得改动或复用。
pub(crate) fn format_code(format: ContentFormat) -> u8 {
    match format {
        ContentFormat::Text => 0,
        ContentFormat::Json => 1,
    }
}

pub(crate) fn format_from_code(code: i64) -> Option<ContentFormat> {
    match code {
        0 => Some(ContentFormat::Text),
        1 => Some(ContentFormat::Json),
        _ => None,
    }
}

/// 一份待写入的正文。`bytes` 在 `prepare_missing` 之前是原始字节，之后是 codec 字节；
/// `encoded` 记录当前形态，陈旧的存在性提示可以在不重新序列化的前提下重建正文。
pub(crate) struct Content {
    digest: ContentDigest,
    format: ContentFormat,
    raw_len: i64,
    bytes: Vec<u8>,
    encoded: bool,
}

impl Content {
    /// 文本正文构造器，供 Observation 等引用方使用；Turn Chain 与 History Marker 已持有摘要，
    /// 走 `with_digest`。
    pub(crate) fn text(text: impl Into<String>) -> Self {
        let raw = text.into().into_bytes();
        Self::with_digest(
            content_digest(ContentFormat::Text, &raw),
            ContentFormat::Text,
            raw,
        )
    }

    /// JSON 正文构造器；`raw` 必须是紧凑 JSON 字节。
    pub(crate) fn json(raw: Vec<u8>) -> Self {
        Self::with_digest(
            content_digest(ContentFormat::Json, &raw),
            ContentFormat::Json,
            raw,
        )
    }

    /// 由共享正文单元定义产出的单元构造：文本保存原始 UTF-8，JSON 保存紧凑 serde JSON。
    /// Turn Chain、History Marker 与 Observation 对同一段正文必须得到同一份字节，
    /// 所以三方都经由这里构造，不各自重复编码。
    pub(crate) fn from_unit_body(body: ContentUnitBody<'_>) -> Self {
        match body {
            ContentUnitBody::Text(text) => Self::text(text),
            ContentUnitBody::Json(value) => Self::json(value.to_string().into_bytes()),
        }
    }

    pub(crate) fn digest(&self) -> ContentDigest {
        self.digest
    }

    pub(crate) fn format(&self) -> ContentFormat {
        self.format
    }

    /// 由调用方已计算的摘要构造，避免对同一批字节再次哈希。
    pub(crate) fn with_digest(digest: ContentDigest, format: ContentFormat, raw: Vec<u8>) -> Self {
        let raw_len = i64::try_from(raw.len()).unwrap_or(i64::MAX);
        Self {
            digest,
            format,
            raw_len,
            bytes: raw,
            encoded: false,
        }
    }

    /// 释放正文；只有在调用方的事务已锁定对应行（或已写入）后才可调用。
    fn release(&mut self) {
        self.bytes = Vec::new();
        self.encoded = true;
    }
}

/// 已读取的正文；文本以 UTF-8 直接返回，不经过 `serde_json`。
pub(crate) struct Body {
    pub(crate) id: i64,
    pub(crate) format: ContentFormat,
    /// 读取时已校验过的内容身份；管理端内容端点的 ETag 以它为输入。
    pub(crate) digest: ContentDigest,
    pub(crate) bytes: Vec<u8>,
}

/// 尚未解码的数据库行。迁移读取把所有权交给有界 blocking worker；
/// 普通读取仍在原路径调用同一校验，不改变缓存与读取契约。
pub(crate) type EncodedBody = (i64, Vec<u8>, i64, i64, Vec<u8>);

pub(crate) fn decode_body(
    (id, digest, format, raw_len, encoded): EncodedBody,
) -> anyhow::Result<Body> {
    let format = format_from_code(format).context("invalid stored content format")?;
    let raw = crate::storage_codec::decode(&encoded)?;
    ensure!(
        i64::try_from(raw.len()).unwrap_or(i64::MAX) == raw_len,
        "stored content length mismatch"
    );
    let computed = content_digest(format, &raw);
    ensure!(computed == digest.as_slice(), "damaged content");
    Ok(Body {
        id,
        format,
        digest: computed,
        bytes: raw.into_owned(),
    })
}

impl Body {
    pub(crate) fn text(&self) -> anyhow::Result<&str> {
        ensure!(
            self.format == ContentFormat::Text,
            "content row is not a text unit"
        );
        std::str::from_utf8(&self.bytes).context("invalid stored text content")
    }

    pub(crate) fn json(&self) -> anyhow::Result<serde_json::Value> {
        ensure!(
            self.format == ContentFormat::Json,
            "content row is not a JSON unit"
        );
        serde_json::from_slice(&self.bytes).context("invalid stored JSON content")
    }

    /// 调用方看到的 JSON 值：文本包装为字符串（保持原始 UTF-8），JSON 原样解析。
    pub(crate) fn value(&self) -> anyhow::Result<serde_json::Value> {
        match self.format {
            ContentFormat::Text => Ok(serde_json::Value::String(self.text()?.to_owned())),
            ContentFormat::Json => self.json(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum LookupScope<'a> {
    Id(i64),
    Principal(&'a str),
}

/// 一次事务内可用的正文读取集合。
pub(crate) struct Bodies {
    by_id: HashMap<i64, Body>,
}

impl Bodies {
    pub(crate) fn from_rows(rows: Vec<Body>) -> Self {
        Self {
            by_id: rows.into_iter().map(|body| (body.id, body)).collect(),
        }
    }

    pub(crate) fn get(&self, id: i64) -> Option<&Body> {
        self.by_id.get(&id)
    }

    /// 按给定顺序取出全部正文；任何一个缺失都是引用悬空，返回错误而不是少拼一段。
    pub(crate) fn require(&self, ids: impl IntoIterator<Item = i64>) -> anyhow::Result<Vec<&Body>> {
        ids.into_iter()
            .map(|id| {
                self.get(id)
                    .with_context(|| format!("referenced content {id} is missing"))
            })
            .collect()
    }
}

/// 压缩尚未确认存在的内容正文；存在性提示在写事务之前给出，因此这里只处理缺失项。
/// 巨大正文的压缩在阻塞线程池完成，不占用异步调度线程。
pub(crate) async fn prepare_missing(
    contents: &mut [Content],
    ids: &[Option<i64>],
) -> anyhow::Result<()> {
    ensure!(contents.len() == ids.len(), "content id count mismatch");
    let pending: Vec<_> = contents
        .iter_mut()
        .zip(ids)
        .enumerate()
        .filter(|(_, (content, id))| id.is_none() && !content.encoded)
        .map(|(index, (content, _))| (index, std::mem::take(&mut content.bytes)))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let prepared = tokio::task::spawn_blocking(move || {
        pending
            .into_iter()
            .map(|(index, raw)| Ok((index, crate::storage_codec::encode(raw.into())?)))
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await
    .context("content preparation worker failed")??;
    for (index, bytes) in prepared {
        contents[index].bytes = bytes;
        contents[index].encoded = true;
    }
    Ok(())
}

/// 内容位置按摘要分组，不按摘要分配：首次出现覆盖常见的唯一情形，只有真正的重复
/// 才使用一个小溢出向量。命中只是提示，写入方仍须在事务内重检。
struct DigestPositions {
    first: HashMap<ContentDigest, usize>,
    duplicates: HashMap<ContentDigest, Vec<usize>>,
}

impl DigestPositions {
    fn new(contents: &[Content]) -> Self {
        let mut first = HashMap::with_capacity(contents.len());
        let mut duplicates: HashMap<ContentDigest, Vec<usize>> = HashMap::new();
        for (index, content) in contents.iter().enumerate() {
            match first.entry(content.digest) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(index);
                }
                std::collections::hash_map::Entry::Occupied(entry) => {
                    duplicates.entry(*entry.key()).or_default().push(index);
                }
            }
        }
        Self { first, duplicates }
    }

    fn digests(&self) -> impl Iterator<Item = ContentDigest> + '_ {
        self.first.keys().copied()
    }

    fn observe(&self, ids: &mut [Option<i64>], digest: &[u8], id: i64) -> anyhow::Result<()> {
        let digest: ContentDigest = digest.try_into().context("invalid stored content digest")?;
        if let Some(index) = self.first.get(&digest) {
            ids[*index] = Some(id);
            if let Some(indices) = self.duplicates.get(&digest) {
                for index in indices {
                    ids[*index] = Some(id);
                }
            }
        }
        Ok(())
    }
}

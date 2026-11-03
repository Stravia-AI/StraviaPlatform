//! Observation 侧对共享内容存储的引用读写。
//!
//! 内容表与四张引用表由内容存储迁移（0037/0038）创建；本模块只做三件事：
//! 在已打开的事务里按 Principal 取 scope、把正文单元登记/复用为 content id、
//! 在删除引用后把失去最后引用的正文入队回收。等 Phase 1 的公开 API 落地后，
//! 这里的 SQL 会替换为对 `content_store` 模块的调用；调用点与事务边界不变。

use std::collections::HashMap;

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row, SqliteConnection};

use crate::storage_codec;
use stravia_runtime_contract::protocol::ir::content::{ContentFormat, ContentUnitBody};

const DIGEST_DOMAIN: &[u8] = b"stravia.content.v1\0";

/// 一个待登记的正文单元；`raw` 是未压缩的原始字节。
#[derive(Debug, Clone)]
pub(super) struct StoreUnit {
    pub format: ContentFormat,
    pub raw: Vec<u8>,
}

impl StoreUnit {
    pub(super) fn text(text: impl Into<String>) -> Self {
        Self {
            format: ContentFormat::Text,
            raw: text.into().into_bytes(),
        }
    }

    pub(super) fn json(value: &Value) -> Self {
        Self {
            format: ContentFormat::Json,
            raw: value.to_string().into_bytes(),
        }
    }

    /// 工具结果载荷：字符串按文本单元保存，其他 JSON 压成紧凑 JSON 单元。
    pub(super) fn from_value(value: &Value) -> Self {
        match value {
            Value::String(text) => Self::text(text.clone()),
            other => Self::json(other),
        }
    }

    /// 从共享定义产出的正文单元复制一份可写入的所有权副本。
    pub(super) fn from_unit(
        unit: &stravia_runtime_contract::protocol::ir::content::ContentUnit<'_>,
    ) -> Self {
        match unit.body {
            ContentUnitBody::Text(text) => Self::text(text),
            ContentUnitBody::Json(value) => Self::json(value),
        }
    }

    pub(super) fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(DIGEST_DOMAIN);
        hasher.update([self.format.as_byte()]);
        hasher.update(&self.raw);
        hasher.finalize().into()
    }
}

/// 读回的内容正文；文本按 UTF-8 解释，JSON 保持 byte。
#[derive(Debug, Clone)]
pub(super) struct StoredContent {
    pub format: ContentFormat,
    pub raw: Vec<u8>,
}

impl StoredContent {
    pub(super) fn text(&self) -> anyhow::Result<String> {
        Ok(String::from_utf8(self.raw.clone())?)
    }

    pub(super) fn json(&self) -> anyhow::Result<Value> {
        Ok(serde_json::from_slice(&self.raw)?)
    }

    /// 还原成调用方看到的 JSON 值：文本包装为字符串，JSON 原样解析。
    pub(super) fn as_value(&self) -> anyhow::Result<Value> {
        Ok(match self.format {
            ContentFormat::Text => Value::String(self.text()?),
            ContentFormat::Json => self.json()?,
        })
    }
}

fn raw_len(unit: &StoreUnit) -> anyhow::Result<i64> {
    i64::try_from(unit.raw.len()).map_err(|_| anyhow::anyhow!("content unit length overflows i64"))
}

pub(super) async fn resolve_scope_sqlite(
    connection: &mut SqliteConnection<'_>,
    principal: &str,
) -> anyhow::Result<i64> {
    sqlx::query("INSERT INTO content_scopes(principal) VALUES (?) ON CONFLICT(principal) DO NOTHING")
        .bind(principal)
        .execute(&mut *connection)
        .await?;
    Ok(sqlx::query_scalar("SELECT id FROM content_scopes WHERE principal=?")
        .bind(principal)
        .fetch_one(&mut *connection)
        .await?)
}

pub(super) async fn resolve_scope_postgres(
    connection: &mut PgConnection<'_>,
    principal: &str,
) -> anyhow::Result<i64> {
    sqlx::query("INSERT INTO content_scopes(principal) VALUES ($1) ON CONFLICT(principal) DO NOTHING")
        .bind(principal)
        .execute(&mut *connection)
        .await?;
    Ok(sqlx::query_scalar("SELECT id FROM content_scopes WHERE principal=$1")
        .bind(principal)
        .fetch_one(&mut *connection)
        .await?)
}

/// 在调用方事务内登记正文，返回与 `units` 同序的 content id。
/// 批内相同摘要只登记一次；SQLite 已有写租约，PostgreSQL 按行锁协议重检。
pub(super) async fn intern_sqlite(
    connection: &mut SqliteConnection<'_>,
    principal: &str,
    units: &[StoreUnit],
) -> anyhow::Result<Vec<i64>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let scope = resolve_scope_sqlite(connection, principal).await?;
    let mut ids: HashMap<[u8; 32], i64> = HashMap::new();
    let mut result = Vec::with_capacity(units.len());
    for unit in units {
        let digest = unit.digest();
        if let Some(id) = ids.get(&digest) {
            result.push(*id);
            continue;
        }
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM contents WHERE scope_id=? AND digest=?")
                .bind(scope)
                .bind(digest.as_slice())
                .fetch_optional(&mut *connection)
                .await?;
        let id = match existing {
            Some(id) => id,
            None => {
                let inserted: Option<i64> = sqlx::query_scalar(
                    "INSERT INTO contents(scope_id,digest,format,raw_len,body) VALUES (?,?,?,?,?) \
                     ON CONFLICT(scope_id,digest) DO NOTHING RETURNING id",
                )
                .bind(scope)
                .bind(digest.as_slice())
                .bind(unit.format.as_database_value())
                .bind(raw_len(unit)?)
                .bind(storage_codec::encode(unit.raw.clone().into())?)
                .fetch_optional(&mut *connection)
                .await?;
                match inserted {
                    Some(id) => id,
                    None => sqlx::query_scalar(
                        "SELECT id FROM contents WHERE scope_id=? AND digest=?",
                    )
                    .bind(scope)
                    .bind(digest.as_slice())
                    .fetch_one(&mut *connection)
                    .await?,
                }
            }
        };
        ids.insert(digest, id);
        result.push(id);
    }
    Ok(result)
}

pub(super) async fn intern_postgres(
    connection: &mut PgConnection<'_>,
    principal: &str,
    units: &[StoreUnit],
) -> anyhow::Result<Vec<i64>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let scope = resolve_scope_postgres(connection, principal).await?;
    let mut ids: HashMap<[u8; 32], i64> = HashMap::new();
    let mut result = Vec::with_capacity(units.len());
    for unit in units {
        let digest = unit.digest();
        if let Some(id) = ids.get(&digest) {
            result.push(*id);
            continue;
        }
        let id = reuse_or_insert_postgres(connection, scope, unit).await?;
        ids.insert(digest, id);
        result.push(id);
    }
    Ok(result)
}

async fn reuse_or_insert_postgres(
    connection: &mut PgConnection<'_>,
    scope: i64,
    unit: &StoreUnit,
) -> anyhow::Result<i64> {
    let digest = unit.digest();
    let locked: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM contents WHERE scope_id=$1 AND digest=$2 FOR KEY SHARE",
    )
    .bind(scope)
    .bind(digest.as_slice())
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(id) = locked {
        return Ok(id);
    }
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO contents(scope_id,digest,format,raw_len,body) VALUES ($1,$2,$3,$4,$5) \
         ON CONFLICT (scope_id,digest) DO NOTHING RETURNING id",
    )
    .bind(scope)
    .bind(digest.as_slice())
    .bind(unit.format.as_database_value())
    .bind(raw_len(unit)?)
    .bind(storage_codec::encode(unit.raw.clone().into())?)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(id) = inserted {
        return Ok(id);
    }
    Ok(sqlx::query_scalar("SELECT id FROM contents WHERE scope_id=$1 AND digest=$2 FOR KEY SHARE")
        .bind(scope)
        .bind(digest.as_slice())
        .fetch_one(&mut *connection)
        .await?)
}

/// 删除观察引用后，把已无任何引用的正文登记进回收队列（同一事务）。
pub(super) async fn enqueue_unreferenced_sqlite(
    connection: &mut SqliteConnection<'_>,
    content_ids: &[i64],
) -> anyhow::Result<()> {
    for chunk in content_ids.chunks(100) {
        let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "INSERT INTO content_gc(content_id) SELECT c.id FROM contents c WHERE c.id IN (",
        );
        let mut separated = builder.separated(",");
        for id in chunk {
            separated.push_bind(*id);
        }
        separated.push_unseparated(
            ") AND NOT EXISTS(SELECT 1 FROM turn_node_contents t WHERE t.content_id=c.id) \
                  AND NOT EXISTS(SELECT 1 FROM history_marker_contents m WHERE m.content_id=c.id) \
                  AND NOT EXISTS(SELECT 1 FROM observation_event_contents e WHERE e.content_id=c.id) \
                  AND NOT EXISTS(SELECT 1 FROM observation_tool_results r WHERE r.content_id=c.id) \
                  ON CONFLICT(content_id) DO NOTHING",
        );
        builder.build().execute(&mut *connection).await?;
    }
    Ok(())
}

pub(super) async fn enqueue_unreferenced_postgres(
    connection: &mut PgConnection<'_>,
    content_ids: &[i64],
) -> anyhow::Result<()> {
    for chunk in content_ids.chunks(100) {
        let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "INSERT INTO content_gc(content_id) SELECT c.id FROM contents c WHERE c.id IN (",
        );
        let mut separated = builder.separated(",");
        for id in chunk {
            separated.push_bind(*id);
        }
        separated.push_unseparated(
            ") AND NOT EXISTS(SELECT 1 FROM turn_node_contents t WHERE t.content_id=c.id) \
                  AND NOT EXISTS(SELECT 1 FROM history_marker_contents m WHERE m.content_id=c.id) \
                  AND NOT EXISTS(SELECT 1 FROM observation_event_contents e WHERE e.content_id=c.id) \
                  AND NOT EXISTS(SELECT 1 FROM observation_tool_results r WHERE r.content_id=c.id) \
                  ON CONFLICT (content_id) DO NOTHING",
        );
        builder.build().execute(&mut *connection).await?;
    }
    Ok(())
}

/// 读取一批正文；调用方负责只请求可见引用。
pub(super) async fn read_contents(
    store: &super::store::ObservationStore,
    content_ids: &[i64],
) -> anyhow::Result<HashMap<i64, StoredContent>> {
    use super::store::ObservationStore;
    let mut by_id = HashMap::new();
    if content_ids.is_empty() {
        return Ok(by_id);
    }
    match store {
        ObservationStore::Sqlite(pool, _) => {
            for chunk in content_ids.chunks(500) {
                let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                    "SELECT id,format,body FROM contents WHERE id IN (",
                );
                let mut separated = builder.separated(",");
                for id in chunk {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
                let rows = builder.build().fetch_all(pool).await?;
                for row in rows {
                    let id: i64 = row.try_get("id")?;
                    let format: i64 = row.try_get("format")?;
                    let body: Vec<u8> = row.try_get("body")?;
                    by_id.insert(
                        id,
                        StoredContent {
                            format: ContentFormat::from_database_value(format)
                                .ok_or_else(|| anyhow::anyhow!("unknown content format {format}"))?,
                            raw: storage_codec::decode(&body)?.to_vec(),
                        },
                    );
                }
            }
        }
        ObservationStore::Postgres(pool, _) => {
            for chunk in content_ids.chunks(500) {
                let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                    "SELECT id,format,body FROM contents WHERE id IN (",
                );
                let mut separated = builder.separated(',');
                for id in chunk {
                    separated.push_bind(*id);
                }
                separated.push_unseparated(")");
                let rows = builder.build().fetch_all(pool).await?;
                for row in rows {
                    let id: i64 = row.try_get("id")?;
                    let format: i32 = row.try_get("format")?;
                    let body: Vec<u8> = row.try_get("body")?;
                    by_id.insert(
                        id,
                        StoredContent {
                            format: ContentFormat::from_database_value(i64::from(format))
                                .ok_or_else(|| anyhow::anyhow!("unknown content format {format}"))?,
                            raw: storage_codec::decode(&body)?.to_vec(),
                        },
                    );
                }
            }
        }
    }
    Ok(by_id)
}

//! Observation 侧对共享内容存储的引用读写。
//!
//! 正文表与四张引用表由共享内容存储（`crate::content_store`）拥有；本模块只负责
//! Observation 特有的两件事：把事件正文拆成内容单元，以及按交互 Principal 取
//! scope 后调用内容存储登记/读取。写入时的事务与锁由调用方持有。

use serde_json::Value;
use sqlx::{PgConnection, SqliteConnection};

use crate::content_store::{self, Body, Bodies, Content};
use stravia_runtime_contract::protocol::ir::content::{ContentUnit, ContentUnitBody};

/// 一个文本内容单元。
pub(super) fn text_content(text: impl Into<String>) -> Content {
    Content::text(text)
}

/// 工具结果载荷：字符串按文本单元保存，其他 JSON 压成紧凑 JSON 单元。
pub(super) fn value_content(value: &Value) -> Content {
    match value {
        Value::String(text) => Content::text(text.clone()),
        other => Content::json(other.to_string().into_bytes()),
    }
}

/// 把共享定义产出的正文单元复制成可写入的内容。
pub(super) fn unit_content(unit: &ContentUnit<'_>) -> Content {
    match unit.body {
        ContentUnitBody::Text(text) => Content::text(text),
        ContentUnitBody::Json(value) => Content::json(value.to_string().into_bytes()),
    }
}

/// 还原成调用方看到的 JSON 值：文本包装为字符串，JSON 原样解析。
pub(super) fn body_value(body: &Body) -> anyhow::Result<Value> {
    use stravia_runtime_contract::protocol::ir::content::ContentFormat;
    Ok(match body.format {
        ContentFormat::Text => Value::String(body.text()?.to_owned()),
        ContentFormat::Json => body.json()?,
    })
}

/// 在调用方事务内登记正文，返回与 `contents` 同序的 content id。
pub(super) async fn intern_sqlite(
    connection: &mut SqliteConnection<'_>,
    principal: &str,
    contents: &mut [Content],
) -> anyhow::Result<Vec<i64>> {
    if contents.is_empty() {
        return Ok(Vec::new());
    }
    let scope = content_store::sqlite::resolve_scope(connection, principal).await?;
    content_store::sqlite::intern(connection, scope, contents).await
}

pub(super) async fn intern_postgres(
    connection: &mut PgConnection<'_>,
    principal: &str,
    contents: &mut [Content],
) -> anyhow::Result<Vec<i64>> {
    if contents.is_empty() {
        return Ok(Vec::new());
    }
    let scope = content_store::postgres::resolve_scope(connection, principal).await?;
    content_store::postgres::intern(connection, scope, contents).await
}

/// 读取某个 Principal 的一批正文；该 Principal 没有 scope 时没有正文。
pub(super) async fn read_scoped(
    store: &super::store::ObservationStore,
    principal: &str,
    content_ids: &[i64],
) -> anyhow::Result<Bodies> {
    use super::store::ObservationStore;
    if content_ids.is_empty() {
        return Ok(Bodies::from_rows(Vec::new()));
    }
    let rows = match store {
        ObservationStore::Sqlite(pool, _) => {
            let mut connection = pool.acquire().await?;
            let Some(scope) = content_store::sqlite::scope_id(&mut connection, principal).await?
            else {
                return Ok(Bodies::from_rows(Vec::new()));
            };
            content_store::sqlite::read(&mut connection, scope, content_ids).await?
        }
        ObservationStore::Postgres(pool, _) => {
            let mut connection = pool.acquire().await?;
            let Some(scope) =
                content_store::postgres::scope_id(&mut connection, principal).await?
            else {
                return Ok(Bodies::from_rows(Vec::new()));
            };
            content_store::postgres::read(&mut connection, scope, content_ids).await?
        }
    };
    Ok(Bodies::from_rows(rows))
}

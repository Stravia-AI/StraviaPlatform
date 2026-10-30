use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use std::time::Duration;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::turn_chain::*;

#[derive(sqlx::FromRow)]
struct StoredAncestorRow {
    node_pk: i64,
    scope_id: i64,
    content_references: sqlx::types::Json<Vec<[i64; 3]>>,
    parent_pk: Option<i64>,
    id: String,
    parent_id: Option<String>,
    payload_version: i64,
    payload: Option<Vec<u8>>,
    expires_at: Option<i64>,
}

mod content;
mod gc;
mod sql;
pub(crate) mod upgrade;

pub use sql::SqlTurnChainStore;

#[cfg(test)]
pub(crate) async fn test_store() -> SqlTurnChainStore {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite Turn Chain test database");
    SqlTurnChainStore::sqlite(pool)
}

#[cfg(test)]
mod content_tests;
#[cfg(test)]
mod tests;

use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use std::time::Duration;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::turn_chain::*;

mod sql;

pub use sql::SqlTurnChainStore;

#[cfg(test)]
pub(crate) async fn test_store() -> SqlTurnChainStore {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("SQLite Turn Chain test pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite Turn Chain test migrations");
    SqlTurnChainStore::sqlite(pool)
}

#[cfg(test)]
mod tests;

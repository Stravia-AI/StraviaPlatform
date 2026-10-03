use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use std::time::Duration;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::turn_chain::*;

mod content;
mod sql;
pub(crate) mod upgrade;

pub use sql::SqlTurnChainStore;

#[cfg(test)]
pub(crate) async fn test_store() -> SqlTurnChainStore {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite Turn Chain test database");
    SqlTurnChainStore::sqlite(pool, std::sync::Arc::new(tokio::sync::Mutex::new(())))
}

#[cfg(test)]
mod content_tests;
#[cfg(test)]
mod tests;

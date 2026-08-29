use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, SqlitePool};

use crate::hook::Principal;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnNodeId(String);

impl TurnNodeId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn response() -> Self {
        Self(format!("resp_{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn agent() -> Self {
        Self(format!("aturn_{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn web_search() -> Self {
        Self(format!("wst_{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TurnNodeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnNodeKind {
    Response,
    Agent,
    WebSearch,
}

impl TurnNodeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Agent => "agent",
            Self::WebSearch => "web_search",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnNode {
    pub id: TurnNodeId,
    pub kind: TurnNodeKind,
    pub parent_id: Option<TurnNodeId>,
    pub payload_version: u32,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct MaterializedTurnChain {
    pub nodes: Vec<TurnNode>,
    pub expires_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct TurnCommit {
    pub id: TurnNodeId,
    pub kind: TurnNodeKind,
    pub parent_id: Option<TurnNodeId>,
    pub principal: Principal,
    pub payload_version: u32,
    pub payload: Value,
    pub idle_ttl: Duration,
    pub reusable_prefix: Option<ReusablePrefixMetadata>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixMetadata {
    pub namespace: String,
    pub fingerprint: String,
    pub item_count: u32,
    pub completed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixQuery {
    pub namespace: String,
    pub fingerprints: Vec<(String, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixCandidate {
    pub node_id: TurnNodeId,
    pub item_count: u32,
    pub completed_at: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TurnUnavailable {
    #[error("turn unavailable")]
    Unavailable,
    #[error("turn storage failed: {0}")]
    Storage(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TurnCommitError {
    #[error("turn parent unavailable")]
    ParentUnavailable,
    #[error("turn already exists")]
    AlreadyExists,
    #[error("turn storage failed: {0}")]
    Storage(String),
}

#[async_trait]
pub trait TurnChainStore: Send + Sync {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<Vec<TurnNode>, TurnUnavailable>;

    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<MaterializedTurnChain, TurnUnavailable> {
        Ok(MaterializedTurnChain {
            nodes: self.materialize(principal, kind, id).await?,
            // External adapters that have not adopted the richer contract
            // remain correct by declining to retain a materialization.
            expires_at: std::time::Instant::now(),
        })
    }

    async fn commit(&self, commit: TurnCommit) -> Result<TurnNodeId, TurnCommitError>;

    async fn find_reusable_prefixes(
        &self,
        _principal: &Principal,
        _kind: TurnNodeKind,
        _query: &ReusablePrefixQuery,
    ) -> Result<Vec<ReusablePrefixCandidate>, TurnUnavailable> {
        Ok(Vec::new())
    }

    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable>;
}

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

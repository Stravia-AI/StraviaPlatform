use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use tokio::sync::RwLock;

use super::definition::{
    AgentDefinitionConfig, AgentDefinitionError, AgentDefinitionId, AgentDefinitionSpec,
};

#[async_trait]
pub(crate) trait AgentDefinitionStore: Send + Sync {
    async fn synchronize_revision(
        &self,
        spec: &AgentDefinitionSpec,
        spec_hash: &str,
    ) -> Result<AgentDefinitionConfig, AgentDefinitionError>;

    async fn load_revision(
        &self,
        id: &AgentDefinitionId,
        revision: u32,
    ) -> Result<Option<(AgentDefinitionSpec, String)>, AgentDefinitionError>;

    async fn patch_config(
        &self,
        id: &AgentDefinitionId,
        config: AgentDefinitionConfig,
    ) -> Result<AgentDefinitionConfig, AgentDefinitionError>;
}
type RevisionKey = (AgentDefinitionId, u32);
type StoredRevision = (AgentDefinitionSpec, String);
type RevisionMap = HashMap<RevisionKey, StoredRevision>;
type RevisionStore = Arc<RwLock<RevisionMap>>;

#[derive(Clone, Default)]
pub(crate) struct MemoryAgentDefinitionStore {
    revisions: RevisionStore,
    configs: Arc<RwLock<HashMap<AgentDefinitionId, AgentDefinitionConfig>>>,
}

#[async_trait]
impl AgentDefinitionStore for MemoryAgentDefinitionStore {
    async fn synchronize_revision(
        &self,
        spec: &AgentDefinitionSpec,
        spec_hash: &str,
    ) -> Result<AgentDefinitionConfig, AgentDefinitionError> {
        let key = (spec.id.clone(), spec.revision);
        let mut revisions = self.revisions.write().await;
        if let Some((_, existing_hash)) = revisions.get(&key) {
            if existing_hash != spec_hash {
                return Err(AgentDefinitionError::Invalid(format!(
                    "revision {} for {} changed without a version bump",
                    spec.revision,
                    spec.id.as_str()
                )));
            }
        } else {
            revisions.insert(key, (spec.clone(), spec_hash.to_owned()));
        }
        drop(revisions);
        Ok(self
            .configs
            .write()
            .await
            .entry(spec.id.clone())
            .or_default()
            .clone())
    }

    async fn load_revision(
        &self,
        id: &AgentDefinitionId,
        revision: u32,
    ) -> Result<Option<(AgentDefinitionSpec, String)>, AgentDefinitionError> {
        Ok(self
            .revisions
            .read()
            .await
            .get(&(id.clone(), revision))
            .cloned())
    }

    async fn patch_config(
        &self,
        id: &AgentDefinitionId,
        config: AgentDefinitionConfig,
    ) -> Result<AgentDefinitionConfig, AgentDefinitionError> {
        let mut configs = self.configs.write().await;
        let current = configs.get_mut(id).ok_or(AgentDefinitionError::NotFound)?;
        *current = config.clone();
        Ok(config)
    }
}

#[derive(Clone)]
pub(crate) enum SqlAgentDefinitionStore {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

impl SqlAgentDefinitionStore {
    pub(crate) fn sqlite(pool: SqlitePool) -> Self {
        Self::Sqlite(pool)
    }

    pub(crate) fn postgres(pool: PgPool) -> Self {
        Self::Postgres(pool)
    }
}

fn encode_spec(spec: &AgentDefinitionSpec) -> Result<String, AgentDefinitionError> {
    serde_json::to_string(spec).map_err(|error| AgentDefinitionError::Storage(error.to_string()))
}

fn decode_spec(value: String) -> Result<AgentDefinitionSpec, AgentDefinitionError> {
    serde_json::from_str(&value).map_err(|error| AgentDefinitionError::Storage(error.to_string()))
}

#[async_trait]
impl AgentDefinitionStore for SqlAgentDefinitionStore {
    async fn synchronize_revision(
        &self,
        spec: &AgentDefinitionSpec,
        spec_hash: &str,
    ) -> Result<AgentDefinitionConfig, AgentDefinitionError> {
        let spec_json = encode_spec(spec)?;
        let now = chrono::Utc::now().timestamp_millis();
        match self {
            Self::Sqlite(pool) => {
                let mut transaction = pool
                    .begin()
                    .await
                    .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                sqlx::query(
                    "INSERT OR IGNORE INTO agent_definition_revisions \
                     (definition_id, slug, version, spec_hash, spec_json, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(spec.id.as_str())
                .bind(spec.slug.as_str())
                .bind(i64::from(spec.revision))
                .bind(spec_hash)
                .bind(&spec_json)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                let (existing_hash,): (String,) = sqlx::query_as(
                    "SELECT spec_hash FROM agent_definition_revisions \
                     WHERE definition_id = ? AND version = ?",
                )
                .bind(spec.id.as_str())
                .bind(i64::from(spec.revision))
                .fetch_one(&mut *transaction)
                .await
                .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                if existing_hash != spec_hash {
                    return Err(revision_mismatch(spec));
                }
                sqlx::query(
                    "INSERT OR IGNORE INTO agent_definition_configs \
                     (definition_id, enabled, model_id, updated_at) VALUES (?, 0, NULL, ?)",
                )
                .bind(spec.id.as_str())
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                let (enabled, model_id, thinking_level): (bool, Option<String>, Option<String>) =
                    sqlx::query_as(
                        "SELECT enabled, model_id, thinking_level FROM agent_definition_configs \
                         WHERE definition_id = ?",
                    )
                    .bind(spec.id.as_str())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                transaction
                    .commit()
                    .await
                    .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                Ok(AgentDefinitionConfig {
                    enabled,
                    model_id,
                    thinking_level: decode_thinking_level(thinking_level)?,
                })
            }
            Self::Postgres(pool) => {
                let mut transaction = pool
                    .begin()
                    .await
                    .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                sqlx::query(
                    "INSERT INTO agent_definition_revisions \
                     (definition_id, slug, version, spec_hash, spec_json, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(spec.id.as_str())
                .bind(spec.slug.as_str())
                .bind(i64::from(spec.revision))
                .bind(spec_hash)
                .bind(&spec_json)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                let (existing_hash,): (String,) = sqlx::query_as(
                    "SELECT spec_hash FROM agent_definition_revisions \
                     WHERE definition_id = $1 AND version = $2 FOR UPDATE",
                )
                .bind(spec.id.as_str())
                .bind(i64::from(spec.revision))
                .fetch_one(&mut *transaction)
                .await
                .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                if existing_hash != spec_hash {
                    return Err(revision_mismatch(spec));
                }
                sqlx::query(
                    "INSERT INTO agent_definition_configs \
                     (definition_id, enabled, model_id, updated_at) VALUES ($1, FALSE, NULL, $2) \
                     ON CONFLICT (definition_id) DO NOTHING",
                )
                .bind(spec.id.as_str())
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                let (enabled, model_id, thinking_level): (bool, Option<String>, Option<String>) =
                    sqlx::query_as(
                        "SELECT enabled, model_id, thinking_level FROM agent_definition_configs \
                         WHERE definition_id = $1",
                    )
                    .bind(spec.id.as_str())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                transaction
                    .commit()
                    .await
                    .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
                Ok(AgentDefinitionConfig {
                    enabled,
                    model_id,
                    thinking_level: decode_thinking_level(thinking_level)?,
                })
            }
        }
    }

    async fn load_revision(
        &self,
        id: &AgentDefinitionId,
        revision: u32,
    ) -> Result<Option<(AgentDefinitionSpec, String)>, AgentDefinitionError> {
        let row: Option<(String, String)> = match self {
            Self::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT spec_json, spec_hash FROM agent_definition_revisions \
                 WHERE definition_id = ? AND version = ?",
                )
                .bind(id.as_str())
                .bind(i64::from(revision))
                .fetch_optional(pool)
                .await
            }
            Self::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT spec_json, spec_hash FROM agent_definition_revisions \
                 WHERE definition_id = $1 AND version = $2",
                )
                .bind(id.as_str())
                .bind(i64::from(revision))
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
        row.map(|(spec, hash)| decode_spec(spec).map(|spec| (spec, hash)))
            .transpose()
    }

    async fn patch_config(
        &self,
        id: &AgentDefinitionId,
        config: AgentDefinitionConfig,
    ) -> Result<AgentDefinitionConfig, AgentDefinitionError> {
        let now = chrono::Utc::now().timestamp_millis();
        let rows = match self {
            Self::Sqlite(pool) => sqlx::query(
                "UPDATE agent_definition_configs \
                 SET enabled = ?, model_id = ?, thinking_level = ?, updated_at = ? \
                 WHERE definition_id = ?",
            )
            .bind(config.enabled)
            .bind(&config.model_id)
            .bind(config.thinking_level.map(|level| level.as_str()))
            .bind(now)
            .bind(id.as_str())
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            Self::Postgres(pool) => sqlx::query(
                "UPDATE agent_definition_configs \
                 SET enabled = $1, model_id = $2, thinking_level = $3, updated_at = $4 \
                 WHERE definition_id = $5",
            )
            .bind(config.enabled)
            .bind(&config.model_id)
            .bind(config.thinking_level.map(|level| level.as_str()))
            .bind(now)
            .bind(id.as_str())
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
        }
        .map_err(|error| AgentDefinitionError::Storage(error.to_string()))?;
        if rows == 0 {
            return Err(AgentDefinitionError::NotFound);
        }
        Ok(config)
    }
}

fn decode_thinking_level(
    value: Option<String>,
) -> Result<Option<crate::thinking::ThinkingLevel>, AgentDefinitionError> {
    value
        .map(|value| {
            crate::thinking::ThinkingLevel::from_wire(&value)
                .map_err(|error| AgentDefinitionError::Storage(error.to_string()))
        })
        .transpose()
}

fn revision_mismatch(spec: &AgentDefinitionSpec) -> AgentDefinitionError {
    AgentDefinitionError::Invalid(format!(
        "revision {} for {} changed without a version bump",
        spec.revision,
        spec.id.as_str()
    ))
}

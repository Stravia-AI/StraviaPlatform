use std::time::Duration;

use async_trait::async_trait;
use sqlx::Connection;

use super::RedactionError;
use crate::hook::Principal;

const PENDING_RETENTION: Duration = Duration::from_secs(60 * 60);
const PUBLISHED_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

// Intentionally no Debug: mapping plaintext must never enter diagnostics.
#[derive(Clone, sqlx::FromRow)]
pub(crate) struct Mapping {
    pub reference: String,
    pub secret: String,
    pub expires_at: i64,
}

#[async_trait]
pub(crate) trait MappingStore: Send + Sync {
    async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError>;
    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[String],
    ) -> Result<Vec<Mapping>, RedactionError>;
    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError>;
    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError>;
    async fn cleanup_expired(&self) -> Result<u64, RedactionError>;
}

#[derive(Clone)]
pub(crate) enum SqlMappingStore {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

fn storage(_: sqlx::Error) -> RedactionError {
    RedactionError::Storage
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn after(now: i64, retention: Duration) -> i64 {
    now.saturating_add(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX))
}

impl SqlMappingStore {
    pub(crate) fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self::Sqlite(pool)
    }

    pub(crate) fn postgres(pool: sqlx::PgPool) -> Self {
        Self::Postgres(pool)
    }

    async fn retain(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
        publish: bool,
    ) -> Result<(), RedactionError> {
        if references.is_empty() {
            return Ok(());
        }
        let principal = principal.continuation_key();
        let retention = if publish {
            retention.max(PUBLISHED_RETENTION)
        } else {
            retention
        };
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool.acquire().await.map_err(storage)?;
                let mut transaction = connection
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(storage)?;
                let now = now();
                let expires_at = after(now, retention);
                for reference in references {
                    sqlx::query(
                        "UPDATE reversible_redaction_mappings SET \
                         published_at = CASE WHEN ? THEN COALESCE(published_at, ?) ELSE published_at END, \
                         expires_at = MAX(expires_at, ?), updated_at = ? \
                         WHERE principal = ? AND reference = ? AND expires_at > ? \
                         AND (? OR published_at IS NOT NULL)",
                    )
                    .bind(publish).bind(now).bind(expires_at).bind(now)
                    .bind(&principal).bind(reference).bind(now).bind(publish)
                    .execute(&mut *transaction).await.map_err(storage)?;
                }
                transaction.commit().await.map_err(storage)?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage)?;
                // The same principal lock as intern makes lifecycle decisions after any writer wait.
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(&principal)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage)?;
                let now = now();
                let expires_at = after(now, retention);
                for reference in references {
                    sqlx::query(
                        "UPDATE reversible_redaction_mappings SET \
                         published_at = CASE WHEN $1 THEN COALESCE(published_at, $2) ELSE published_at END, \
                         expires_at = GREATEST(expires_at, $3), updated_at = $2 \
                         WHERE principal = $4 AND reference = $5 AND expires_at > $2 \
                         AND ($1 OR published_at IS NOT NULL)",
                    )
                    .bind(publish).bind(now).bind(expires_at).bind(&principal).bind(reference)
                    .execute(&mut *transaction).await.map_err(storage)?;
                }
                transaction.commit().await.map_err(storage)?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MappingStore for SqlMappingStore {
    async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError> {
        let principal = principal.continuation_key();
        match self {
            Self::Sqlite(pool) => sqlx::query_as::<_, Mapping>(
                "SELECT reference, secret, expires_at FROM reversible_redaction_mappings \
                 WHERE principal = ? AND expires_at > ? ORDER BY reference",
            )
            .bind(principal)
            .bind(now())
            .fetch_all(pool)
            .await
            .map_err(storage),
            Self::Postgres(pool) => sqlx::query_as::<_, Mapping>(
                "SELECT reference, secret, expires_at FROM reversible_redaction_mappings \
                 WHERE principal = $1 AND expires_at > $2 ORDER BY reference",
            )
            .bind(principal)
            .bind(now())
            .fetch_all(pool)
            .await
            .map_err(storage),
        }
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[String],
    ) -> Result<Vec<Mapping>, RedactionError> {
        if secrets.is_empty() {
            return Ok(Vec::new());
        }
        let principal = principal.continuation_key();
        let mut mappings = Vec::with_capacity(secrets.len());
        // Serialize lookup-and-insert across processes, not just this store instance.
        // Secrets can be arbitrarily long (e.g. private keys), so do not B-tree index them.
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool.acquire().await.map_err(storage)?;
                let mut transaction = connection
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(storage)?;
                let now = now();
                for secret in secrets {
                    let existing = sqlx::query_as::<_, Mapping>(
                        "SELECT reference, secret, expires_at FROM reversible_redaction_mappings \
                         WHERE principal = ? AND secret = ? AND expires_at > ?",
                    )
                    .bind(&principal)
                    .bind(secret)
                    .bind(now)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(storage)?;
                    let mapping = match existing {
                        Some(mapping) => mapping,
                        None => {
                            let reference =
                                format!("~stravia-secret:{}~", uuid::Uuid::new_v4().simple());
                            sqlx::query(
                                "INSERT INTO reversible_redaction_mappings \
                                 (reference, principal, secret, created_at, updated_at, expires_at) \
                                 VALUES (?, ?, ?, ?, ?, ?)",
                            ).bind(&reference).bind(&principal).bind(secret).bind(now).bind(now)
                                .bind(after(now, PENDING_RETENTION))
                                .execute(&mut *transaction).await.map_err(storage)?;
                            Mapping {
                                reference,
                                secret: secret.clone(),
                                expires_at: after(now, PENDING_RETENTION),
                            }
                        }
                    };
                    mappings.push(mapping);
                }
                transaction.commit().await.map_err(storage)?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage)?;
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(&principal)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage)?;
                let now = now();
                for secret in secrets {
                    let existing = sqlx::query_as::<_, Mapping>(
                        "SELECT reference, secret, expires_at FROM reversible_redaction_mappings \
                         WHERE principal = $1 AND secret = $2 AND expires_at > $3",
                    )
                    .bind(&principal)
                    .bind(secret)
                    .bind(now)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(storage)?;
                    let mapping = match existing {
                        Some(mapping) => mapping,
                        None => {
                            let reference =
                                format!("~stravia-secret:{}~", uuid::Uuid::new_v4().simple());
                            sqlx::query(
                                "INSERT INTO reversible_redaction_mappings \
                                 (reference, principal, secret, created_at, updated_at, expires_at) \
                                 VALUES ($1, $2, $3, $4, $4, $5)",
                            ).bind(&reference).bind(&principal).bind(secret).bind(now)
                                .bind(after(now, PENDING_RETENTION))
                                .execute(&mut *transaction).await.map_err(storage)?;
                            Mapping {
                                reference,
                                secret: secret.clone(),
                                expires_at: after(now, PENDING_RETENTION),
                            }
                        }
                    };
                    mappings.push(mapping);
                }
                transaction.commit().await.map_err(storage)?;
            }
        }
        Ok(mappings)
    }

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError> {
        self.retain(principal, references, retention, true).await
    }

    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError> {
        self.retain(principal, references, retention, false).await
    }

    async fn cleanup_expired(&self) -> Result<u64, RedactionError> {
        match self {
            Self::Sqlite(pool) => {
                sqlx::query("DELETE FROM reversible_redaction_mappings WHERE expires_at <= ?")
                    .bind(now())
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
                    .map_err(storage)
            }
            Self::Postgres(pool) => {
                sqlx::query("DELETE FROM reversible_redaction_mappings WHERE expires_at <= $1")
                    .bind(now())
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
                    .map_err(storage)
            }
        }
    }
}

#[cfg(test)]
mod tests;

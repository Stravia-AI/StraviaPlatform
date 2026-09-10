use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use sqlx::{PgPool, SqlitePool};
use tokio::io::AsyncReadExt;

use crate::host::MediaArtifactHost;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::artifact::{ArtifactError, ArtifactId, ArtifactRef, ArtifactSource};

#[derive(Debug, Clone)]
pub struct MediaDerivative {
    pub derivative: ArtifactRef,
}

#[derive(Debug, thiserror::Error)]
pub enum MediaStoreError {
    #[error("Media Artifact is unavailable")]
    Unavailable,
    #[error("Media Artifact exceeds its byte limit")]
    TooLarge,
    #[error("Media Derivative mapping is corrupt")]
    Corrupt,
    #[error("Media storage failed: {0}")]
    Storage(String),
}

enum MediaDatabase {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

pub struct MediaDerivativeStore {
    database: MediaDatabase,
    artifacts: Arc<dyn MediaArtifactHost>,
}

impl MediaDerivativeStore {
    pub fn sqlite(pool: SqlitePool, artifacts: Arc<dyn MediaArtifactHost>) -> Self {
        Self {
            database: MediaDatabase::Sqlite(pool),
            artifacts,
        }
    }

    pub fn postgres(pool: PgPool, artifacts: Arc<dyn MediaArtifactHost>) -> Self {
        Self {
            database: MediaDatabase::Postgres(pool),
            artifacts,
        }
    }

    pub async fn create_source(
        &self,
        principal: &Principal,
        mime_type: &str,
        bytes: Bytes,
        retention: Duration,
    ) -> Result<ArtifactRef, MediaStoreError> {
        self.artifacts
            .create_ready_bytes(principal, mime_type, bytes, retention)
            .await
            .map_err(MediaStoreError::from)
    }

    pub async fn inspect_artifact(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<ArtifactRef, MediaStoreError> {
        self.artifacts
            .open(principal, id)
            .await
            .map(|reader| reader.artifact)
            .map_err(MediaStoreError::from)
    }

    pub async fn read_artifact_bounded(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        max_bytes: u64,
    ) -> Result<(ArtifactRef, Bytes), MediaStoreError> {
        let reader = self
            .artifacts
            .open(principal, id)
            .await
            .map_err(MediaStoreError::from)?;
        if reader.artifact.size == 0 || reader.artifact.size > max_bytes {
            return Err(MediaStoreError::TooLarge);
        }
        let ArtifactSource::LocalPath(path) = &reader.source else {
            return Err(MediaStoreError::Corrupt);
        };
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| MediaStoreError::Corrupt)?;
        let capacity = usize::try_from(reader.artifact.size).unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| MediaStoreError::Corrupt)?;
        if bytes.len() as u64 > max_bytes {
            return Err(MediaStoreError::TooLarge);
        }
        if bytes.len() as u64 != reader.artifact.size {
            return Err(MediaStoreError::Corrupt);
        }
        Ok((reader.artifact, Bytes::from(bytes)))
    }

    pub async fn find_derivative(
        &self,
        principal: &Principal,
        source_id: &ArtifactId,
    ) -> Result<Option<MediaDerivative>, MediaStoreError> {
        self.artifacts
            .open(principal, source_id)
            .await
            .map_err(MediaStoreError::from)?;
        let Some(derivative_id) = self.mapped_derivative(principal, source_id).await? else {
            return Ok(None);
        };
        let derivative = self.verified_derivative(principal, &derivative_id).await?;
        Ok(Some(MediaDerivative { derivative }))
    }

    pub async fn source_for_derivative(
        &self,
        principal: &Principal,
        derivative_id: &ArtifactId,
    ) -> Result<Option<ArtifactId>, MediaStoreError> {
        let principal_key = principal.continuation_key();
        let source_id = match &self.database {
            MediaDatabase::Sqlite(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT source_artifact_id FROM media_derivatives WHERE principal = ? AND derivative_artifact_id = ?",
                )
                .bind(&principal_key)
                .bind(derivative_id.as_str())
                .fetch_optional(pool)
                .await
            }
            MediaDatabase::Postgres(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT source_artifact_id FROM media_derivatives WHERE principal = $1 AND derivative_artifact_id = $2",
                )
                .bind(&principal_key)
                .bind(derivative_id.as_str())
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(|error| MediaStoreError::Storage(error.to_string()))?
        .map(ArtifactId::new);
        let Some(source_id) = source_id else {
            return Ok(None);
        };
        self.artifacts
            .open(principal, &source_id)
            .await
            .map_err(|_| MediaStoreError::Corrupt)?;
        self.verified_derivative(principal, derivative_id).await?;
        Ok(Some(source_id))
    }

    pub async fn get_or_create_derivative(
        &self,
        principal: &Principal,
        source_id: &ArtifactId,
        bytes: Bytes,
        retention: Duration,
    ) -> Result<MediaDerivative, MediaStoreError> {
        if let Some(existing) = self.find_derivative(principal, source_id).await? {
            return Ok(existing);
        }
        self.artifacts
            .open(principal, source_id)
            .await
            .map_err(MediaStoreError::from)?;
        let retention = retention.max(
            self.remaining_source_retention(principal, source_id)
                .await?,
        );

        let candidate = self
            .artifacts
            .create_ready_bytes(principal, "image/jpeg", bytes, retention)
            .await
            .map_err(MediaStoreError::from)?;
        let created_at = chrono::Utc::now().timestamp_millis();
        let principal_key = principal.continuation_key();
        let insertion = match &self.database {
            MediaDatabase::Sqlite(pool) => {
                sqlx::query(
                    "INSERT OR IGNORE INTO media_derivatives (principal, source_artifact_id, derivative_artifact_id, created_at) VALUES (?, ?, ?, ?)",
                )
                .bind(&principal_key)
                .bind(source_id.as_str())
                .bind(candidate.id.as_str())
                .bind(created_at)
                .execute(pool)
                .await
                .map(|result| result.rows_affected())
            }
            MediaDatabase::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO media_derivatives (principal, source_artifact_id, derivative_artifact_id, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT (source_artifact_id) DO NOTHING",
                )
                .bind(&principal_key)
                .bind(source_id.as_str())
                .bind(candidate.id.as_str())
                .bind(created_at)
                .execute(pool)
                .await
                .map(|result| result.rows_affected())
            }
        };
        let won = match insertion {
            Ok(rows_affected) => rows_affected == 1,
            Err(error) => {
                self.artifacts
                    .delete_ready(principal, &candidate.id)
                    .await
                    .map_err(MediaStoreError::from)?;
                return Err(MediaStoreError::Storage(error.to_string()));
            }
        };
        if won {
            return Ok(MediaDerivative {
                derivative: candidate,
            });
        }

        self.artifacts
            .delete_ready(principal, &candidate.id)
            .await
            .map_err(MediaStoreError::from)?;
        let derivative_id = self
            .mapped_derivative(principal, source_id)
            .await?
            .ok_or(MediaStoreError::Corrupt)?;
        let derivative = self.verified_derivative(principal, &derivative_id).await?;
        Ok(MediaDerivative { derivative })
    }

    pub async fn promote(
        &self,
        principal: &Principal,
        artifacts: &[ArtifactId],
        retention: Duration,
    ) -> Result<(), MediaStoreError> {
        self.artifacts
            .extend_retention(principal, artifacts, retention)
            .await
            .map_err(MediaStoreError::from)
    }

    async fn remaining_source_retention(
        &self,
        principal: &Principal,
        source_id: &ArtifactId,
    ) -> Result<Duration, MediaStoreError> {
        let principal_key = principal.continuation_key();
        let expires_at = match &self.database {
            MediaDatabase::Sqlite(pool) => {
                sqlx::query_scalar::<_, i64>(
                    "SELECT expires_at FROM artifacts WHERE id = ? AND principal = ? AND state = 'ready'",
                )
                .bind(source_id.as_str())
                .bind(&principal_key)
                .fetch_optional(pool)
                .await
            }
            MediaDatabase::Postgres(pool) => {
                sqlx::query_scalar::<_, i64>(
                    "SELECT expires_at FROM artifacts WHERE id = $1 AND principal = $2 AND state = 'ready'",
                )
                .bind(source_id.as_str())
                .bind(&principal_key)
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(|error| MediaStoreError::Storage(error.to_string()))?
        .ok_or(MediaStoreError::Unavailable)?;
        let remaining_ms = expires_at.saturating_sub(chrono::Utc::now().timestamp_millis());
        Ok(Duration::from_millis(
            u64::try_from(remaining_ms).unwrap_or_default(),
        ))
    }

    async fn mapped_derivative(
        &self,
        principal: &Principal,
        source_id: &ArtifactId,
    ) -> Result<Option<ArtifactId>, MediaStoreError> {
        let principal_key = principal.continuation_key();
        let id = match &self.database {
            MediaDatabase::Sqlite(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT derivative_artifact_id FROM media_derivatives WHERE principal = ? AND source_artifact_id = ?",
                )
                .bind(&principal_key)
                .bind(source_id.as_str())
                .fetch_optional(pool)
                .await
            }
            MediaDatabase::Postgres(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT derivative_artifact_id FROM media_derivatives WHERE principal = $1 AND source_artifact_id = $2",
                )
                .bind(&principal_key)
                .bind(source_id.as_str())
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(|error| MediaStoreError::Storage(error.to_string()))?;
        Ok(id.map(ArtifactId::new))
    }

    async fn verified_derivative(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<ArtifactRef, MediaStoreError> {
        let (artifact, bytes) = self
            .read_artifact_bounded(principal, id, super::MAX_DERIVATIVE_BYTES as u64)
            .await
            .map_err(|_| MediaStoreError::Corrupt)?;
        if artifact.mime_type != "image/jpeg"
            || image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).is_err()
        {
            return Err(MediaStoreError::Corrupt);
        }
        Ok(artifact)
    }
}

impl From<ArtifactError> for MediaStoreError {
    fn from(error: ArtifactError) -> Self {
        match error {
            ArtifactError::NotFound | ArtifactError::Forbidden | ArtifactError::Unauthorized => {
                Self::Unavailable
            }
            ArtifactError::Invalid(message) | ArtifactError::Storage(message) => {
                Self::Storage(message)
            }
        }
    }
}

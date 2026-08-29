use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use sqlx::{PgPool, SqlitePool};
use tokio::io::AsyncReadExt;

use crate::agent::{
    ArtifactError, ArtifactId, ArtifactRef, ArtifactSource, ArtifactStore, LocalArtifactStore,
};
use crate::hook::Principal;

#[derive(Debug, Clone)]
pub(crate) struct MediaDerivative {
    pub derivative: ArtifactRef,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MediaStoreError {
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

pub(crate) struct MediaDerivativeStore {
    database: MediaDatabase,
    artifacts: Arc<LocalArtifactStore>,
}

impl MediaDerivativeStore {
    pub fn sqlite(pool: SqlitePool, artifacts: Arc<LocalArtifactStore>) -> Self {
        Self {
            database: MediaDatabase::Sqlite(pool),
            artifacts,
        }
    }

    pub fn postgres(pool: PgPool, artifacts: Arc<LocalArtifactStore>) -> Self {
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
        let ArtifactSource::LocalPath(path) = reader.source else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ArtifactSource, ArtifactStore};

    fn jpeg(value: u8) -> Bytes {
        use image::ImageEncoder;

        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .write_image(&[value, value, value], 1, 1, image::ExtendedColorType::Rgb8)
            .expect("encode JPEG");
        Bytes::from(bytes)
    }

    #[tokio::test]
    async fn derivative_mapping_has_one_winner_and_survives_reconstruction() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let root = data_dir.path().join("artifacts");
        let artifacts = Arc::new(LocalArtifactStore::sqlite(pool.clone(), &root));
        let store = MediaDerivativeStore::sqlite(pool.clone(), Arc::clone(&artifacts));
        let owner = Principal::new("owner");
        let source = store
            .create_source(
                &owner,
                "image/png",
                Bytes::from_static(b"source"),
                Duration::from_secs(2 * 60 * 60),
            )
            .await
            .expect("source snapshot");

        let first_bytes = jpeg(1);
        let second_bytes = jpeg(2);
        let first = store.get_or_create_derivative(
            &owner,
            &source.id,
            first_bytes.clone(),
            Duration::from_secs(60),
        );
        let second = store.get_or_create_derivative(
            &owner,
            &source.id,
            second_bytes.clone(),
            Duration::from_secs(60),
        );
        let (first, second) = tokio::join!(first, second);
        let first = first.expect("first derivative");
        let second = second.expect("second derivative");
        assert_eq!(first.derivative.id, second.derivative.id);
        let source_expiry: i64 =
            sqlx::query_scalar("SELECT expires_at FROM artifacts WHERE id = ?")
                .bind(source.id.as_str())
                .fetch_one(&pool)
                .await
                .expect("source expiry");
        let derivative_expiry: i64 =
            sqlx::query_scalar("SELECT expires_at FROM artifacts WHERE id = ?")
                .bind(first.derivative.id.as_str())
                .fetch_one(&pool)
                .await
                .expect("derivative expiry");
        assert!(derivative_expiry >= source_expiry);

        let reconstructed_artifacts = Arc::new(LocalArtifactStore::sqlite(pool.clone(), &root));
        let reconstructed =
            MediaDerivativeStore::sqlite(pool, Arc::clone(&reconstructed_artifacts));
        let reused = reconstructed
            .get_or_create_derivative(&owner, &source.id, jpeg(3), Duration::from_secs(60))
            .await
            .expect("reused derivative");
        assert_eq!(reused.derivative.id, first.derivative.id);
        let reader = reconstructed_artifacts
            .open(&owner, &reused.derivative.id)
            .await
            .expect("open derivative");
        let ArtifactSource::LocalPath(path) = reader.source else {
            panic!("expected local derivative");
        };
        let bytes = tokio::fs::read(path).await.expect("read derivative");
        assert!(bytes == first_bytes || bytes == second_bytes);
    }

    #[tokio::test]
    async fn failed_mapping_insert_cleans_up_derivative_candidate() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let artifacts = Arc::new(LocalArtifactStore::sqlite(
            pool.clone(),
            data_dir.path().join("artifacts"),
        ));
        let store = MediaDerivativeStore::sqlite(pool.clone(), Arc::clone(&artifacts));
        let owner = Principal::new("owner");
        let source = store
            .create_source(
                &owner,
                "image/png",
                Bytes::from_static(b"source"),
                Duration::from_secs(60),
            )
            .await
            .expect("source");
        sqlx::query(
            "CREATE TRIGGER reject_media_derivative_insert BEFORE INSERT ON media_derivatives BEGIN SELECT RAISE(ABORT, 'rejected'); END",
        )
        .execute(&pool)
        .await
        .expect("failure trigger");

        assert!(matches!(
            store
                .get_or_create_derivative(&owner, &source.id, jpeg(1), Duration::from_secs(60),)
                .await,
            Err(MediaStoreError::Storage(_))
        ));
        let artifact_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&pool)
            .await
            .expect("Artifact count");
        assert_eq!(artifact_count, 1);
    }

    #[tokio::test]
    async fn promotion_is_owner_scoped_and_missing_derivative_fails_closed() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let artifacts = Arc::new(LocalArtifactStore::sqlite(
            pool.clone(),
            data_dir.path().join("artifacts"),
        ));
        let store = MediaDerivativeStore::sqlite(pool.clone(), Arc::clone(&artifacts));
        let owner = Principal::new("owner");
        let other = Principal::new("other");
        let source = store
            .create_source(
                &owner,
                "image/png",
                Bytes::from_static(b"source"),
                Duration::from_secs(60),
            )
            .await
            .expect("source snapshot");
        let media = store
            .get_or_create_derivative(
                &owner,
                &source.id,
                Bytes::from_static(b"derivative"),
                Duration::from_secs(60),
            )
            .await
            .expect("derivative");

        assert!(matches!(
            store
                .get_or_create_derivative(
                    &other,
                    &source.id,
                    Bytes::from_static(b"foreign"),
                    Duration::from_secs(60),
                )
                .await,
            Err(MediaStoreError::Unavailable)
        ));

        sqlx::query("UPDATE artifacts SET expires_at = 0 WHERE principal = ?")
            .bind(owner.continuation_key())
            .execute(&pool)
            .await
            .expect("expire media");
        assert!(matches!(
            store
                .promote(
                    &owner,
                    &[source.id.clone(), ArtifactId::new("missing")],
                    Duration::from_secs(7 * 24 * 60 * 60),
                )
                .await,
            Err(MediaStoreError::Unavailable)
        ));
        let source_expiry: i64 =
            sqlx::query_scalar("SELECT expires_at FROM artifacts WHERE id = ?")
                .bind(source.id.as_str())
                .fetch_one(&pool)
                .await
                .expect("source expiry after failed promotion");
        assert_eq!(source_expiry, 0);
        store
            .promote(
                &owner,
                &[source.id.clone(), media.derivative.id.clone()],
                Duration::from_secs(7 * 24 * 60 * 60),
            )
            .await
            .expect("promote media");
        artifacts
            .open(&owner, &source.id)
            .await
            .expect("promoted source");
        let reader = artifacts
            .open(&owner, &media.derivative.id)
            .await
            .expect("promoted derivative");
        let ArtifactSource::LocalPath(path) = reader.source else {
            panic!("expected local derivative");
        };
        tokio::fs::remove_file(path)
            .await
            .expect("remove derivative bytes");

        assert!(matches!(
            store
                .get_or_create_derivative(
                    &owner,
                    &source.id,
                    Bytes::from_static(b"replacement"),
                    Duration::from_secs(60),
                )
                .await,
            Err(MediaStoreError::Corrupt)
        ));
    }
}

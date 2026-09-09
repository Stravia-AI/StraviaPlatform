use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::agent::LocalArtifactStore;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::artifact::ArtifactId;

use stravia_media::store::*;

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::artifact::ArtifactSource;
    use stravia_runtime_contract::artifact::ArtifactStore;

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
        let store = MediaDerivativeStore::sqlite(
            pool.clone(),
            Arc::new(super::super::ArtifactHost(Arc::clone(&artifacts))),
        );
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
        let reconstructed = MediaDerivativeStore::sqlite(
            pool,
            Arc::new(super::super::ArtifactHost(Arc::clone(
                &reconstructed_artifacts,
            ))),
        );
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
        let store = MediaDerivativeStore::sqlite(
            pool.clone(),
            Arc::new(super::super::ArtifactHost(Arc::clone(&artifacts))),
        );
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
        let store = MediaDerivativeStore::sqlite(
            pool.clone(),
            Arc::new(super::super::ArtifactHost(Arc::clone(&artifacts))),
        );
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

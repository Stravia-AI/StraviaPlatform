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
    async fn failed_mapping_insert_leaves_the_candidate_to_retention_cleanup() {
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
        let bytes = jpeg(1);
        let candidate = store
            .create_source(&owner, "image/jpeg", bytes.clone(), Duration::from_secs(60))
            .await
            .expect("existing shared content");
        sqlx::query(
            "CREATE TRIGGER reject_media_derivative_insert BEFORE INSERT ON media_derivatives BEGIN SELECT RAISE(ABORT, 'rejected'); END",
        )
        .execute(&pool)
        .await
        .expect("failure trigger");

        assert!(matches!(
            store
                .get_or_create_derivative(
                    &owner,
                    &source.id,
                    bytes.clone(),
                    Duration::from_secs(60)
                )
                .await,
            Err(MediaStoreError::Storage(_))
        ));
        let (_, surviving_bytes) = store
            .read_artifact_bounded(&owner, &candidate.id, bytes.len() as u64)
            .await
            .expect("shared content survives failed mapping");
        assert_eq!(surviving_bytes, bytes);
        assert!(
            store
                .find_derivative(&owner, &source.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_same_source_mapping_never_deletes_the_shared_winner() {
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
                Duration::from_secs(2 * 60 * 60),
            )
            .await
            .expect("source snapshot");
        let bytes = jpeg(9);
        let first = store.get_or_create_derivative(
            &owner,
            &source.id,
            bytes.clone(),
            Duration::from_secs(60),
        );
        let second = store.get_or_create_derivative(
            &owner,
            &source.id,
            bytes.clone(),
            Duration::from_secs(60),
        );
        let (first, second) = tokio::join!(first, second);
        let first = first.expect("first derivative");
        let second = second.expect("second derivative");
        assert_eq!(first.derivative.id, second.derivative.id);
        // Both racers created the same content-addressed Artifact; the loser
        // must not have deleted the winner's derivative bytes.
        let reader = artifacts
            .open(&owner, &first.derivative.id)
            .await
            .expect("winner derivative survives");
        let ArtifactSource::LocalPath(path) = reader.source else {
            panic!("expected local derivative");
        };
        assert_eq!(
            tokio::fs::read(path).await.expect("derivative bytes"),
            bytes
        );
    }

    #[tokio::test]
    async fn distinct_sources_share_a_derivative_and_a_source_may_be_its_own_derivative() {
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
            Arc::new(super::super::ArtifactHost(artifacts)),
        );
        let owner = Principal::new("owner");
        let first = store
            .create_source(
                &owner,
                "image/png",
                Bytes::from_static(b"first-original"),
                Duration::from_secs(60),
            )
            .await
            .expect("first source");
        let second = store
            .create_source(
                &owner,
                "image/png",
                Bytes::from_static(b"second-original"),
                Duration::from_secs(60),
            )
            .await
            .expect("second source");
        let shared = jpeg(4);
        let first_media = store
            .get_or_create_derivative(&owner, &first.id, shared.clone(), Duration::from_secs(60))
            .await
            .expect("first mapping");
        let second_media = store
            .get_or_create_derivative(&owner, &second.id, shared.clone(), Duration::from_secs(60))
            .await
            .expect("second mapping");
        assert_eq!(first_media.derivative.id, second_media.derivative.id);
        for source_id in [&first.id, &second.id] {
            assert_eq!(
                store
                    .find_derivative(&owner, source_id)
                    .await
                    .expect("source lookup")
                    .expect("source mapping")
                    .derivative
                    .id,
                first_media.derivative.id
            );
        }

        // A JPEG whose normalized form is byte-identical maps onto itself.
        let self_source = store
            .create_source(
                &owner,
                "image/jpeg",
                shared.clone(),
                Duration::from_secs(60),
            )
            .await
            .expect("self source");
        let self_media = store
            .get_or_create_derivative(&owner, &self_source.id, shared, Duration::from_secs(60))
            .await
            .expect("self mapping");
        assert_eq!(self_media.derivative.id, self_source.id);
        assert_eq!(
            store
                .find_derivative(&owner, &self_source.id)
                .await
                .expect("self lookup")
                .expect("self mapping row")
                .derivative
                .id,
            self_source.id
        );
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

        let original_expiry: i64 =
            sqlx::query_scalar("SELECT expires_at FROM artifacts WHERE id = ?")
                .bind(source.id.as_str())
                .fetch_one(&pool)
                .await
                .expect("live source expiry");
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
        assert_eq!(source_expiry, original_expiry);
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

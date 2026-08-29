use super::*;

fn policy() -> ArtifactPolicy {
    ArtifactPolicy {
        max_artifacts: 2,
        max_bytes: 1024,
        allowed_mime_types: vec!["image/png".into()],
    }
}

#[test]
fn staging_quota_bounds_active_uploads_and_declared_bytes() {
    assert!(validate_staging_quota(15, 0, 1).is_ok());
    assert!(matches!(
        validate_staging_quota(16, 0, 1),
        Err(ArtifactError::Invalid(message)) if message == "Artifact staging quota exceeded"
    ));
    assert!(matches!(
        validate_staging_quota(
            1,
            MAX_PRINCIPAL_STAGING_BYTES as i64,
            1,
        ),
        Err(ArtifactError::Invalid(message)) if message == "Artifact staging quota exceeded"
    ));
}

#[tokio::test]
async fn multipart_upload_is_principal_scoped_and_survives_store_reconstruction() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let root = data_dir.path().join("artifacts");
    let store = LocalArtifactStore::sqlite(pool.clone(), &root);
    let owner = Principal::new("owner");
    let other = Principal::new("other");
    let upload = store
        .create_upload(
            &owner,
            ArtifactUploadRequest {
                mime_type: "image/png".into(),
                size: 6,
                idle_ttl: Duration::from_secs(60),
                retention_ttl: Duration::from_secs(7 * 24 * 60 * 60),
                policy: policy(),
            },
        )
        .await
        .expect("create upload");
    assert!(matches!(
        store
            .upload_part(
                &other,
                &upload.upload_id,
                &upload.upload_token,
                1,
                bytes_stream(Bytes::from_static(b"abc")),
            )
            .await,
        Err(ArtifactError::Forbidden)
    ));
    let _first = store
        .upload_part(
            &owner,
            &upload.upload_id,
            &upload.upload_token,
            1,
            bytes_stream(Bytes::from_static(b"abc")),
        )
        .await
        .expect("first part");
    let first = store
        .upload_part(
            &owner,
            &upload.upload_id,
            &upload.upload_token,
            1,
            bytes_stream(Bytes::from_static(b"ABC")),
        )
        .await
        .expect("replace first part");
    let second = store
        .upload_part(
            &owner,
            &upload.upload_id,
            &upload.upload_token,
            2,
            bytes_stream(Bytes::from_static(b"def")),
        )
        .await
        .expect("second part");
    let artifact = store
        .complete_upload(
            &owner,
            &upload.upload_id,
            &upload.upload_token,
            &[first, second],
        )
        .await
        .expect("complete upload");

    let reconstructed = LocalArtifactStore::sqlite(pool, root);
    let reader = reconstructed
        .open(&owner, &artifact.id)
        .await
        .expect("open Artifact");
    let ArtifactSource::LocalPath(path) = reader.source else {
        panic!("expected local Artifact");
    };
    assert_eq!(
        tokio::fs::read(path).await.expect("read Artifact"),
        b"ABCdef"
    );
    assert!(matches!(
        reconstructed.open(&other, &artifact.id).await,
        Err(ArtifactError::NotFound)
    ));
}
#[tokio::test]
async fn failed_ready_artifact_file_cleanup_keeps_its_database_record() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = LocalArtifactStore::sqlite(pool.clone(), data_dir.path().join("artifacts"));
    let owner = Principal::new("owner");
    let artifact = store
        .create_ready_bytes(
            &owner,
            "image/jpeg",
            Bytes::from_static(b"jpeg"),
            Duration::from_secs(60),
        )
        .await
        .expect("ready Artifact");
    let object_path = store.object_path(artifact.id.as_str());
    tokio::fs::remove_file(&object_path)
        .await
        .expect("remove object fixture");
    tokio::fs::create_dir(&object_path)
        .await
        .expect("replace object with directory");

    assert!(matches!(
        store.delete_ready(&owner, &artifact.id).await,
        Err(ArtifactError::Storage(_))
    ));
    let record_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE id = ?")
        .bind(artifact.id.as_str())
        .fetch_one(&pool)
        .await
        .expect("Artifact row count");
    assert_eq!(record_count, 1);
    sqlx::query("UPDATE artifacts SET expires_at = 0 WHERE id = ?")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .expect("expire Artifact");
    assert!(matches!(
        store.sweep_expired().await,
        Err(ArtifactError::Storage(_))
    ));
    let record_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE id = ?")
        .bind(artifact.id.as_str())
        .fetch_one(&pool)
        .await
        .expect("Artifact row count after failed sweep");
    assert_eq!(record_count, 1);
}

#[tokio::test]
async fn retention_extension_prevents_ready_artifact_sweep() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = LocalArtifactStore::sqlite(pool.clone(), data_dir.path().join("artifacts"));
    let owner = Principal::new("owner");
    let artifact = store
        .create_ready_bytes(
            &owner,
            "image/jpeg",
            Bytes::from_static(b"jpeg"),
            Duration::from_secs(60),
        )
        .await
        .expect("ready Artifact");
    sqlx::query("UPDATE artifacts SET expires_at = 0 WHERE id = ?")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .expect("expire Artifact");

    store
        .extend_retention(
            &owner,
            std::slice::from_ref(&artifact.id),
            Duration::from_secs(60 * 60),
        )
        .await
        .expect("extend retention");

    assert_eq!(store.sweep_expired().await.expect("sweep"), 0);
    store
        .open(&owner, &artifact.id)
        .await
        .expect("retained Artifact remains available");
}

#[tokio::test]
async fn sweep_expired_upload_removes_staging_before_upload_metadata() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let root = data_dir.path().join("artifacts");
    let store = LocalArtifactStore::sqlite(pool.clone(), &root);
    let owner = Principal::new("sweep-owner");
    let upload = store
        .create_upload(
            &owner,
            ArtifactUploadRequest {
                mime_type: "image/png".into(),
                size: 3,
                idle_ttl: Duration::from_secs(60),
                retention_ttl: Duration::from_secs(60 * 60),
                policy: policy(),
            },
        )
        .await
        .expect("create upload");
    let staging = store.staging_dir(&upload.upload_id);
    tokio::fs::write(staging.join("partial.tmp"), b"abc")
        .await
        .expect("write partial upload");
    sqlx::query("UPDATE artifact_uploads SET expires_at = 0 WHERE id = ?")
        .bind(&upload.upload_id)
        .execute(&pool)
        .await
        .expect("expire upload");

    assert_eq!(store.sweep_expired().await.expect("sweep upload"), 0);
    assert!(matches!(
        tokio::fs::metadata(&staging).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ));
    let (upload_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifact_uploads")
        .fetch_one(&pool)
        .await
        .expect("count uploads");
    let (artifact_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifacts")
        .fetch_one(&pool)
        .await
        .expect("count artifacts");
    assert_eq!(upload_count, 0);
    assert_eq!(artifact_count, 1);

    sqlx::query("UPDATE artifacts SET expires_at = 0 WHERE id = ?")
        .bind(upload.artifact_id.as_str())
        .execute(&pool)
        .await
        .expect("expire staging artifact");
    assert_eq!(store.sweep_expired().await.expect("sweep artifact"), 1);
    let (artifact_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifacts")
        .fetch_one(&pool)
        .await
        .expect("count artifacts after sweep");
    assert_eq!(artifact_count, 0);
}

#[tokio::test]
async fn concurrent_upload_creation_enforces_principal_staging_quota() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = LocalArtifactStore::sqlite(pool, data_dir.path().join("artifacts"));
    let owner = Principal::new("quota-owner");
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut tasks = Vec::with_capacity(17);
    for _ in 0..17 {
        let store = store.clone();
        let owner = owner.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .create_upload(
                    &owner,
                    ArtifactUploadRequest {
                        mime_type: "image/png".into(),
                        size: 1,
                        idle_ttl: Duration::from_secs(60),
                        retention_ttl: Duration::from_secs(60 * 60),
                        policy: policy(),
                    },
                )
                .await
        }));
    }

    let results = futures::future::join_all(tasks).await;
    let successes = results
        .iter()
        .filter(|result| result.as_ref().expect("upload task").is_ok())
        .count();
    assert_eq!(successes, MAX_PRINCIPAL_STAGING_UPLOADS as usize);
    assert!(results.iter().any(|result| {
        matches!(
            result.as_ref().expect("upload task"),
            Err(ArtifactError::Invalid(message))
                if message == "Artifact staging quota exceeded"
        )
    }));
}

#[tokio::test]
async fn concurrent_parts_cannot_exceed_the_declared_upload_size() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = LocalArtifactStore::sqlite(pool, data_dir.path().join("artifacts"));
    let owner = Principal::new("concurrent-owner");
    let upload = store
        .create_upload(
            &owner,
            ArtifactUploadRequest {
                mime_type: "image/png".into(),
                size: 5,
                idle_ttl: Duration::from_secs(60),
                retention_ttl: Duration::from_secs(60 * 60),
                policy: policy(),
            },
        )
        .await
        .expect("create upload");

    let first = store.upload_part(
        &owner,
        &upload.upload_id,
        &upload.upload_token,
        1,
        bytes_stream(Bytes::from_static(b"abc")),
    );
    let second = store.upload_part(
        &owner,
        &upload.upload_id,
        &upload.upload_token,
        2,
        bytes_stream(Bytes::from_static(b"def")),
    );
    let (first, second) = tokio::join!(first, second);
    assert_ne!(first.is_ok(), second.is_ok());
    let rejection = first.err().or_else(|| second.err()).expect("one rejection");
    assert!(matches!(
        rejection,
        ArtifactError::Invalid(message)
            if message == "Artifact part exceeds the declared upload size"
    ));
}

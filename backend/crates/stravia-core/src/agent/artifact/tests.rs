use super::*;

#[test]
fn artifact_reference_identity_ignores_question_without_accepting_foreign_urls() {
    let id = ArtifactId::from_reference("https://stravia/artifact/artifact_abc?question=what%3F")
        .unwrap();
    assert_eq!(id.as_str(), "artifact_abc");
    for reference in [
        "https://stravia.example/artifact/artifact_abc",
        "http://stravia/artifact/artifact_abc",
        "https://stravia@evil.example/artifact/artifact_abc",
        "https://stravia/artifact/../secret",
        "https://stravia/artifact/artifact_abc#fragment",
    ] {
        assert!(ArtifactId::from_reference(reference).is_err());
    }
}

#[tokio::test]
async fn download_grants_and_readers_survive_expiry_and_store_reconstruction() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let root = directory.path().join("artifacts");
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1_800_000_000_000));
    let controlled = time.clone();
    let clock: Arc<dyn Fn() -> i64 + Send + Sync> =
        Arc::new(move || controlled.load(std::sync::atomic::Ordering::SeqCst));
    let store = LocalArtifactStore::sqlite(pool.clone(), &root).with_clock(clock.clone());
    let principal = Principal::new("download-owner");
    let artifact = store
        .ingest(
            &principal,
            "application/octet-stream",
            None,
            bytes_stream(Bytes::from_static(b"complete content")),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let settings = ArtifactSettings {
        client_base_url: "https://client.example/deployment".into(),
        ..Default::default()
    };
    let grant = store
        .download(&principal, &artifact.id, Duration::from_secs(60), &settings)
        .await
        .unwrap();
    assert!(
        grant
            .url
            .starts_with("https://client.example/deployment/v1/artifacts/downloads/")
    );
    let token = grant.url.rsplit('/').next().unwrap();
    let stored_hash: String =
        sqlx::query_scalar("SELECT token_hash FROM artifact_download_grants WHERE artifact_id=?")
            .bind(artifact.id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(stored_hash, token);
    time.fetch_add(30_000, std::sync::atomic::Ordering::SeqCst);
    ArtifactStore::extend_retention(&store, &principal, &artifact.id, Duration::from_secs(60))
        .await
        .unwrap();
    time.fetch_add(30_000, std::sync::atomic::Ordering::SeqCst);
    drop(store.open(&principal, &artifact.id).await.unwrap());
    time.fetch_add(30_000, std::sync::atomic::Ordering::SeqCst);
    let reconstructed = LocalArtifactStore::sqlite(pool.clone(), &root).with_clock(clock);
    assert!(matches!(
        ArtifactStore::extend_retention(
            &reconstructed,
            &principal,
            &artifact.id,
            Duration::from_secs(60)
        )
        .await,
        Err(ArtifactError::NotFound)
    ));
    assert!(matches!(
        reconstructed
            .download(&principal, &artifact.id, Duration::from_secs(60), &settings)
            .await,
        Err(ArtifactError::NotFound)
    ));
    assert_eq!(reconstructed.sweep_expired().await.unwrap(), 0);
    let reader = reconstructed.read_download(token).await.unwrap();
    time.store(grant.expires_at, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        store.read_download(token).await,
        Err(ArtifactError::Unauthorized)
    ));
    assert_eq!(store.sweep_expired().await.unwrap(), 0);
    let ArtifactSource::LocalPath(path) = &reader.source else {
        panic!("local source");
    };
    assert_eq!(tokio::fs::read(path).await.unwrap(), b"complete content");
    drop(reader);
    assert_eq!(store.sweep_expired().await.unwrap(), 1);
    assert!(matches!(
        store.open(&principal, &artifact.id).await,
        Err(ArtifactError::NotFound)
    ));
}

#[tokio::test]
async fn common_ingestion_reserves_staging_and_releases_completed_slots() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let store = LocalArtifactStore::sqlite(pool, directory.path().join("artifacts"));
    let principal = Principal::new("ingestion-owner");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let stream = Box::pin(futures::stream::once(async move {
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
        Ok(Bytes::from_static(b"x"))
    }));
    let running_store = store.clone();
    let running_principal = principal.clone();
    let ingestion = tokio::spawn(async move {
        running_store
            .ingest(
                &running_principal,
                "application/octet-stream",
                None,
                stream,
                Duration::from_secs(60),
            )
            .await
    });
    entered_rx.await.unwrap();
    for _ in 0..3 {
        store
            .create_upload(
                &principal,
                ArtifactUploadRequest {
                    mime_type: "application/octet-stream".into(),
                    size: MAX_ARTIFACT_BYTES,
                    idle_ttl: Duration::from_secs(60),
                    retention_ttl: Duration::from_secs(60),
                    policy: ArtifactPolicy {
                        max_artifacts: 1,
                        max_bytes: MAX_ARTIFACT_BYTES,
                        allowed_mime_types: vec!["*/*".into()],
                    },
                },
            )
            .await
            .unwrap();
    }
    assert!(matches!(
        store
            .ingest(
                &principal,
                "application/octet-stream",
                Some(1),
                bytes_stream(Bytes::from_static(b"x")),
                Duration::from_secs(60)
            )
            .await,
        Err(ArtifactError::Invalid(_))
    ));
    release_tx.send(()).unwrap();
    let artifact = ingestion.await.unwrap().unwrap();
    let (_, bytes) = store
        .read_bytes(&principal, &artifact.id, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(bytes, Bytes::from_static(b"x"));
    store
        .ingest(
            &principal,
            "application/octet-stream",
            Some(1),
            bytes_stream(Bytes::from_static(b"y")),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .ingest(
                &principal,
                "application/octet-stream",
                Some(1),
                bytes_stream(Bytes::from_static(b"too long")),
                Duration::from_secs(60)
            )
            .await,
        Err(ArtifactError::Invalid(_))
    ));
}

#[tokio::test]
async fn cancelled_s3_read_removes_partial_cache_without_blocking_configuration() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started_tx)));
    let router = axum::Router::new().route(
        "/{bucket}/objects/{id}",
        axum::routing::put(|_body: Bytes| async { axum::http::StatusCode::OK }).get(move || {
            let started = started.clone();
            async move {
                let stream = futures::stream::once(async move {
                    if let Some(sender) = started.lock().await.take() {
                        let _ = sender.send(());
                    }
                    Ok::<_, std::io::Error>(Bytes::from_static(b"a"))
                })
                .chain(futures::stream::pending());
                axum::response::Response::builder()
                    .header(axum::http::header::CONTENT_LENGTH, "3")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let root = directory.path().join("artifacts");
    let store = LocalArtifactStore::sqlite(pool.clone(), &root);
    let settings = ArtifactSettings {
        s3: Some(ArtifactS3Settings {
            endpoint,
            region: "us-east-1".into(),
            bucket: "test".into(),
            access_key_id: "test".into(),
            secret_access_key: "test".into(),
            session_token: None,
            credentials_expires_at: None,
        }),
        ..Default::default()
    };
    // A store constructed before the persisted update must use the shared S3 config.
    sqlx::query("INSERT INTO settings(name,value) VALUES('artifact_settings',?)")
        .bind(serde_json::to_string(&settings).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let owner = Principal::new("cancel-reader");
    let artifact = store
        .ingest(
            &owner,
            "application/octet-stream",
            Some(3),
            bytes_stream(Bytes::from_static(b"abc")),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let reader_store = store.clone();
    let reading = tokio::spawn(async move { reader_store.open(&owner, &artifact.id).await });
    started_rx.await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        store.configure(&ArtifactSettings::default()),
    )
    .await
    .unwrap()
    .unwrap();
    reading.abort();
    assert!(reading.await.unwrap_err().is_cancelled());
    let mut files = tokio::fs::read_dir(root.join("objects")).await.unwrap();
    while let Some(file) = files.next_entry().await.unwrap() {
        assert!(
            !file.file_name().to_string_lossy().contains(".read-"),
            "cancelled read left a partial cache"
        );
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
#[ignore = "requires an explicitly configured isolated PostgreSQL DB_URL"]
async fn postgres_download_lifecycle_survives_reconstruction() {
    let url = std::env::var("DB_URL").expect("isolated PostgreSQL DB_URL");
    let admin = PgPool::connect(&url).await.unwrap();
    let schema = format!("artifact_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let search_path = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |connection, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(search_path)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    crate::migrations::migrate_postgres(&pool).await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = LocalArtifactStore::postgres(pool.clone(), root.path());
    let owner = Principal::new("pg-owner");
    let artifact = store
        .ingest(
            &owner,
            "application/octet-stream",
            Some(3),
            bytes_stream(Bytes::from_static(b"abc")),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let settings = ArtifactSettings {
        client_base_url: "https://client.example/base".into(),
        ..Default::default()
    };
    let grant = store
        .download(&owner, &artifact.id, Duration::from_secs(60), &settings)
        .await
        .unwrap();
    let token = grant.url.rsplit('/').next().unwrap();
    let replica = LocalArtifactStore::postgres(pool.clone(), root.path());
    let updated = ArtifactSettings {
        client_base_url: "https://updated.example/prefix".into(),
        ..Default::default()
    };
    sqlx::query("INSERT INTO settings(name,value) VALUES('artifact_settings',$1)")
        .bind(serde_json::to_string(&updated).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let updated_grant = replica
        .download(&owner, &artifact.id, Duration::from_secs(60), &settings)
        .await
        .unwrap();
    assert!(
        updated_grant
            .url
            .starts_with("https://updated.example/prefix/v1/artifacts/downloads/")
    );
    sqlx::query("UPDATE artifacts SET expires_at=0 WHERE id=$1")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    let rebuilt = LocalArtifactStore::postgres(pool.clone(), root.path());
    assert!(matches!(
        rebuilt.open(&owner, &artifact.id).await,
        Err(ArtifactError::NotFound)
    ));
    assert!(matches!(
        ArtifactStore::extend_retention(&rebuilt, &owner, &artifact.id, Duration::from_secs(60))
            .await,
        Err(ArtifactError::NotFound)
    ));
    assert_eq!(rebuilt.sweep_expired().await.unwrap(), 0);
    let reader = rebuilt.read_download(token).await.unwrap();
    sqlx::query("UPDATE artifact_download_grants SET expires_at=0 WHERE artifact_id=$1")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store.sweep_expired().await.unwrap(), 0);
    let ArtifactSource::LocalPath(path) = &reader.source else {
        panic!("local read");
    };
    assert_eq!(tokio::fs::read(path).await.unwrap(), b"abc");
    drop(reader);
    // The exclusive database lock is a deterministic barrier for reader rollback.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,1))")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(rebuilt.sweep_expired().await.unwrap(), 1);
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires an explicitly configured isolated S3-compatible bucket"]
async fn s3_multipart_native_download_and_platform_relay_preserve_bytes() {
    let endpoint =
        std::env::var("STRAVIA_ARTIFACT_TEST_S3_ENDPOINT").expect("isolated S3 endpoint");
    let s3 = ArtifactS3Settings {
        endpoint: endpoint.clone(),
        region: "us-east-1".into(),
        bucket: std::env::var("STRAVIA_ARTIFACT_TEST_S3_BUCKET")
            .unwrap_or_else(|_| "artifacts".into()),
        access_key_id: std::env::var("STRAVIA_ARTIFACT_TEST_S3_ACCESS_KEY").unwrap(),
        secret_access_key: std::env::var("STRAVIA_ARTIFACT_TEST_S3_SECRET_KEY").unwrap(),
        session_token: None,
        credentials_expires_at: None,
    };
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let store = LocalArtifactStore::sqlite(pool.clone(), directory.path().join("artifacts"));
    let rebuilt = LocalArtifactStore::sqlite(pool.clone(), directory.path().join("artifacts"));
    let mut settings = ArtifactSettings {
        client_base_url: "https://client.example/prefix".into(),
        s3: Some(s3),
        ..Default::default()
    };
    sqlx::query("INSERT INTO settings(name,value) VALUES('artifact_settings',?)")
        .bind(serde_json::to_string(&settings).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let owner = Principal::new("s3-owner");
    let artifact = rebuilt
        .ingest(
            &owner,
            "application/octet-stream",
            None,
            bytes_stream(Bytes::from_static(b"s3 complete bytes")),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let relay = store
        .download(&owner, &artifact.id, Duration::from_secs(60), &settings)
        .await
        .unwrap();
    assert!(
        relay
            .url
            .starts_with("https://client.example/prefix/v1/artifacts/downloads/")
    );
    let reader = rebuilt
        .read_download(relay.url.rsplit('/').next().unwrap())
        .await
        .unwrap();
    let ArtifactSource::LocalPath(path) = &reader.source else {
        panic!("platform relay source");
    };
    assert_eq!(tokio::fs::read(path).await.unwrap(), b"s3 complete bytes");
    drop(reader);
    settings.external_signed_downloads = true;
    settings.file_public_base_url = Some(endpoint.clone());
    sqlx::query("UPDATE settings SET value=? WHERE name='artifact_settings'")
        .bind(serde_json::to_string(&settings).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let native = rebuilt
        .download(&owner, &artifact.id, Duration::from_secs(60), &settings)
        .await
        .unwrap();
    assert!(native.url.starts_with(&endpoint));
    let response = reqwest::get(&native.url).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        response.bytes().await.unwrap(),
        Bytes::from_static(b"s3 complete bytes")
    );
    settings.s3.as_mut().unwrap().credentials_expires_at =
        Some(chrono::Utc::now().timestamp_millis() + 299_000);
    sqlx::query("UPDATE settings SET value=? WHERE name='artifact_settings'")
        .bind(serde_json::to_string(&settings).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        rebuilt
            .download(&owner, &artifact.id, Duration::from_secs(60), &settings)
            .await,
        Err(ArtifactError::Invalid(_))
    ));
    sqlx::query("UPDATE artifacts SET expires_at=0 WHERE id=?")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(rebuilt.sweep_expired().await.unwrap(), 0);
    sqlx::query("UPDATE artifact_download_grants SET expires_at=0 WHERE artifact_id=?")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(rebuilt.sweep_expired().await.unwrap(), 1);
    assert_eq!(
        reqwest::get(&native.url).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
}

fn policy() -> ArtifactPolicy {
    ArtifactPolicy {
        max_artifacts: 2,
        max_bytes: 1024,
        allowed_mime_types: vec!["image/png".into()],
    }
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
        .ingest(
            &owner,
            "image/jpeg",
            Some(4),
            bytes_stream(Bytes::from_static(b"jpeg")),
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
async fn batch_retention_extension_cannot_revive_expired_artifacts() {
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
        .ingest(
            &owner,
            "image/jpeg",
            Some(4),
            bytes_stream(Bytes::from_static(b"jpeg")),
            Duration::from_secs(60),
        )
        .await
        .expect("ready Artifact");
    sqlx::query("UPDATE artifacts SET expires_at = 0 WHERE id = ?")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .expect("expire Artifact");

    assert!(matches!(
        store
            .extend_retention(
                &owner,
                std::slice::from_ref(&artifact.id),
                Duration::from_secs(60 * 60)
            )
            .await,
        Err(ArtifactError::NotFound)
    ));
    assert_eq!(store.sweep_expired().await.expect("sweep"), 1);
    assert!(matches!(
        store.open(&owner, &artifact.id).await,
        Err(ArtifactError::NotFound)
    ));
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
    let root = data_dir.path().join("artifacts");
    let owner = Principal::new("quota-owner");
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut tasks = Vec::with_capacity(17);
    for _ in 0..17 {
        let store = LocalArtifactStore::sqlite(pool.clone(), &root);
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

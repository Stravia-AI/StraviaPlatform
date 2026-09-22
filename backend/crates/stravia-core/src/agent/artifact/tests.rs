use super::*;

#[test]
fn artifact_reference_identity_ignores_question_and_rejects_non_digest_identities() {
    let digest = "a".repeat(stravia_runtime_contract::identifier::DIGEST_ID_LEN);
    let id = ArtifactId::from_reference(&format!("stravia://artifacts/{digest}?question=what%3F"))
        .unwrap();
    assert_eq!(id.as_str(), digest);
    for reference in [
        format!("https://stravia/artifact/{digest}"),
        format!("sa:{digest}"),
        format!("stravia://artifacts/{}", "a".repeat(54)),
        format!("stravia://artifacts/{}", "a".repeat(56)),
        format!("stravia://artifacts/{}", "A".repeat(55)),
        format!("stravia://artifacts/{}", "0".repeat(55)),
        format!("stravia://artifacts/{digest}#fragment"),
    ] {
        assert!(ArtifactId::from_reference(&reference).is_err());
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
async fn empty_text_artifacts_remain_readable_and_owner_scoped() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let store = LocalArtifactStore::sqlite(pool, directory.path().join("artifacts"));
    let principal = Principal::new("empty-text-owner");
    let artifact = store
        .ingest(
            &principal,
            "text/plain",
            Some(0),
            bytes_stream(Bytes::new()),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let (_, bytes) = store
        .read_bytes(&principal, &artifact.id, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), b"");
    assert!(
        store
            .read_bytes(
                &Principal::new("another-owner"),
                &artifact.id,
                Duration::from_secs(60)
            )
            .await
            .is_err()
    );
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
async fn delayed_s3_sweep_allows_unrelated_sqlite_writes_and_safe_reuse() {
    let (delete_started_tx, delete_started_rx) = tokio::sync::oneshot::channel();
    let delete_started = Arc::new(Mutex::new(Some(delete_started_tx)));
    let (release_delete_tx, release_delete_rx) = tokio::sync::oneshot::channel();
    let release_delete = Arc::new(Mutex::new(Some(release_delete_rx)));
    let object_present = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let put_object_present = Arc::clone(&object_present);
    let get_object_present = Arc::clone(&object_present);
    let delete_object_present = Arc::clone(&object_present);
    let router = axum::Router::new().route(
        "/{bucket}/objects/{id}",
        axum::routing::put(move |_body: Bytes| {
            let object_present = Arc::clone(&put_object_present);
            async move {
                object_present.store(true, std::sync::atomic::Ordering::SeqCst);
                axum::http::StatusCode::OK
            }
        })
        .get(move || {
            let object_present = Arc::clone(&get_object_present);
            async move {
                let present = object_present.load(std::sync::atomic::Ordering::SeqCst);
                axum::response::Response::builder()
                    .status(if present {
                        axum::http::StatusCode::OK
                    } else {
                        axum::http::StatusCode::NOT_FOUND
                    })
                    .body(axum::body::Body::from(if present {
                        Bytes::from_static(b"reusable")
                    } else {
                        Bytes::new()
                    }))
                    .unwrap()
            }
        })
        .delete(move || {
            let delete_started = Arc::clone(&delete_started);
            let release_delete = Arc::clone(&release_delete);
            let object_present = Arc::clone(&delete_object_present);
            async move {
                if let Some(sender) = delete_started.lock().await.take() {
                    let _ = sender.send(());
                }
                let receiver = release_delete.lock().await.take();
                if let Some(receiver) = receiver {
                    let _ = receiver.await;
                }
                object_present.store(false, std::sync::atomic::Ordering::SeqCst);
                axum::http::StatusCode::NO_CONTENT
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
    sqlx::query("INSERT INTO settings(name,value) VALUES('artifact_settings',?)")
        .bind(serde_json::to_string(&settings).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let owner = Principal::new("reuse-owner");
    let payload = Bytes::from_static(b"reusable");
    let artifact = store
        .ingest(
            &owner,
            "application/octet-stream",
            Some(payload.len() as u64),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let backend_key: String = sqlx::query_scalar("SELECT backend_key FROM artifacts WHERE id=?")
        .bind(artifact.id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
    let object_path = root.join(backend_key);
    sqlx::query("UPDATE artifacts SET expires_at=0 WHERE id=?")
        .bind(artifact.id.as_str())
        .execute(&pool)
        .await
        .unwrap();

    let sweeping_store = store.clone();
    let sweeping = tokio::spawn(async move { sweeping_store.sweep_expired().await });
    delete_started_rx.await.unwrap();

    tokio::time::timeout(
        Duration::from_secs(1),
        store.create_upload(
            &Principal::new("unrelated-owner"),
            ArtifactUploadRequest {
                mime_type: "application/octet-stream".into(),
                size: 1,
                idle_ttl: Duration::from_secs(60),
                retention_ttl: Duration::from_secs(60),
                policy: ArtifactPolicy {
                    max_artifacts: 1,
                    max_bytes: MAX_ARTIFACT_BYTES,
                    allowed_mime_types: vec!["*/*".into()],
                },
            },
        ),
    )
    .await
    .expect("delayed object deletion must not hold the SQLite writer lock")
    .expect("unrelated upload metadata write");

    let reuse_store = store.clone();
    let reuse_owner = owner.clone();
    let reuse_payload = payload.clone();
    let reuse = tokio::spawn(async move {
        reuse_store
            .ingest(
                &reuse_owner,
                "application/octet-stream",
                Some(reuse_payload.len() as u64),
                bytes_stream(reuse_payload),
                Duration::from_secs(60),
            )
            .await
    });
    let reuse_upload_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let upload_id: Option<String> =
                sqlx::query_scalar("SELECT id FROM artifact_uploads WHERE principal=?")
                    .bind(owner.continuation_key())
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            if let Some(upload_id) = upload_id {
                break upload_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reuse upload metadata");
    tokio::time::timeout(Duration::from_secs(5), async {
        let assembled = root
            .join("staging")
            .join(reuse_upload_id)
            .join("assembled.tmp");
        loop {
            match tokio::fs::metadata(&assembled).await {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("inspect assembled reuse fixture: {error}"),
            }
        }
    })
    .await
    .expect("reuse reaches the artifact deletion lock");
    assert!(
        !reuse.is_finished(),
        "reuse must wait for the artifact deletion lock"
    );

    let mut blocker = pool.acquire().await.unwrap();
    let mut blocker = blocker.begin_with("BEGIN IMMEDIATE").await.unwrap();
    release_delete_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match tokio::fs::metadata(&object_path).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Ok(_) => tokio::task::yield_now().await,
                Err(error) => panic!("inspect deleted object fixture: {error}"),
            }
        }
    })
    .await
    .expect("object deletion before blocked metadata delete");
    sweeping.abort();
    assert!(sweeping.await.unwrap_err().is_cancelled());
    let record_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE id=?")
        .bind(artifact.id.as_str())
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    assert_eq!(record_count, 1, "cancelled metadata delete keeps its row");
    blocker.rollback().await.unwrap();

    let reused = tokio::time::timeout(Duration::from_secs(5), reuse)
        .await
        .expect("reuse completion")
        .expect("reuse task")
        .expect("reuse result");
    assert_eq!(reused.id, artifact.id);
    let (_, bytes) = store
        .read_bytes(&owner, &reused.id, Duration::from_secs(60))
        .await
        .expect("reused Artifact remains readable");
    assert_eq!(bytes, payload);

    // 真正失败的 metadata DELETE 必须保留过期状态；再次上传修复对象后才可复活。
    sqlx::query("UPDATE artifacts SET expires_at=0 WHERE id=?")
        .bind(reused.id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_ready_delete BEFORE DELETE ON artifacts WHEN OLD.state='ready' BEGIN SELECT RAISE(ABORT, 'isolated metadata delete failure'); END")
        .execute(&pool).await.unwrap();
    assert!(matches!(
        store.sweep_expired().await,
        Err(ArtifactError::Storage(_))
    ));
    assert!(!object_present.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(
        store.open(&owner, &reused.id).await,
        Err(ArtifactError::NotFound)
    ));
    sqlx::query("DROP TRIGGER fail_ready_delete")
        .execute(&pool)
        .await
        .unwrap();
    let repaired = store
        .ingest(
            &owner,
            "application/octet-stream",
            Some(payload.len() as u64),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(repaired.id, artifact.id);
    assert!(object_present.load(std::sync::atomic::Ordering::SeqCst));
    let (_, bytes) = store
        .read_bytes(&owner, &repaired.id, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(bytes, payload);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn cancelled_retention_excludes_cleanup_until_its_sql_write_settles() {
    let directory = tempfile::tempdir().unwrap();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("guard.sqlite"))
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1000));
    let controlled = Arc::clone(&time);
    let root = directory.path().join("artifacts");
    let store = LocalArtifactStore::sqlite(pool.clone(), &root).with_clock(Arc::new(move || {
        controlled.load(std::sync::atomic::Ordering::SeqCst)
    }));
    let owner = Principal::new("cancelled-retention");
    let artifact = store
        .ingest(
            &owner,
            "text/plain",
            Some(4),
            bytes_stream(Bytes::from_static(b"safe")),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let mut blocker_connection = pool.acquire().await.unwrap();
    let blocker = blocker_connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.num_idle() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture returns its prior SQL connections before renewal");
    let renewing_store = store.clone();
    let renewing_owner = owner.clone();
    let id = artifact.id.clone();
    let renewal = tokio::spawn(async move {
        ArtifactStore::extend_retention(
            &renewing_store,
            &renewing_owner,
            &id,
            Duration::from_secs(600),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.num_idle() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal reaches the blocked SQLite writer");
    renewal.abort();
    assert!(renewal.await.unwrap_err().is_cancelled());
    time.store(100_000, std::sync::atomic::Ordering::SeqCst);
    // 独立进程的清理者也使用这把 OS 锁：取消等待者不能让其取得删除声明。
    let claim = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("locks").join(artifact.id.as_str()))
        .unwrap();
    assert!(matches!(
        claim.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    blocker.rollback().await.unwrap();
    drop(blocker_connection);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let expiry: i64 = sqlx::query_scalar("SELECT expires_at FROM artifacts WHERE id=?")
                .bind(artifact.id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
            if expiry == 601_000 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled caller does not abandon its accepted renewal");
    store.sweep_expired().await.unwrap();
    let (_, bytes) = store
        .read_bytes(&owner, &artifact.id, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(bytes, Bytes::from_static(b"safe"));
    pool.close().await;
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
    // 超过单个锁池容量的并发完成不得因重复内容发布而等待另一条锁连接。
    let uploads = (0..12).map(|_| {
        store.ingest(
            &owner,
            "application/octet-stream",
            Some(3),
            bytes_stream(Bytes::from_static(b"abc")),
            Duration::from_secs(60),
        )
    });
    let duplicates =
        tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(uploads))
            .await
            .expect("concurrent PostgreSQL publication must not exhaust its own lock pool");
    for duplicate in duplicates {
        assert_eq!(duplicate.unwrap().id, artifact.id);
    }
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
    let reader = store
        .open(&owner, &artifact.id)
        .await
        .expect("open ready Artifact");
    let ArtifactSource::LocalPath(object_path) = reader.source.clone() else {
        panic!("expected local Artifact");
    };
    drop(reader);
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
    let staging_artifact_id: String =
        sqlx::query_scalar("SELECT artifact_id FROM artifact_uploads WHERE id = ?")
            .bind(&upload.upload_id)
            .fetch_one(&pool)
            .await
            .expect("staging Artifact fixture");
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
        .bind(&staging_artifact_id)
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

#[tokio::test]
async fn stable_artifact_id_is_shared_by_ingest_chunkings_and_store_reconstruction() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let root = data_dir.path().join("artifacts");
    let store = LocalArtifactStore::sqlite(pool.clone(), &root);
    let owner = Principal::new("stable-owner");
    let payload = Bytes::from_static(b"ABCdef");

    let direct = store
        .ingest(
            &owner,
            "image/png",
            Some(6),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("direct ingestion");
    assert!(stravia_runtime_contract::identifier::valid_digest_id(
        direct.id.as_str()
    ));
    assert_eq!(
        direct.reference(),
        format!("stravia://artifacts/{}", direct.id.as_str())
    );
    let repeated = store
        .ingest(
            &owner,
            "image/png",
            Some(6),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("repeated ingestion");
    assert_eq!(direct.id, repeated.id);

    let two_parts = store
        .create_upload(
            &owner,
            ArtifactUploadRequest {
                mime_type: "image/png".into(),
                size: 6,
                idle_ttl: Duration::from_secs(60),
                retention_ttl: Duration::from_secs(60),
                policy: policy(),
            },
        )
        .await
        .expect("create two-part upload");
    let first_part = store
        .upload_part(
            &owner,
            &two_parts.upload_id,
            &two_parts.upload_token,
            1,
            bytes_stream(Bytes::from_static(b"AB")),
        )
        .await
        .expect("upload first part");
    let second_part = store
        .upload_part(
            &owner,
            &two_parts.upload_id,
            &two_parts.upload_token,
            2,
            bytes_stream(Bytes::from_static(b"Cdef")),
        )
        .await
        .expect("upload second part");
    let assembled = store
        .complete_upload(
            &owner,
            &two_parts.upload_id,
            &two_parts.upload_token,
            &[first_part, second_part],
        )
        .await
        .expect("complete two-part upload");
    assert_eq!(direct.id, assembled.id);

    let reconstructed = LocalArtifactStore::sqlite(pool.clone(), &root);
    let restored = reconstructed
        .ingest(
            &owner,
            "image/png",
            Some(6),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("ingestion after store reconstruction");
    assert_eq!(direct.id, restored.id);
    let (_, bytes) = reconstructed
        .read_bytes(&owner, &direct.id, Duration::from_secs(60))
        .await
        .expect("read reconstructed Artifact");
    assert_eq!(bytes, payload);
}

#[tokio::test]
async fn stable_artifact_id_scopes_to_owner_mime_and_bytes() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = LocalArtifactStore::sqlite(pool, data_dir.path().join("artifacts"));
    let owner = Principal::new("scope-owner");
    let stranger = Principal::new("scope-stranger");
    let payload = Bytes::from_static(b"payload");

    let base = store
        .ingest(
            &owner,
            "image/png",
            Some(7),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("base ingestion");
    let foreign = store
        .ingest(
            &stranger,
            "image/png",
            Some(7),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("foreign ingestion");
    assert_ne!(base.id, foreign.id);
    assert!(matches!(
        store.open(&stranger, &base.id).await,
        Err(ArtifactError::NotFound)
    ));
    assert!(matches!(
        store.open(&owner, &foreign.id).await,
        Err(ArtifactError::NotFound)
    ));

    let relabelled = store
        .ingest(
            &owner,
            "image/jpeg",
            Some(7),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("relabelled ingestion");
    assert_ne!(base.id, relabelled.id);
    let mutated = store
        .ingest(
            &owner,
            "image/png",
            Some(8),
            bytes_stream(Bytes::from_static(b"payload2")),
            Duration::from_secs(60),
        )
        .await
        .expect("mutated ingestion");
    assert_ne!(base.id, mutated.id);

    let (_, base_bytes) = store
        .read_bytes(&owner, &base.id, Duration::from_secs(60))
        .await
        .expect("read base");
    assert_eq!(base_bytes, payload);
    let (_, relabelled_bytes) = store
        .read_bytes(&owner, &relabelled.id, Duration::from_secs(60))
        .await
        .expect("read relabelled");
    assert_eq!(relabelled_bytes, payload);
    let (_, mutated_bytes) = store
        .read_bytes(&owner, &mutated.id, Duration::from_secs(60))
        .await
        .expect("read mutated");
    assert_eq!(mutated_bytes, Bytes::from_static(b"payload2"));
    let (_, foreign_bytes) = store
        .read_bytes(&stranger, &foreign.id, Duration::from_secs(60))
        .await
        .expect("read foreign");
    assert_eq!(foreign_bytes, payload);
}

#[tokio::test]
async fn concurrent_identical_uploads_converge_on_one_artifact_id() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let root = data_dir.path().join("artifacts");
    let owner = Principal::new("converge-owner");
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::with_capacity(8);
    for index in 0..8 {
        let store = LocalArtifactStore::sqlite(pool.clone(), &root);
        let owner = owner.clone();
        let barrier = Arc::clone(&barrier);
        let payload = Bytes::from_static(b"converge");
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            if index % 2 == 0 {
                store
                    .ingest(
                        &owner,
                        "image/png",
                        Some(payload.len() as u64),
                        bytes_stream(payload),
                        Duration::from_secs(60),
                    )
                    .await
            } else {
                let upload = store
                    .create_upload(
                        &owner,
                        ArtifactUploadRequest {
                            mime_type: "image/png".into(),
                            size: payload.len() as u64,
                            idle_ttl: Duration::from_secs(60),
                            retention_ttl: Duration::from_secs(60),
                            policy: policy(),
                        },
                    )
                    .await?;
                let part = store
                    .upload_part(
                        &owner,
                        &upload.upload_id,
                        &upload.upload_token,
                        1,
                        bytes_stream(payload),
                    )
                    .await?;
                store
                    .complete_upload(&owner, &upload.upload_id, &upload.upload_token, &[part])
                    .await
            }
        }));
    }

    let results = futures::future::join_all(tasks).await;
    let ids: Vec<ArtifactId> = results
        .into_iter()
        .map(|result| {
            result
                .expect("upload task")
                .expect("concurrent identical upload")
                .id
        })
        .collect();
    let converged = &ids[0];
    assert!(
        ids.iter().all(|id| id == converged),
        "concurrent identical uploads diverged: {ids:?}"
    );

    let reader = LocalArtifactStore::sqlite(pool.clone(), &root);
    let (_, bytes) = reader
        .read_bytes(&owner, converged, Duration::from_secs(60))
        .await
        .expect("read converged Artifact");
    assert_eq!(bytes, Bytes::from_static(b"converge"));
}

#[tokio::test]
async fn duplicate_upload_extends_retention_without_shortening_it() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let root = data_dir.path().join("artifacts");
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1_800_000_000_000));
    let controlled = time.clone();
    let clock: Arc<dyn Fn() -> i64 + Send + Sync> =
        Arc::new(move || controlled.load(std::sync::atomic::Ordering::SeqCst));
    let store = LocalArtifactStore::sqlite(pool, &root).with_clock(clock);
    let owner = Principal::new("retention-owner");
    let payload = Bytes::from_static(b"retained");

    let long = store
        .ingest(
            &owner,
            "image/png",
            Some(8),
            bytes_stream(payload.clone()),
            Duration::from_secs(3600),
        )
        .await
        .expect("long ingestion");
    time.fetch_add(60_000, std::sync::atomic::Ordering::SeqCst);
    let short = store
        .ingest(
            &owner,
            "image/png",
            Some(8),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("short duplicate ingestion");
    assert_eq!(long.id, short.id);
    // The short duplicate must not pull the retention horizon back with it.
    time.fetch_add(2_000_000, std::sync::atomic::Ordering::SeqCst);
    drop(
        store
            .open(&owner, &long.id)
            .await
            .expect("Artifact survives past the short retention window"),
    );

    // A longer duplicate extends the horizon from its own upload time.
    let extended = store
        .ingest(
            &owner,
            "image/png",
            Some(8),
            bytes_stream(payload),
            Duration::from_secs(3600),
        )
        .await
        .expect("extending ingestion");
    assert_eq!(long.id, extended.id);
    time.fetch_add(2_000_000, std::sync::atomic::Ordering::SeqCst);
    drop(
        store
            .open(&owner, &long.id)
            .await
            .expect("duplicate extends beyond the original retention window"),
    );
    // Retention is max(existing, now + ttl), never an accumulation of duplicates.
    time.fetch_add(2_000_000, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        store.open(&owner, &long.id).await,
        Err(ArtifactError::NotFound)
    ));
}

#[tokio::test]
async fn duplicate_upload_while_reader_held_neither_hangs_nor_corrupts() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = LocalArtifactStore::sqlite(pool, data_dir.path().join("artifacts"));
    let owner = Principal::new("held-owner");
    let payload = Bytes::from_static(b"held-payload");

    let first = store
        .ingest(
            &owner,
            "image/png",
            Some(12),
            bytes_stream(payload.clone()),
            Duration::from_secs(3600),
        )
        .await
        .expect("initial ingestion");
    let reader = store
        .open(&owner, &first.id)
        .await
        .expect("open held reader");
    let ArtifactSource::LocalPath(path) = &reader.source else {
        panic!("expected local Artifact");
    };

    let duplicate_store = store.clone();
    let duplicate_owner = owner.clone();
    let duplicate = tokio::time::timeout(
        Duration::from_secs(10),
        duplicate_store.ingest(
            &duplicate_owner,
            "image/png",
            Some(12),
            bytes_stream(payload.clone()),
            Duration::from_secs(3600),
        ),
    )
    .await
    .expect("duplicate upload must not hang while a reader is held")
    .expect("duplicate upload");
    assert_eq!(first.id, duplicate.id);

    assert_eq!(
        Bytes::from(tokio::fs::read(path).await.expect("held reader content")),
        payload
    );
    drop(reader);
    let (_, bytes) = store
        .read_bytes(&owner, &first.id, Duration::from_secs(60))
        .await
        .expect("read after duplicate upload");
    assert_eq!(bytes, payload);
}

#[tokio::test]
async fn reupload_after_expiry_sweep_restores_the_same_artifact_id() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let root = data_dir.path().join("artifacts");
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1_800_000_000_000));
    let controlled = time.clone();
    let clock: Arc<dyn Fn() -> i64 + Send + Sync> =
        Arc::new(move || controlled.load(std::sync::atomic::Ordering::SeqCst));
    let store = LocalArtifactStore::sqlite(pool, &root).with_clock(clock);
    let owner = Principal::new("revive-owner");
    let payload = Bytes::from_static(b"revived");

    let original = store
        .ingest(
            &owner,
            "image/png",
            Some(7),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("original ingestion");
    let (_, bytes) = store
        .read_bytes(&owner, &original.id, Duration::from_secs(60))
        .await
        .expect("read original");
    assert_eq!(bytes, payload);

    time.fetch_add(61_000, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        store.open(&owner, &original.id).await,
        Err(ArtifactError::NotFound)
    ));
    assert_eq!(
        store.sweep_expired().await.expect("sweep expired Artifact"),
        1
    );
    assert!(matches!(
        store.open(&owner, &original.id).await,
        Err(ArtifactError::NotFound)
    ));

    let revived = store
        .ingest(
            &owner,
            "image/png",
            Some(7),
            bytes_stream(payload.clone()),
            Duration::from_secs(60),
        )
        .await
        .expect("re-ingestion after sweep");
    assert_eq!(original.id, revived.id);
    let (_, bytes) = store
        .read_bytes(&owner, &revived.id, Duration::from_secs(60))
        .await
        .expect("read revived");
    assert_eq!(bytes, payload);
}

#[tokio::test]
async fn failed_duplicate_complete_upload_keeps_existing_ready_artifact() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = LocalArtifactStore::sqlite(pool, data_dir.path().join("artifacts"));
    let owner = Principal::new("failed-duplicate-owner");
    let payload = Bytes::from_static(b"existing");

    let ready = store
        .ingest(
            &owner,
            "image/png",
            Some(8),
            bytes_stream(payload.clone()),
            Duration::from_secs(3600),
        )
        .await
        .expect("ready ingestion");

    let upload = store
        .create_upload(
            &owner,
            ArtifactUploadRequest {
                mime_type: "image/png".into(),
                size: 8,
                idle_ttl: Duration::from_secs(60),
                retention_ttl: Duration::from_secs(60),
                policy: policy(),
            },
        )
        .await
        .expect("create duplicate upload");
    let part = store
        .upload_part(
            &owner,
            &upload.upload_id,
            &upload.upload_token,
            1,
            bytes_stream(payload),
        )
        .await
        .expect("stage duplicate part");
    // Corrupt the staged bytes so assembly fails verification against the manifest.
    tokio::fs::write(
        store
            .staging_dir(&upload.upload_id)
            .join(format!("{:08}.part", part.part_number)),
        b"corrupted",
    )
    .await
    .expect("corrupt staged part");

    assert!(matches!(
        store
            .complete_upload(&owner, &upload.upload_id, &upload.upload_token, &[part])
            .await,
        Err(ArtifactError::Invalid(_))
    ));
    // The pre-existing ready Artifact must survive the failed duplicate untouched.
    let (_, bytes) = store
        .read_bytes(&owner, &ready.id, Duration::from_secs(60))
        .await
        .expect("existing Artifact still readable");
    assert_eq!(bytes, Bytes::from_static(b"existing"));
}

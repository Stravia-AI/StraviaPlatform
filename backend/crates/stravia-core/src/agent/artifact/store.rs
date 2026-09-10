use super::*;

#[path = "s3.rs"]
mod s3;
#[path = "transfer.rs"]
mod transfer;

#[derive(Clone)]
enum ArtifactDatabase {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

#[derive(Clone)]
pub struct LocalArtifactStore {
    database: ArtifactDatabase,
    root: Arc<PathBuf>,
    upload_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    settings: Arc<tokio::sync::RwLock<ArtifactSettings>>,
    lock_pool: Option<PgPool>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Debug, sqlx::FromRow)]
struct UploadRow {
    artifact_id: String,
    principal: String,
    token_hash: String,
    declared_size: i64,
    expires_at: i64,
    created_at: i64,
    mime_type: String,
}

impl LocalArtifactStore {
    fn now(&self) -> i64 {
        (self.clock)()
    }

    #[cfg(test)]
    pub(super) fn with_clock(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub fn sqlite(pool: SqlitePool, root: impl Into<PathBuf>) -> Self {
        Self {
            database: ArtifactDatabase::Sqlite(pool),
            lock_pool: None,
            root: Arc::new(root.into()),
            upload_locks: Arc::new(Mutex::new(HashMap::new())),
            settings: Arc::new(tokio::sync::RwLock::new(ArtifactSettings::default())),
            clock: Arc::new(now_millis),
        }
    }

    pub fn postgres(pool: PgPool, root: impl Into<PathBuf>) -> Self {
        let lock_pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with((*pool.connect_options()).clone());
        Self {
            database: ArtifactDatabase::Postgres(pool),
            lock_pool: Some(lock_pool),
            root: Arc::new(root.into()),
            upload_locks: Arc::new(Mutex::new(HashMap::new())),
            settings: Arc::new(tokio::sync::RwLock::new(ArtifactSettings::default())),
            clock: Arc::new(now_millis),
        }
    }

    pub(super) fn staging_dir(&self, upload_id: &str) -> PathBuf {
        self.root.join("staging").join(upload_id)
    }

    pub(super) fn object_path(&self, artifact_id: &str) -> PathBuf {
        self.root.join("objects").join(artifact_id)
    }

    async fn lock_upload(
        &self,
        upload_id: &str,
    ) -> Result<(OwnedMutexGuard<()>, Arc<dyn ArtifactReadGuard>), ArtifactError> {
        if upload_id.is_empty()
            || !upload_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ArtifactError::NotFound);
        }
        let lock = {
            let mut locks = self.upload_locks.lock().await;
            Arc::clone(
                locks
                    .entry(upload_id.to_owned())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let local = lock.lock_owned().await;
        let persistent: Arc<dyn ArtifactReadGuard> = match &self.database {
            ArtifactDatabase::Sqlite(_) => {
                let directory = self.root.join("locks");
                tokio::fs::create_dir_all(&directory)
                    .await
                    .map_err(storage_error)?;
                let path = directory.join(upload_id);
                let file = tokio::task::spawn_blocking(move || {
                    let file = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(path)?;
                    file.lock()?;
                    Ok::<_, std::io::Error>(file)
                })
                .await
                .map_err(storage_error)?
                .map_err(storage_error)?;
                Arc::new(file)
            }
            ArtifactDatabase::Postgres(_) => {
                let mut transaction = self
                    .lock_pool
                    .as_ref()
                    .expect("PostgreSQL lock pool")
                    .begin()
                    .await
                    .map_err(storage_error)?;
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 2))")
                    .bind(upload_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                Arc::new(Mutex::new(transaction))
            }
        };
        Ok((local, persistent))
    }

    async fn load_upload(&self, upload_id: &str) -> Result<Option<UploadRow>, ArtifactError> {
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT u.artifact_id, u.principal, u.token_hash, u.declared_size, \
                     u.expires_at, a.mime_type, u.created_at \
                     FROM artifact_uploads u JOIN artifacts a ON a.id = u.artifact_id WHERE u.id = ?",
                )
                .bind(upload_id)
                .fetch_optional(pool)
                .await
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT u.artifact_id, u.principal, u.token_hash, u.declared_size, \
                     u.expires_at, a.mime_type, u.created_at \
                     FROM artifact_uploads u JOIN artifacts a ON a.id = u.artifact_id WHERE u.id = $1",
                )
                .bind(upload_id)
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(storage_error)
    }

    fn authorize_upload(
        &self,
        row: &UploadRow,
        principal: &Principal,
        token: &str,
    ) -> Result<(), ArtifactError> {
        if row.principal != principal.continuation_key() {
            return Err(ArtifactError::Forbidden);
        }
        if row.token_hash != sha256_hex(token.as_bytes()) {
            return Err(ArtifactError::Unauthorized);
        }
        if row.expires_at <= self.now() {
            return Err(ArtifactError::NotFound);
        }
        Ok(())
    }
}

#[async_trait]
impl ArtifactStore for LocalArtifactStore {
    async fn configure(&self, settings: &ArtifactSettings) -> Result<(), ArtifactError> {
        settings.validate()?;
        *self.settings.write().await = settings.clone();
        Ok(())
    }

    async fn ingest(
        &self,
        principal: &Principal,
        mime_type: &str,
        size: Option<u64>,
        bytes: ArtifactByteStream,
        retention: Duration,
    ) -> Result<ArtifactRef, ArtifactError> {
        self.ingest_stream(principal, mime_type, size, bytes, retention)
            .await
    }

    async fn download(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        retention: Duration,
        settings: &ArtifactSettings,
    ) -> Result<ArtifactDownload, ArtifactError> {
        self.issue_download(principal, id, retention, settings)
            .await
    }

    async fn read_download(&self, token: &str) -> Result<ArtifactReader, ArtifactError> {
        self.open_grant(token).await
    }

    async fn read_bytes(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        retention: Duration,
    ) -> Result<(ArtifactRef, Bytes), ArtifactError> {
        ArtifactStore::extend_retention(self, principal, id, retention).await?;
        let reader = self.open(principal, id).await?;
        let ArtifactSource::LocalPath(path) = &reader.source else {
            return Err(ArtifactError::Storage(
                "invalid internal Artifact source".into(),
            ));
        };
        let bytes = tokio::fs::read(path).await.map_err(storage_error)?;
        if bytes.len() as u64 != reader.artifact.size {
            return Err(ArtifactError::Storage(
                "Artifact content size mismatch".into(),
            ));
        }
        Ok((reader.artifact, Bytes::from(bytes)))
    }

    async fn create_upload(
        &self,
        principal: &Principal,
        request: ArtifactUploadRequest,
    ) -> Result<ArtifactUpload, ArtifactError> {
        validate_upload_request(&request)?;
        let artifact_id = ArtifactId::new(format!("artifact_{}", uuid::Uuid::new_v4().simple()));
        let upload_id = format!("upload_{}", uuid::Uuid::new_v4().simple());
        let upload_token = hex_bytes(&rand::random::<[u8; 32]>());
        let token_hash = sha256_hex(upload_token.as_bytes());
        let created_at = self.now();
        let upload_ttl_millis = i64::try_from(request.idle_ttl.as_millis()).unwrap_or(i64::MAX);
        let upload_expires_at = created_at.saturating_add(upload_ttl_millis);
        let retention_ttl_millis =
            i64::try_from(request.retention_ttl.as_millis()).unwrap_or(i64::MAX);
        let artifact_expires_at = created_at.saturating_add(retention_ttl_millis);
        let principal_key = principal.continuation_key();
        let backend_key = format!("objects/{}", artifact_id.as_str());
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                let mut connection = pool.acquire().await.map_err(storage_error)?;
                let mut transaction = connection
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(storage_error)?;
                let (active_uploads, staged_bytes): (i64, i64) = sqlx::query_as(
                    "SELECT COUNT(*), COALESCE(SUM(declared_size), 0) FROM artifact_uploads \
                     WHERE principal = ? AND expires_at > ?",
                )
                .bind(&principal_key)
                .bind(created_at)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
                validate_staging_quota(active_uploads, staged_bytes, request.size)?;
                sqlx::query(
                    "INSERT INTO artifacts \
                     (id, principal, mime_type, size, backend_key, state, expires_at, created_at) \
                     VALUES (?, ?, ?, ?, ?, 'staging', ?, ?)",
                )
                .bind(artifact_id.as_str())
                .bind(&principal_key)
                .bind(&request.mime_type)
                .bind(request.size as i64)
                .bind(&backend_key)
                .bind(artifact_expires_at)
                .bind(created_at)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
                sqlx::query(
                    "INSERT INTO artifact_uploads \
                     (id, artifact_id, principal, token_hash, declared_size, expires_at, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&upload_id)
                .bind(artifact_id.as_str())
                .bind(&principal_key)
                .bind(&token_hash)
                .bind(request.size as i64)
                .bind(upload_expires_at)
                .bind(created_at)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
                transaction.commit().await.map_err(storage_error)?;
            }
            ArtifactDatabase::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage_error)?;
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(&principal_key)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                let (active_uploads, staged_bytes): (i64, i64) = sqlx::query_as(
                    "SELECT COUNT(*), LEAST(COALESCE(SUM(declared_size), 0), $3::bigint)::bigint FROM artifact_uploads \
                     WHERE principal = $1 AND expires_at > $2",
                )
                .bind(&principal_key)
                .bind(created_at)
                .bind(MAX_PRINCIPAL_STAGING_BYTES as i64 + 1)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
                validate_staging_quota(active_uploads, staged_bytes, request.size)?;
                sqlx::query(
                    "INSERT INTO artifacts \
                     (id, principal, mime_type, size, backend_key, state, expires_at, created_at) \
                     VALUES ($1, $2, $3, $4, $5, 'staging', $6, $7)",
                )
                .bind(artifact_id.as_str())
                .bind(&principal_key)
                .bind(&request.mime_type)
                .bind(request.size as i64)
                .bind(&backend_key)
                .bind(artifact_expires_at)
                .bind(created_at)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
                sqlx::query(
                    "INSERT INTO artifact_uploads \
                     (id, artifact_id, principal, token_hash, declared_size, expires_at, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(&upload_id)
                .bind(artifact_id.as_str())
                .bind(&principal_key)
                .bind(&token_hash)
                .bind(request.size as i64)
                .bind(upload_expires_at)
                .bind(created_at)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
                transaction.commit().await.map_err(storage_error)?;
            }
        }
        tokio::fs::create_dir_all(self.staging_dir(&upload_id))
            .await
            .map_err(storage_error)?;
        Ok(ArtifactUpload {
            upload_id,
            artifact_id,
            upload_token,
            expires_at: upload_expires_at,
        })
    }

    async fn upload_part(
        &self,
        principal: &Principal,
        upload_id: &str,
        upload_token: &str,
        part_number: u32,
        mut bytes: ArtifactByteStream,
    ) -> Result<UploadedArtifactPart, ArtifactError> {
        if part_number == 0 {
            return Err(ArtifactError::Invalid(
                "part number must be greater than zero".into(),
            ));
        }
        let _upload_guard = self.lock_upload(upload_id).await?;
        let row = self
            .load_upload(upload_id)
            .await?
            .ok_or(ArtifactError::NotFound)?;
        self.authorize_upload(&row, principal, upload_token)?;
        let existing_parts = self.load_parts(upload_id).await?;
        let existing_size = existing_parts
            .iter()
            .filter(|part| part.part_number != part_number)
            .try_fold(0_u64, |total, part| {
                total
                    .checked_add(part.size)
                    .ok_or_else(|| ArtifactError::Invalid("Artifact upload size overflow".into()))
            })?;
        let declared_size = u64::try_from(row.declared_size)
            .map_err(|_| ArtifactError::Storage("invalid declared upload size".into()))?;
        let remaining_size = declared_size.saturating_sub(existing_size);
        let staging = self.staging_dir(upload_id);
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(storage_error)?;
        let path = staging.join(format!("{part_number:08}.part"));
        let temporary = staging.join(format!("{part_number:08}.tmp"));
        let mut file = tokio::fs::File::create(&temporary)
            .await
            .map_err(storage_error)?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&temporary).await;
                    return Err(error);
                }
            };
            let chunk_size = u64::try_from(chunk.len())
                .map_err(|_| ArtifactError::Invalid("Artifact part is too large".into()))?;
            size = size
                .checked_add(chunk_size)
                .ok_or_else(|| ArtifactError::Invalid("Artifact upload size overflow".into()))?;
            if chunk_size > MAX_ARTIFACT_BYTES || size > remaining_size {
                drop(file);
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(ArtifactError::Invalid(
                    "Artifact part exceeds the declared upload size".into(),
                ));
            }
            digest.update(&chunk);
            file.write_all(&chunk).await.map_err(storage_error)?;
        }
        file.flush().await.map_err(storage_error)?;
        drop(file);
        if tokio::fs::try_exists(&path).await.map_err(storage_error)? {
            tokio::fs::remove_file(&path).await.map_err(storage_error)?;
        }
        if let Err(error) = tokio::fs::rename(&temporary, &path).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(storage_error(error));
        }
        let etag = hex_bytes(&digest.finalize());
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO artifact_upload_parts (upload_id, part_number, etag, size) \
                     VALUES (?, ?, ?, ?) ON CONFLICT(upload_id, part_number) \
                     DO UPDATE SET etag = excluded.etag, size = excluded.size",
                )
                .bind(upload_id)
                .bind(i64::from(part_number))
                .bind(&etag)
                .bind(size as i64)
                .execute(pool)
                .await
                .map_err(storage_error)?;
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO artifact_upload_parts (upload_id, part_number, etag, size) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT(upload_id, part_number) \
                     DO UPDATE SET etag = EXCLUDED.etag, size = EXCLUDED.size",
                )
                .bind(upload_id)
                .bind(i64::from(part_number))
                .bind(&etag)
                .bind(size as i64)
                .execute(pool)
                .await
                .map_err(storage_error)?;
            }
        }
        let idle_ttl = row.expires_at.saturating_sub(row.created_at);
        let refreshed_expiry = self.now().saturating_add(idle_ttl);
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query("UPDATE artifact_uploads SET expires_at = ? WHERE id = ?")
                    .bind(refreshed_expiry)
                    .bind(upload_id)
                    .execute(pool)
                    .await
                    .map_err(storage_error)?;
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query("UPDATE artifact_uploads SET expires_at = $1 WHERE id = $2")
                    .bind(refreshed_expiry)
                    .bind(upload_id)
                    .execute(pool)
                    .await
                    .map_err(storage_error)?;
            }
        }
        Ok(UploadedArtifactPart {
            part_number,
            etag,
            size,
        })
    }

    async fn complete_upload(
        &self,
        principal: &Principal,
        upload_id: &str,
        upload_token: &str,
        parts: &[UploadedArtifactPart],
    ) -> Result<ArtifactRef, ArtifactError> {
        let _upload_guard = self.lock_upload(upload_id).await?;
        let row = self
            .load_upload(upload_id)
            .await?
            .ok_or(ArtifactError::NotFound)?;
        self.authorize_upload(&row, principal, upload_token)?;
        if parts.is_empty() {
            return Err(ArtifactError::Invalid("upload has no parts".into()));
        }
        let mut expected = parts.to_vec();
        expected.sort_by_key(|part| part.part_number);
        for (index, part) in expected.iter().enumerate() {
            if part.part_number as usize != index + 1 {
                return Err(ArtifactError::Invalid(
                    "Artifact parts must be contiguous from one".into(),
                ));
            }
        }
        let stored = self.load_parts(upload_id).await?;
        if stored != expected {
            return Err(ArtifactError::Invalid(
                "Artifact part manifest does not match uploaded parts".into(),
            ));
        }
        let total_size = stored.iter().try_fold(0_u64, |total, part| {
            total
                .checked_add(part.size)
                .ok_or_else(|| ArtifactError::Invalid("Artifact upload size overflow".into()))
        })?;
        let declared_size = u64::try_from(row.declared_size)
            .map_err(|_| ArtifactError::Storage("invalid declared upload size".into()))?;
        if total_size != declared_size {
            return Err(ArtifactError::Invalid(format!(
                "Artifact upload size mismatch: expected {declared_size}, received {total_size}"
            )));
        }
        tokio::fs::create_dir_all(self.root.join("objects"))
            .await
            .map_err(storage_error)?;
        let final_path = self.object_path(&row.artifact_id);
        let temporary = final_path.with_extension("tmp");
        let mut output = tokio::fs::File::create(&temporary)
            .await
            .map_err(storage_error)?;
        for part in &stored {
            let part_path = self
                .staging_dir(upload_id)
                .join(format!("{:08}.part", part.part_number));
            let mut input = tokio::fs::File::open(part_path)
                .await
                .map_err(storage_error)?;
            let mut digest = Sha256::new();
            let mut copied = 0_u64;
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let read = input.read(&mut buffer).await.map_err(storage_error)?;
                if read == 0 {
                    break;
                }
                let chunk = &buffer[..read];
                digest.update(chunk);
                copied = copied.checked_add(read as u64).ok_or_else(|| {
                    ArtifactError::Invalid("Artifact upload size overflow".into())
                })?;
                output.write_all(chunk).await.map_err(storage_error)?;
            }
            if copied != part.size || hex_bytes(&digest.finalize()) != part.etag {
                drop(output);
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(ArtifactError::Invalid(
                    "Artifact part content does not match its manifest".into(),
                ));
            }
        }
        output.flush().await.map_err(storage_error)?;
        drop(output);
        tokio::fs::rename(&temporary, &final_path)
            .await
            .map_err(storage_error)?;
        {
            let settings = self.transfer_settings(None).await?;
            if let Some(s3) = &settings.s3 {
                self.persist_s3_location(&row.artifact_id, s3).await?;
                self.upload_s3(
                    s3,
                    &ArtifactId::new(&row.artifact_id),
                    &final_path,
                    total_size,
                    &row.mime_type,
                )
                .await?;
            }
        }
        self.mark_ready(upload_id, &row.artifact_id).await?;
        if let Err(error) = tokio::fs::remove_dir_all(self.staging_dir(upload_id)).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(upload_id, error = %error, "completed Artifact staging cleanup failed");
        }
        drop(_upload_guard);
        self.upload_locks.lock().await.remove(upload_id);
        Ok(ArtifactRef {
            id: ArtifactId::new(row.artifact_id),
            mime_type: row.mime_type,
            size: total_size,
        })
    }

    async fn open(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<ArtifactReader, ArtifactError> {
        self.open_authorized(Some(principal), id, None).await
    }

    async fn extend_retention(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        retention: Duration,
    ) -> Result<(), ArtifactError> {
        let retention_millis = i64::try_from(retention.as_millis()).unwrap_or(i64::MAX);
        let expires_at = self.now().saturating_add(retention_millis);
        let principal_key = principal.continuation_key();
        let affected = match &self.database {
            ArtifactDatabase::Sqlite(pool) => sqlx::query(
                "UPDATE artifacts SET expires_at = MAX(expires_at, ?) \
                 WHERE id = ? AND principal = ? AND state = 'ready' AND expires_at > ?",
            )
            .bind(expires_at)
            .bind(id.as_str())
            .bind(&principal_key)
            .bind(self.now())
            .execute(pool)
            .await
            .map_err(storage_error)?
            .rows_affected(),
            ArtifactDatabase::Postgres(pool) => sqlx::query(
                "UPDATE artifacts SET expires_at = GREATEST(expires_at, $1) \
                 WHERE id = $2 AND principal = $3 AND state = 'ready' AND expires_at > $4",
            )
            .bind(expires_at)
            .bind(id.as_str())
            .bind(&principal_key)
            .bind(self.now())
            .execute(pool)
            .await
            .map_err(storage_error)?
            .rows_affected(),
        };
        if affected == 0 {
            return Err(ArtifactError::NotFound);
        }
        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64, ArtifactError> {
        let now = self.now();
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query("DELETE FROM artifact_download_grants WHERE expires_at<=?")
                    .bind(now)
                    .execute(pool)
                    .await
                    .map_err(storage_error)?;
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query("DELETE FROM artifact_download_grants WHERE expires_at<=$1")
                    .bind(now)
                    .execute(pool)
                    .await
                    .map_err(storage_error)?;
            }
        }
        let expired_uploads: Vec<(String, String)> = match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query_as("SELECT id, artifact_id FROM artifact_uploads WHERE expires_at <= ?")
                    .bind(now)
                    .fetch_all(pool)
                    .await
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT id, artifact_id FROM artifact_uploads WHERE expires_at <= $1",
                )
                .bind(now)
                .fetch_all(pool)
                .await
            }
        }
        .map_err(storage_error)?;
        for (upload_id, _) in &expired_uploads {
            let _guard = self.lock_upload(upload_id).await?;
            let Some(row) = self.load_upload(upload_id).await? else {
                continue;
            };
            if row.expires_at > self.now() {
                continue;
            }
            match tokio::fs::remove_dir_all(self.staging_dir(upload_id)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage_error(error)),
            }
            match &self.database {
                ArtifactDatabase::Sqlite(pool) => {
                    sqlx::query("DELETE FROM artifact_uploads WHERE id = ?")
                        .bind(upload_id)
                        .execute(pool)
                        .await
                        .map_err(storage_error)?;
                }
                ArtifactDatabase::Postgres(pool) => {
                    sqlx::query("DELETE FROM artifact_uploads WHERE id = $1")
                        .bind(upload_id)
                        .execute(pool)
                        .await
                        .map_err(storage_error)?;
                }
            }
        }

        let expired_artifacts: Vec<(String, String, Option<String>)> = match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT a.id, a.state, u.id FROM artifacts a \
                     LEFT JOIN artifact_uploads u ON u.artifact_id = a.id \
                     WHERE a.expires_at <= ? AND (a.state='ready' OR u.id IS NULL)",
                )
                .bind(now)
                .fetch_all(pool)
                .await
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT a.id, a.state, u.id FROM artifacts a \
                     LEFT JOIN artifact_uploads u ON u.artifact_id = a.id \
                     WHERE a.expires_at <= $1 AND (a.state='ready' OR u.id IS NULL)",
                )
                .bind(now)
                .fetch_all(pool)
                .await
            }
        }
        .map_err(storage_error)?;
        let mut affected = 0_u64;
        for (artifact_id, state, upload_id) in expired_artifacts {
            if state != "ready" && state != "staging" {
                return Err(ArtifactError::Storage(format!(
                    "invalid Artifact state: {state}"
                )));
            }
            if state == "ready" {
                let deleted = match &self.database {
                    ArtifactDatabase::Sqlite(pool) => {
                        self.sweep_ready_sqlite(pool, &artifact_id, now).await?
                    }
                    ArtifactDatabase::Postgres(pool) => {
                        self.sweep_ready_postgres(pool, &artifact_id, now).await?
                    }
                };
                affected = affected.saturating_add(deleted);
                continue;
            }
            self.remove_object(&artifact_id).await?;
            if state == "staging" {
                let staging_upload_id = upload_id.or_else(|| {
                    expired_uploads
                        .iter()
                        .find(|(_, expired_artifact_id)| expired_artifact_id == &artifact_id)
                        .map(|(expired_upload_id, _)| expired_upload_id.clone())
                });
                if let Some(staging_upload_id) = staging_upload_id {
                    match tokio::fs::remove_dir_all(self.staging_dir(&staging_upload_id)).await {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(storage_error(error)),
                    }
                }
            }
            let result = match &self.database {
                ArtifactDatabase::Sqlite(pool) => sqlx::query("DELETE FROM artifacts WHERE id = ?")
                    .bind(&artifact_id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected()),
                ArtifactDatabase::Postgres(pool) => {
                    sqlx::query("DELETE FROM artifacts WHERE id = $1")
                        .bind(&artifact_id)
                        .execute(pool)
                        .await
                        .map(|result| result.rows_affected())
                }
            }
            .map_err(storage_error)?;
            affected = affected.saturating_add(result);
        }
        Ok(affected)
    }
}

impl LocalArtifactStore {
    async fn sweep_ready_sqlite(
        &self,
        pool: &sqlx::SqlitePool,
        artifact_id: &str,
        now: i64,
    ) -> Result<u64, ArtifactError> {
        let location = self.object_location(artifact_id).await?;
        let settings = self.transfer_settings(None).await?;
        let directory = self.root.join("locks");
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(storage_error)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(artifact_id))
            .map_err(storage_error)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(0),
            Err(error) => return Err(storage_error(error)),
        }
        let mut transaction = pool.begin().await.map_err(storage_error)?;
        let claimed = sqlx::query(
            "UPDATE artifacts SET expires_at = expires_at WHERE id = ? AND state = 'ready' AND expires_at <= ? AND NOT EXISTS (SELECT 1 FROM artifact_download_grants g WHERE g.artifact_id=artifacts.id AND g.expires_at>?)",
        )
        .bind(artifact_id)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?
        .rows_affected();
        if claimed == 0 {
            return Ok(0);
        }
        self.remove_object_at(artifact_id, location, &settings)
            .await?;
        let deleted = sqlx::query(
            "DELETE FROM artifacts WHERE id = ? AND state = 'ready' AND expires_at <= ? AND NOT EXISTS (SELECT 1 FROM artifact_download_grants g WHERE g.artifact_id=artifacts.id AND g.expires_at>?)",
        )
        .bind(artifact_id)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?
        .rows_affected();
        transaction.commit().await.map_err(storage_error)?;
        Ok(deleted)
    }

    async fn sweep_ready_postgres(
        &self,
        pool: &sqlx::PgPool,
        artifact_id: &str,
        now: i64,
    ) -> Result<u64, ArtifactError> {
        let location = self.object_location(artifact_id).await?;
        let settings = self.transfer_settings(None).await?;
        let mut transaction = pool.begin().await.map_err(storage_error)?;
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 1))")
                .bind(artifact_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(storage_error)?;
        if !locked {
            return Ok(0);
        }
        let claimed = sqlx::query(
            "UPDATE artifacts SET expires_at = expires_at WHERE id = $1 AND state = 'ready' AND expires_at <= $2 AND NOT EXISTS (SELECT 1 FROM artifact_download_grants g WHERE g.artifact_id=artifacts.id AND g.expires_at>$2)",
        )
        .bind(artifact_id)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?
        .rows_affected();
        if claimed == 0 {
            return Ok(0);
        }
        self.remove_object_at(artifact_id, location, &settings)
            .await?;
        let deleted = sqlx::query(
            "DELETE FROM artifacts WHERE id = $1 AND state = 'ready' AND expires_at <= $2 AND NOT EXISTS (SELECT 1 FROM artifact_download_grants g WHERE g.artifact_id=artifacts.id AND g.expires_at>$2)",
        )
        .bind(artifact_id)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?
        .rows_affected();
        transaction.commit().await.map_err(storage_error)?;
        Ok(deleted)
    }

    async fn load_parts(
        &self,
        upload_id: &str,
    ) -> Result<Vec<UploadedArtifactPart>, ArtifactError> {
        let rows: Vec<(i64, String, i64)> = match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT part_number, etag, size FROM artifact_upload_parts \
                 WHERE upload_id = ? ORDER BY part_number",
                )
                .bind(upload_id)
                .fetch_all(pool)
                .await
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT part_number, etag, size FROM artifact_upload_parts \
                 WHERE upload_id = $1 ORDER BY part_number",
                )
                .bind(upload_id)
                .fetch_all(pool)
                .await
            }
        }
        .map_err(storage_error)?;
        rows.into_iter()
            .map(|(part_number, etag, size)| {
                Ok(UploadedArtifactPart {
                    part_number: u32::try_from(part_number)
                        .map_err(|_| ArtifactError::Storage("invalid stored part number".into()))?,
                    etag,
                    size: u64::try_from(size)
                        .map_err(|_| ArtifactError::Storage("invalid stored part size".into()))?,
                })
            })
            .collect()
    }

    async fn mark_ready(&self, upload_id: &str, artifact_id: &str) -> Result<(), ArtifactError> {
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                let mut transaction = pool.begin().await.map_err(storage_error)?;
                sqlx::query("UPDATE artifacts SET state = 'ready' WHERE id = ?")
                    .bind(artifact_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                sqlx::query("DELETE FROM artifact_uploads WHERE id = ?")
                    .bind(upload_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                transaction.commit().await.map_err(storage_error)?;
            }
            ArtifactDatabase::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage_error)?;
                sqlx::query("UPDATE artifacts SET state = 'ready' WHERE id = $1")
                    .bind(artifact_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                sqlx::query("DELETE FROM artifact_uploads WHERE id = $1")
                    .bind(upload_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                transaction.commit().await.map_err(storage_error)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn delete_ready(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<(), ArtifactError> {
        let principal_key = principal.continuation_key();
        let state: Option<String> = match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query_scalar(
                    "UPDATE artifacts SET expires_at=0 WHERE id=? AND principal=? RETURNING state",
                )
                .bind(id.as_str())
                .bind(&principal_key)
                .fetch_optional(pool)
                .await
            }
            ArtifactDatabase::Postgres(pool) => sqlx::query_scalar(
                "UPDATE artifacts SET expires_at=0 WHERE id=$1 AND principal=$2 RETURNING state",
            )
            .bind(id.as_str())
            .bind(&principal_key)
            .fetch_optional(pool)
            .await,
        }
        .map_err(storage_error)?;
        match state.as_deref() {
            Some("ready") => match &self.database {
                ArtifactDatabase::Sqlite(pool) => {
                    self.sweep_ready_sqlite(pool, id.as_str(), self.now())
                        .await?;
                }
                ArtifactDatabase::Postgres(pool) => {
                    self.sweep_ready_postgres(pool, id.as_str(), self.now())
                        .await?;
                }
            },
            Some("staging") => {
                self.remove_object(id.as_str()).await?;
                match &self.database {
                    ArtifactDatabase::Sqlite(pool) => {
                        sqlx::query(
                            "DELETE FROM artifacts WHERE id=? AND principal=? AND state='staging'",
                        )
                        .bind(id.as_str())
                        .bind(&principal_key)
                        .execute(pool)
                        .await
                        .map_err(storage_error)?;
                    }
                    ArtifactDatabase::Postgres(pool) => {
                        sqlx::query("DELETE FROM artifacts WHERE id=$1 AND principal=$2 AND state='staging'").bind(id.as_str()).bind(&principal_key).execute(pool).await.map_err(storage_error)?;
                    }
                }
            }
            None => {}
            _ => return Err(ArtifactError::Storage("unknown Artifact state".into())),
        }
        Ok(())
    }

    pub(crate) async fn extend_retention(
        &self,
        principal: &Principal,
        ids: &[ArtifactId],
        retention: Duration,
    ) -> Result<(), ArtifactError> {
        let mut ids = ids.iter().collect::<Vec<_>>();
        ids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        ids.dedup();
        let extension = self
            .now()
            .saturating_add(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX));
        let principal_key = principal.continuation_key();
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                let mut transaction = pool.begin().await.map_err(storage_error)?;
                for id in &ids {
                    let rows_affected = sqlx::query(
                        "UPDATE artifacts SET expires_at = MAX(expires_at, ?) WHERE id = ? AND principal = ? AND state = 'ready' AND expires_at > ?",
                    )
                    .bind(extension)
                    .bind(id.as_str())
                    .bind(&principal_key)
                    .bind(self.now())
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?
                    .rows_affected();
                    if rows_affected == 0 {
                        return Err(ArtifactError::NotFound);
                    }
                }
                transaction.commit().await.map_err(storage_error)?;
            }
            ArtifactDatabase::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(storage_error)?;
                for id in &ids {
                    let rows_affected = sqlx::query(
                        "UPDATE artifacts SET expires_at = GREATEST(expires_at, $1) WHERE id = $2 AND principal = $3 AND state = 'ready' AND expires_at > $4",
                    )
                    .bind(extension)
                    .bind(id.as_str())
                    .bind(&principal_key)
                    .bind(self.now())
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?
                    .rows_affected();
                    if rows_affected == 0 {
                        return Err(ArtifactError::NotFound);
                    }
                }
                transaction.commit().await.map_err(storage_error)?;
            }
        }
        Ok(())
    }
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn storage_error(error: impl std::fmt::Display) -> ArtifactError {
    ArtifactError::Storage(error.to_string())
}

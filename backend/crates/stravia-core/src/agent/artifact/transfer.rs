use super::*;

#[derive(sqlx::FromRow)]
struct ObjectRow {
    principal: String,
    mime_type: String,
    size: i64,
    expires_at: i64,
    storage_backend: String,
    storage_endpoint: Option<String>,
    storage_bucket: Option<String>,
}

impl LocalArtifactStore {
    pub(super) async fn transfer_settings(
        &self,
        fallback: Option<&ArtifactSettings>,
    ) -> Result<ArtifactSettings, ArtifactError> {
        let value: Option<String> = match &self.database {
            ArtifactDatabase::Sqlite(pool) => {
                sqlx::query_scalar("SELECT value FROM settings WHERE name='artifact_settings'")
                    .fetch_optional(pool)
                    .await
            }
            ArtifactDatabase::Postgres(pool) => {
                sqlx::query_scalar("SELECT value FROM settings WHERE name='artifact_settings'")
                    .fetch_optional(pool)
                    .await
            }
        }
        .map_err(storage_error)?;
        let settings = match value {
            Some(value) => serde_json::from_str(&value).map_err(|error| {
                ArtifactError::Storage(format!("invalid artifact_settings: {error}"))
            })?,
            None => match fallback {
                Some(settings) => settings.clone(),
                None => self.settings.read().await.clone(),
            },
        };
        settings.validate()?;
        Ok(settings)
    }

    async fn object_row(&self, id: &ArtifactId) -> Result<ObjectRow, ArtifactError> {
        let row = match &self.database {
            ArtifactDatabase::Sqlite(pool) => sqlx::query_as::<_, ObjectRow>("SELECT principal,mime_type,size,expires_at,storage_backend,storage_endpoint,storage_bucket FROM artifacts WHERE id=? AND state='ready'").bind(id.as_str()).fetch_optional(pool).await,
            ArtifactDatabase::Postgres(pool) => sqlx::query_as::<_, ObjectRow>("SELECT principal,mime_type,size,expires_at,storage_backend,storage_endpoint,storage_bucket FROM artifacts WHERE id=$1 AND state='ready'").bind(id.as_str()).fetch_optional(pool).await,
        }.map_err(storage_error)?;
        row.ok_or(ArtifactError::NotFound)
    }

    pub(super) async fn read_guard(
        &self,
        id: &ArtifactId,
    ) -> Result<Arc<dyn ArtifactReadGuard>, ArtifactError> {
        if id.as_str().is_empty()
            || !id
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ArtifactError::NotFound);
        }
        match &self.database {
            ArtifactDatabase::Sqlite(_) => {
                let directory = self.root.join("locks");
                tokio::fs::create_dir_all(&directory)
                    .await
                    .map_err(storage_error)?;
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(directory.join(id.as_str()))
                    .map_err(storage_error)?;
                file.try_lock_shared().map_err(storage_error)?;
                Ok(Arc::new(file))
            }
            ArtifactDatabase::Postgres(_) => {
                let mut transaction = self
                    .lock_pool
                    .as_ref()
                    .expect("PostgreSQL lock pool")
                    .begin()
                    .await
                    .map_err(storage_error)?;
                sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 1))")
                    .bind(id.as_str())
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
                Ok(Arc::new(Mutex::new(transaction)))
            }
        }
    }

    pub(super) async fn open_authorized(
        &self,
        principal: Option<&Principal>,
        id: &ArtifactId,
        grant: Option<&str>,
    ) -> Result<ArtifactReader, ArtifactError> {
        // Acquire the physical hold before observing authorization. Cleanup takes the
        // corresponding exclusive lock before inspecting or deleting the object.
        let guard = self.read_guard(id).await?;
        let row = self.object_row(id).await?;
        if let Some(principal) = principal {
            if row.principal != principal.continuation_key() || row.expires_at <= self.now() {
                return Err(ArtifactError::NotFound);
            }
        } else {
            let token = grant.ok_or(ArtifactError::Unauthorized)?;
            if self.grant_artifact(token).await? != *id {
                return Err(ArtifactError::Unauthorized);
            }
        }
        let path = if row.storage_backend == "s3" {
            let settings = self.transfer_settings(None).await?;
            let s3 = settings.s3.as_ref().ok_or_else(|| {
                ArtifactError::Storage("S3 credentials are not configured".into())
            })?;
            if row.storage_endpoint.as_deref() != Some(s3.endpoint.as_str())
                || row.storage_bucket.as_deref() != Some(s3.bucket.as_str())
            {
                return Err(ArtifactError::Storage(
                    "Artifact belongs to a different S3 endpoint or bucket".into(),
                ));
            }
            let directory = self.root.join("objects");
            tokio::fs::create_dir_all(&directory)
                .await
                .map_err(storage_error)?;
            // Ownership starts before the first network await, so cancelling a
            // pending download removes its partial cache as well as completed reads.
            let cache = tempfile::Builder::new()
                .prefix(&format!("{}.read-", id.as_str()))
                .tempfile_in(directory)
                .map_err(storage_error)?
                .into_temp_path();
            let cache = Arc::new(CachedRead {
                path: cache,
                _hold: guard,
            });
            self.fetch_s3(s3, id, &cache.path, row.size as u64).await?;
            return Ok(ArtifactReader {
                artifact: ArtifactRef {
                    id: id.clone(),
                    mime_type: row.mime_type,
                    size: row.size as u64,
                },
                source: ArtifactSource::LocalPath(cache.path.to_path_buf()),
                guard: cache,
            });
        } else if row.storage_backend == "internal" {
            self.object_path(id.as_str())
        } else {
            return Err(ArtifactError::Storage(
                "unknown Artifact storage backend".into(),
            ));
        };
        Ok(ArtifactReader {
            artifact: ArtifactRef {
                id: id.clone(),
                mime_type: row.mime_type,
                size: row.size as u64,
            },
            source: ArtifactSource::LocalPath(path),
            guard,
        })
    }

    async fn grant_artifact(&self, token: &str) -> Result<ArtifactId, ArtifactError> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ArtifactError::Unauthorized);
        }
        let hash = sha256_hex(token.as_bytes());
        let id: Option<String> = match &self.database {
            ArtifactDatabase::Sqlite(pool) => sqlx::query_scalar("SELECT artifact_id FROM artifact_download_grants WHERE token_hash=? AND expires_at>?").bind(&hash).bind(self.now()).fetch_optional(pool).await,
            ArtifactDatabase::Postgres(pool) => sqlx::query_scalar("SELECT artifact_id FROM artifact_download_grants WHERE token_hash=$1 AND expires_at>$2").bind(&hash).bind(self.now()).fetch_optional(pool).await,
        }.map_err(storage_error)?;
        id.map(ArtifactId::new).ok_or(ArtifactError::Unauthorized)
    }

    pub(super) async fn open_grant(&self, token: &str) -> Result<ArtifactReader, ArtifactError> {
        let id = self.grant_artifact(token).await?;
        self.open_authorized(None, &id, Some(token)).await
    }

    pub(super) async fn issue_download(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        retention: Duration,
        settings: &ArtifactSettings,
    ) -> Result<ArtifactDownload, ArtifactError> {
        let settings = self.transfer_settings(Some(settings)).await?;
        let _guard = self.read_guard(id).await?;
        ArtifactStore::extend_retention(self, principal, id, retention).await?;
        let row = self.object_row(id).await?;
        let now = self.now();
        let mut expires_at = now.saturating_add(900_000);
        let token = hex_bytes(&rand::random::<[u8; 32]>());
        let url = if settings.external_signed_downloads && row.storage_backend == "s3" {
            let s3 = settings.s3.as_ref().ok_or_else(|| {
                ArtifactError::Storage("S3 credentials are not configured".into())
            })?;
            if row.storage_endpoint.as_deref() != Some(s3.endpoint.as_str())
                || row.storage_bucket.as_deref() != Some(s3.bucket.as_str())
            {
                return Err(ArtifactError::Storage(
                    "Artifact S3 storage configuration changed".into(),
                ));
            }
            if let Some(expiry) = s3.credentials_expires_at {
                expires_at = expires_at.min(expiry);
            }
            expires_at = expires_at.div_euclid(1000) * 1000;
            if expires_at.saturating_sub(now) < 300_000 {
                return Err(ArtifactError::Invalid(
                    "S3 credentials cannot provide five minutes of download validity".into(),
                ));
            }
            // The public endpoint is signed as-is, never substituted after signing.
            self.s3_url(
                s3,
                settings
                    .file_public_base_url
                    .as_deref()
                    .unwrap_or(&s3.endpoint),
                id,
                "GET",
                Duration::from_secs((expires_at.div_euclid(1000) - now.div_euclid(1000)) as u64),
                std::time::UNIX_EPOCH + Duration::from_millis(now as u64),
            )?
        } else {
            let base = if settings.external_signed_downloads {
                settings.file_public_base_url.as_deref().unwrap_or("")
            } else {
                &settings.client_base_url
            };
            if base.is_empty() {
                return Err(ArtifactError::Invalid(
                    "client base URL is not configured".into(),
                ));
            }
            format!(
                "{}/v1/artifacts/downloads/{token}",
                base.trim_end_matches('/')
            )
        };
        let hash = sha256_hex(token.as_bytes());
        let affected = match &self.database {
            ArtifactDatabase::Sqlite(pool) => sqlx::query("INSERT INTO artifact_download_grants(token_hash,artifact_id,expires_at) SELECT ?,id,? FROM artifacts WHERE id=? AND principal=? AND state='ready' AND expires_at>?").bind(&hash).bind(expires_at).bind(id.as_str()).bind(principal.continuation_key()).bind(self.now()).execute(pool).await.map(|result| result.rows_affected()),
            ArtifactDatabase::Postgres(pool) => sqlx::query("INSERT INTO artifact_download_grants(token_hash,artifact_id,expires_at) SELECT $1,id,$2 FROM artifacts WHERE id=$3 AND principal=$4 AND state='ready' AND expires_at>$5").bind(&hash).bind(expires_at).bind(id.as_str()).bind(principal.continuation_key()).bind(self.now()).execute(pool).await.map(|result| result.rows_affected()),
        }.map_err(storage_error)?;
        if affected != 1 {
            return Err(ArtifactError::NotFound);
        }
        if expires_at.saturating_sub(self.now()) < 300_000 {
            return Err(ArtifactError::Invalid(
                "download issuance cannot provide five minutes of validity".into(),
            ));
        }
        Ok(ArtifactDownload {
            artifact: ArtifactRef {
                id: id.clone(),
                mime_type: row.mime_type,
                size: row.size as u64,
            },
            url,
            expires_at,
        })
    }

    pub(super) async fn ingest_stream(
        &self,
        principal: &Principal,
        mime_type: &str,
        size: Option<u64>,
        bytes: ArtifactByteStream,
        retention: Duration,
    ) -> Result<ArtifactRef, ArtifactError> {
        let reserved = size.unwrap_or(MAX_ARTIFACT_BYTES);
        let upload = self
            .create_upload(
                principal,
                ArtifactUploadRequest {
                    mime_type: mime_type.to_owned(),
                    size: reserved,
                    idle_ttl: Duration::from_secs(900),
                    retention_ttl: retention,
                    policy: stravia_runtime_contract::agent::ArtifactPolicy {
                        max_artifacts: 1,
                        max_bytes: MAX_ARTIFACT_BYTES,
                        allowed_mime_types: vec!["*/*".into()],
                    },
                },
            )
            .await?;
        let result = async {
            let part = self
                .upload_part(principal, &upload.upload_id, &upload.upload_token, 1, bytes)
                .await?;
            if part.size == 0 {
                return Err(ArtifactError::Invalid("Artifact is empty".into()));
            }
            if size.is_none() {
                match &self.database {
                    ArtifactDatabase::Sqlite(pool) => {
                        let mut transaction = pool.begin().await.map_err(storage_error)?;
                        sqlx::query("UPDATE artifact_uploads SET declared_size=? WHERE id=?")
                            .bind(part.size as i64)
                            .bind(&upload.upload_id)
                            .execute(&mut *transaction)
                            .await
                            .map_err(storage_error)?;
                        sqlx::query("UPDATE artifacts SET size=? WHERE id=?")
                            .bind(part.size as i64)
                            .bind(upload.artifact_id.as_str())
                            .execute(&mut *transaction)
                            .await
                            .map_err(storage_error)?;
                        transaction.commit().await.map_err(storage_error)?;
                    }
                    ArtifactDatabase::Postgres(pool) => {
                        let mut transaction = pool.begin().await.map_err(storage_error)?;
                        sqlx::query("UPDATE artifact_uploads SET declared_size=$1 WHERE id=$2")
                            .bind(part.size as i64)
                            .bind(&upload.upload_id)
                            .execute(&mut *transaction)
                            .await
                            .map_err(storage_error)?;
                        sqlx::query("UPDATE artifacts SET size=$1 WHERE id=$2")
                            .bind(part.size as i64)
                            .bind(upload.artifact_id.as_str())
                            .execute(&mut *transaction)
                            .await
                            .map_err(storage_error)?;
                        transaction.commit().await.map_err(storage_error)?;
                    }
                }
            }
            self.complete_upload(principal, &upload.upload_id, &upload.upload_token, &[part])
                .await
        }
        .await;
        if let Err(original) = &result {
            // Failed automatic ingestion has no client upload task to resume.
            let cleanup = async {
                match tokio::fs::remove_dir_all(self.staging_dir(&upload.upload_id)).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(storage_error(error)),
                }
                self.delete_ready(principal, &upload.artifact_id).await
            }
            .await;
            if let Err(cleanup) = cleanup {
                return Err(ArtifactError::Storage(format!(
                    "{original}; staging cleanup also failed: {cleanup}"
                )));
            }
        }
        result
    }
}

#[derive(Debug)]
struct CachedRead {
    path: tempfile::TempPath,
    _hold: Arc<dyn ArtifactReadGuard>,
}

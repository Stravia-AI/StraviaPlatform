use super::*;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use std::time::SystemTime;

impl LocalArtifactStore {
    pub(super) fn s3_url(
        &self,
        settings: &ArtifactS3Settings,
        endpoint: &str,
        id: &ArtifactId,
        method: &str,
        lifetime: Duration,
        signing_time: SystemTime,
    ) -> Result<String, ArtifactError> {
        let mut url = reqwest::Url::parse(endpoint).map_err(storage_error)?;
        url.path_segments_mut()
            .map_err(|_| ArtifactError::Invalid("invalid S3 endpoint".into()))?
            .pop_if_empty()
            .push(&settings.bucket)
            .push("objects")
            .push(id.as_str());
        let credentials = Credentials::new(
            &settings.access_key_id,
            &settings.secret_access_key,
            settings.session_token.clone(),
            None,
            "stravia-artifacts",
        );
        let identity = credentials.into();
        let mut signing_settings = SigningSettings::default();
        signing_settings.signature_location = SignatureLocation::QueryParams;
        signing_settings.expires_in = Some(lifetime);
        let parameters = v4::SigningParams::builder()
            .identity(&identity)
            .region(&settings.region)
            .name("s3")
            .time(signing_time)
            .settings(signing_settings)
            .build()
            .map_err(storage_error)?
            .into();
        let signable = SignableRequest::new(
            method,
            url.as_str(),
            std::iter::empty(),
            SignableBody::UnsignedPayload,
        )
        .map_err(storage_error)?;
        let (instructions, _) = sign(signable, &parameters)
            .map_err(storage_error)?
            .into_parts();
        url.query_pairs_mut().extend_pairs(
            instructions
                .params()
                .iter()
                .map(|(key, value)| (*key, value.as_ref())),
        );
        Ok(url.into())
    }

    fn s3_client() -> Result<reqwest::Client, ArtifactError> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(900))
            .build()
            .map_err(storage_error)
    }

    pub(super) async fn upload_s3(
        &self,
        settings: &ArtifactS3Settings,
        id: &ArtifactId,
        path: &std::path::Path,
        size: u64,
        mime_type: &str,
    ) -> Result<(), ArtifactError> {
        let url = self.s3_url(
            settings,
            &settings.endpoint,
            id,
            "PUT",
            Duration::from_secs(900),
            SystemTime::now(),
        )?;
        let file = tokio::fs::File::open(path).await.map_err(storage_error)?;
        let stream = futures::stream::try_unfold(file, |mut file| async move {
            let mut buffer = vec![0; 64 * 1024];
            let count = file.read(&mut buffer).await?;
            buffer.truncate(count);
            Ok::<_, std::io::Error>((count != 0).then_some((Bytes::from(buffer), file)))
        });
        let response = Self::s3_client()?
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, size)
            .header(reqwest::header::CONTENT_TYPE, mime_type)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|_| ArtifactError::Storage("S3 upload transport failed".into()))?;
        if !response.status().is_success() {
            return Err(ArtifactError::Storage(format!(
                "S3 upload failed ({})",
                response.status()
            )));
        }
        Ok(())
    }

    pub(super) async fn fetch_s3(
        &self,
        settings: &ArtifactS3Settings,
        id: &ArtifactId,
        path: &std::path::Path,
        expected_size: u64,
    ) -> Result<(), ArtifactError> {
        let url = self.s3_url(
            settings,
            &settings.endpoint,
            id,
            "GET",
            Duration::from_secs(900),
            SystemTime::now(),
        )?;
        let response = Self::s3_client()?
            .get(url)
            .send()
            .await
            .map_err(|_| ArtifactError::Storage("S3 download transport failed".into()))?;
        if !response.status().is_success() {
            return Err(ArtifactError::Storage(format!(
                "S3 download failed ({})",
                response.status()
            )));
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(storage_error)?;
        }
        let result = async {
            // The caller already owns this existing temporary file. Open it
            // synchronously so a cancelled spawn-blocking create cannot recreate it.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .map_err(storage_error)?;
            let mut file = tokio::fs::File::from_std(file);
            let mut stream = response.bytes_stream();
            let mut size = 0_u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|_| ArtifactError::Storage("S3 download stream failed".into()))?;
                size = size.saturating_add(chunk.len() as u64);
                if size > expected_size || size > MAX_ARTIFACT_BYTES {
                    return Err(ArtifactError::Storage(
                        "S3 object exceeds Artifact size".into(),
                    ));
                }
                file.write_all(&chunk).await.map_err(storage_error)?;
            }
            if size != expected_size {
                return Err(ArtifactError::Storage("S3 object is incomplete".into()));
            }
            file.flush().await.map_err(storage_error)
        }
        .await;
        result
    }

    pub(super) async fn object_location(
        &self,
        artifact_id: &str,
    ) -> Result<Option<(String, Option<String>, Option<String>)>, ArtifactError> {
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => sqlx::query_as(
                "SELECT storage_backend,storage_endpoint,storage_bucket FROM artifacts WHERE id=?",
            )
            .bind(artifact_id)
            .fetch_optional(pool)
            .await,
            ArtifactDatabase::Postgres(pool) => sqlx::query_as(
                "SELECT storage_backend,storage_endpoint,storage_bucket FROM artifacts WHERE id=$1",
            )
            .bind(artifact_id)
            .fetch_optional(pool)
            .await,
        }
        .map_err(storage_error)
    }

    pub(super) async fn remove_object(&self, artifact_id: &str) -> Result<(), ArtifactError> {
        let location = self.object_location(artifact_id).await?;
        let settings = self.transfer_settings(None).await?;
        self.remove_object_at(artifact_id, location, &settings)
            .await
    }

    pub(super) async fn remove_object_at(
        &self,
        artifact_id: &str,
        backend: Option<(String, Option<String>, Option<String>)>,
        settings: &ArtifactSettings,
    ) -> Result<(), ArtifactError> {
        let id = ArtifactId::new(artifact_id);
        if let Some((backend, endpoint, bucket)) = backend {
            if backend == "s3" {
                let s3 = settings.s3.as_ref().ok_or_else(|| {
                    ArtifactError::Storage("S3 credentials are not configured".into())
                })?;
                if endpoint.as_deref() != Some(s3.endpoint.as_str())
                    || bucket.as_deref() != Some(s3.bucket.as_str())
                {
                    return Err(ArtifactError::Storage(
                        "Artifact belongs to a different S3 endpoint or bucket".into(),
                    ));
                }
                let url = self.s3_url(
                    s3,
                    &s3.endpoint,
                    &id,
                    "DELETE",
                    Duration::from_secs(900),
                    SystemTime::now(),
                )?;
                let response = Self::s3_client()?
                    .delete(url)
                    .send()
                    .await
                    .map_err(|_| ArtifactError::Storage("S3 delete transport failed".into()))?;
                if !response.status().is_success()
                    && response.status() != reqwest::StatusCode::NOT_FOUND
                {
                    return Err(ArtifactError::Storage(format!(
                        "S3 delete failed ({})",
                        response.status()
                    )));
                }
            }
        }
        match tokio::fs::remove_file(self.object_path(artifact_id)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(storage_error(error)),
        }
    }

    pub(super) async fn persist_s3_location(
        &self,
        id: &str,
        s3: &ArtifactS3Settings,
    ) -> Result<(), ArtifactError> {
        match &self.database {
            ArtifactDatabase::Sqlite(pool) => sqlx::query("UPDATE artifacts SET storage_backend='s3',storage_endpoint=?,storage_bucket=? WHERE id=?").bind(&s3.endpoint).bind(&s3.bucket).bind(id).execute(pool).await.map(|_|()),
            ArtifactDatabase::Postgres(pool) => sqlx::query("UPDATE artifacts SET storage_backend='s3',storage_endpoint=$1,storage_bucket=$2 WHERE id=$3").bind(&s3.endpoint).bind(&s3.bucket).bind(id).execute(pool).await.map(|_|()),
        }.map_err(storage_error)
    }
}

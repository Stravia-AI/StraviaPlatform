use crate::{Principal, agent::ArtifactPolicy};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

pub const MAX_ARTIFACT_BYTES: u64 = 100 * 1024 * 1024;
pub type ArtifactByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ArtifactError>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(String);

impl ArtifactId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_reference(reference: &str) -> Result<Self, ArtifactError> {
        let identity = reference
            .split_once('?')
            .map_or(reference, |(identity, _)| identity);
        let id = identity
            .strip_prefix("https://stravia/artifact/")
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            })
            .ok_or_else(|| ArtifactError::Invalid("invalid Artifact Reference".into()))?;
        if reference.contains('#') {
            return Err(ArtifactError::Invalid(
                "Artifact Reference fragments are not supported".into(),
            ));
        }
        Ok(Self::new(id))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub mime_type: String,
    pub size: u64,
}

impl ArtifactRef {
    pub fn reference(&self) -> String {
        format!("https://stravia/artifact/{}", self.id.as_str())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactSettings {
    pub client_base_url: String,
    pub external_signed_downloads: bool,
    pub file_public_base_url: Option<String>,
    pub upload_prompt_injection: bool,
    pub s3: Option<ArtifactS3Settings>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactS3Settings {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub credentials_expires_at: Option<i64>,
}

impl std::fmt::Debug for ArtifactS3Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactS3Settings")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

impl ArtifactSettings {
    pub fn validate_for_save(&self) -> Result<(), ArtifactError> {
        if self.client_base_url.is_empty() {
            return Err(ArtifactError::Invalid("client base URL is required".into()));
        }
        self.validate()
    }

    pub fn validate(&self) -> Result<(), ArtifactError> {
        fn base(value: &str) -> Result<(), ArtifactError> {
            let url = reqwest::Url::parse(value)
                .map_err(|_| ArtifactError::Invalid("invalid file access base URL".into()))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(ArtifactError::Invalid("file access base must be an HTTP(S) URL without credentials, query or fragment".into()));
            }
            Ok(())
        }
        if !self.client_base_url.is_empty() {
            base(&self.client_base_url)?;
        }
        if self.upload_prompt_injection && self.client_base_url.is_empty() {
            return Err(ArtifactError::Invalid(
                "client base URL is required for upload assistance".into(),
            ));
        }
        if let Some(public) = &self.file_public_base_url {
            if !public.is_empty() {
                base(public)?;
            }
        }
        if self.external_signed_downloads {
            base(
                self.file_public_base_url
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        ArtifactError::Invalid("file public base URL is required".into())
                    })?,
            )?;
        }
        if let Some(s3) = &self.s3 {
            base(&s3.endpoint)?;
            if [
                &s3.region,
                &s3.bucket,
                &s3.access_key_id,
                &s3.secret_access_key,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
                || s3.bucket.contains('/')
            {
                return Err(ArtifactError::Invalid(
                    "S3 region, bucket and credentials are required".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDownload {
    pub artifact: ArtifactRef,
    pub url: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactUpload {
    pub upload_id: String,
    pub artifact_id: ArtifactId,
    pub upload_token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone)]
pub struct ArtifactUploadRequest {
    pub mime_type: String,
    pub size: u64,
    pub idle_ttl: Duration,
    pub retention_ttl: Duration,
    pub policy: ArtifactPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedArtifactPart {
    pub part_number: u32,
    pub etag: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub enum ArtifactSource {
    LocalPath(PathBuf),
    HttpsUrl(String),
}

#[derive(Debug, Clone)]
pub struct ArtifactReader {
    pub artifact: ArtifactRef,
    pub source: ArtifactSource,
    /// Retain for the full read, including lazy streaming of `source`.
    pub guard: std::sync::Arc<dyn ArtifactReadGuard>,
}

pub trait ArtifactReadGuard: std::fmt::Debug + Send + Sync {}
impl<T: std::fmt::Debug + Send + Sync> ArtifactReadGuard for T {}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ArtifactError {
    #[error("invalid Artifact: {0}")]
    Invalid(String),
    #[error("Artifact is not available")]
    NotFound,
    #[error("Artifact access is denied")]
    Forbidden,
    #[error("Artifact upload authentication failed")]
    Unauthorized,
    #[error("Artifact storage failed: {0}")]
    Storage(String),
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn configure(&self, settings: &ArtifactSettings) -> Result<(), ArtifactError>;
    async fn ingest(
        &self,
        principal: &Principal,
        mime_type: &str,
        size: Option<u64>,
        bytes: ArtifactByteStream,
        retention: Duration,
    ) -> Result<ArtifactRef, ArtifactError>;
    async fn download(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        retention: Duration,
        settings: &ArtifactSettings,
    ) -> Result<ArtifactDownload, ArtifactError>;
    async fn read_download(&self, token: &str) -> Result<ArtifactReader, ArtifactError>;
    async fn read_bytes(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        retention: Duration,
    ) -> Result<(ArtifactRef, Bytes), ArtifactError>;
    async fn create_upload(
        &self,
        principal: &Principal,
        request: ArtifactUploadRequest,
    ) -> Result<ArtifactUpload, ArtifactError>;

    async fn upload_part(
        &self,
        principal: &Principal,
        upload_id: &str,
        upload_token: &str,
        part_number: u32,
        bytes: ArtifactByteStream,
    ) -> Result<UploadedArtifactPart, ArtifactError>;

    async fn complete_upload(
        &self,
        principal: &Principal,
        upload_id: &str,
        upload_token: &str,
        parts: &[UploadedArtifactPart],
    ) -> Result<ArtifactRef, ArtifactError>;

    async fn open(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<ArtifactReader, ArtifactError>;
    async fn extend_retention(
        &self,
        principal: &Principal,
        id: &ArtifactId,
        retention: Duration,
    ) -> Result<(), ArtifactError>;

    async fn sweep_expired(&self) -> Result<u64, ArtifactError>;
}

pub fn bytes_stream(bytes: Bytes) -> ArtifactByteStream {
    Box::pin(futures::stream::once(async move { Ok(bytes) }))
}

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgPool, SqlitePool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::ArtifactPolicy;
use crate::hook::Principal;

pub const MAX_ARTIFACT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PRINCIPAL_STAGING_BYTES: u64 = 4 * MAX_ARTIFACT_BYTES;
const MAX_PRINCIPAL_STAGING_UPLOADS: i64 = 16;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub mime_type: String,
    pub size: u64,
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
}

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

mod quota;
mod store;

use quota::*;
pub use store::LocalArtifactStore;

pub fn bytes_stream(bytes: Bytes) -> ArtifactByteStream {
    Box::pin(futures::stream::once(async move { Ok(bytes) }))
}

#[cfg(test)]
mod tests;

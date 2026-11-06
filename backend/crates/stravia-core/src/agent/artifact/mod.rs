use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, SqlitePool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedMutexGuard};

use stravia_runtime_contract::{
    Principal,
    agent::ArtifactPolicy,
    identifier::{encode_digest, new_id, valid_digest_id, valid_id},
};

pub use stravia_runtime_contract::artifact::*;
const MAX_PRINCIPAL_STAGING_BYTES: u64 = 4 * MAX_ARTIFACT_BYTES;
const MAX_PRINCIPAL_STAGING_UPLOADS: i64 = 16;

pub(crate) const ARTIFACT_STORAGE_FAILURE_MESSAGE: &str = "Artifact storage failed";

/// 内部暂存任务；单请求上传与内部 ingest 都只通过该表示驱动存储层。
#[derive(Debug, Clone)]
pub(super) struct ArtifactUpload {
    pub upload_id: String,
    pub upload_token: String,
}

#[derive(Debug, Clone)]
pub(super) struct ArtifactUploadRequest {
    pub mime_type: String,
    pub size: u64,
    pub idle_ttl: Duration,
    pub retention_ttl: Duration,
    pub policy: ArtifactPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UploadedArtifactPart {
    pub part_number: u32,
    pub etag: String,
    pub size: u64,
}

pub(crate) struct ArtifactErrorMapping {
    pub(crate) status: u16,
    pub(crate) public_message: String,
    pub(crate) diagnostic_message: String,
}

pub(crate) fn artifact_error_mapping(error: &ArtifactError) -> ArtifactErrorMapping {
    let status = match error {
        ArtifactError::Invalid(_) => 400,
        ArtifactError::NotFound => 404,
        ArtifactError::Forbidden => 403,
        ArtifactError::Unauthorized => 401,
        ArtifactError::Storage(_) => 500,
    };
    let public_message = match error {
        ArtifactError::Storage(_) => ARTIFACT_STORAGE_FAILURE_MESSAGE.to_owned(),
        _ => error.to_string(),
    };
    let diagnostic_message = match error {
        ArtifactError::Storage(reason) => {
            let reason = crate::interaction_observation::redact_text(reason);
            format!("Artifact storage failed: {reason}")
        }
        _ => public_message.clone(),
    };
    ArtifactErrorMapping {
        status,
        public_message,
        diagnostic_message,
    }
}

mod quota;
mod store;

use quota::*;
pub use store::LocalArtifactStore;

#[cfg(test)]
mod tests;

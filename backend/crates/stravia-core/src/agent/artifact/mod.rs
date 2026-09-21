use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgPool, SqlitePool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[cfg(test)]
use stravia_runtime_contract::agent::ArtifactPolicy;
use stravia_runtime_contract::{
    Principal,
    identifier::{encode_digest, new_id, valid_digest_id, valid_id},
};

pub use stravia_runtime_contract::artifact::*;
const MAX_PRINCIPAL_STAGING_BYTES: u64 = 4 * MAX_ARTIFACT_BYTES;
const MAX_PRINCIPAL_STAGING_UPLOADS: i64 = 16;

pub(crate) const ARTIFACT_STORAGE_FAILURE_MESSAGE: &str = "Artifact storage failed";

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

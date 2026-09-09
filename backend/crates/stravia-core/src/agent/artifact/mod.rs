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

use stravia_runtime_contract::Principal;
#[cfg(test)]
use stravia_runtime_contract::agent::ArtifactPolicy;

use stravia_runtime_contract::artifact::*;
const MAX_PRINCIPAL_STAGING_BYTES: u64 = 4 * MAX_ARTIFACT_BYTES;
const MAX_PRINCIPAL_STAGING_UPLOADS: i64 = 16;

mod quota;
mod store;

use quota::*;
pub use store::LocalArtifactStore;

#[cfg(test)]
mod tests;

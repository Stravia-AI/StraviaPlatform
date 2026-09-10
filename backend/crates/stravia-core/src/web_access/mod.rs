use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod platform;
use crate::db::models::{WebAccessSettings, WebProvider};
use crate::storage::traits::WebAccessApiKeyPermissions;
type RuntimeConfig = (
    WebAccessSettings,
    HashMap<String, WebProvider>,
    WebAccessApiKeyPermissions,
);

pub(crate) use platform::{decode_query_url, internal_platform_tools};
use stravia_web_access_contract::WebAccessError;

mod types;
pub use stravia_web_access_contract::{
    FetchRequest, FetchResult, FetchStatus, SearchRequest, SearchResponse, WebAccessErrorCode,
    WebAccessPublicError,
};
#[cfg(test)]
use stravia_web_access_contract::{SearchMode, SearchResult};
pub use types::*;

mod engine;
mod policy;
mod service;

#[cfg(test)]
use engine::{AdapterSuccess, ProviderFailure, ProviderUsage, WebAccessEngine, WebProviderAdapter};
#[cfg(test)]
use policy::{validate_fetch_request, validate_search_request};
#[cfg(test)]
use service::WebAccessAvailability;
pub(crate) use service::WebAccessRunSnapshotStore;
pub use service::WebAccessService;

#[cfg(test)]
mod tests;

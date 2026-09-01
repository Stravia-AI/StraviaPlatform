use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod platform;
mod providers;
use crate::db::models::{WebAccessSettings, WebProvider};
use crate::storage::traits::WebAccessApiKeyPermissions;
type RuntimeConfig = (
    WebAccessSettings,
    HashMap<String, WebProvider>,
    WebAccessApiKeyPermissions,
);

pub(crate) use platform::{WEB_FETCH_TOOL_ID, WEB_SEARCH_TOOL_ID, internal_platform_tools};

mod types;
pub use types::*;

mod engine;
mod policy;
mod service;
mod ssrf;

#[cfg(test)]
use engine::{AdapterSuccess, ProviderFailure, ProviderUsage, WebAccessEngine, WebProviderAdapter};
pub(crate) use policy::normalize_domains;
#[cfg(test)]
use policy::{validate_fetch_request, validate_search_request};
#[cfg(test)]
use service::WebAccessAvailability;
pub(crate) use service::WebAccessRunSnapshotStore;
pub use service::WebAccessService;
pub(crate) use ssrf::is_public_ip;

#[cfg(test)]
mod tests;

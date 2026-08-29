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

pub use engine::WebAccessService;
#[cfg(test)]
use engine::{
    AdapterSuccess, ProviderFailure, ProviderUsage, WebAccessAvailability, WebAccessEngine,
    WebProviderAdapter, validate_fetch_request, validate_search_request,
};
pub(crate) use engine::{WebAccessRunSnapshotStore, is_public_ip, normalize_domains};

#[cfg(test)]
mod tests;

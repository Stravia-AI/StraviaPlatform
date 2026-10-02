mod config;
pub(crate) mod runtime;
pub use config::{DestinationRpmLimit, RpmConfig, RpmPool, SETTINGS_KEY};
pub(crate) use runtime::{
    PoolKey, RootRequest, SendAdmission, TargetAdmission, current_root_request, scope_root_request,
};
pub(crate) static CONFIG_WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

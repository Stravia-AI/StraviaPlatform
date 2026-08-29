mod codex;
mod config;
mod local;
mod mcp;
mod platform;
mod runner;
mod types;
mod validator;

pub(crate) use mcp::tools as mcp_tools;
#[cfg(test)]
pub(crate) use platform::PUBLIC_WEB_SEARCH_TOOL_ID;
pub(crate) use platform::{builtin_extensions, execute as execute_public_search, input_schema};
pub use runner::{WebSearchEventStream, WebSearchRunner};
pub use types::*;
pub use validator::SearchReportValidator;

pub(crate) use codex::{CodexAgenticSearchBackend, codex_provider_contract};
#[cfg(test)]
pub(crate) use config::MemoryWebSearchConfigStore;
pub(crate) use config::{
    MAX_SEARCH_SECONDS, MAX_SEARCH_TURNS, MIN_SEARCH_SECONDS, MIN_SEARCH_TURNS,
    SettingsWebSearchConfigStore, WebSearchConfigStore, resolve_enabled_config,
};
pub(crate) use local::{
    LOCAL_SEARCH_DEFINITION_ID, LOCAL_SEARCH_DEFINITION_REVISION, LocalSearchBackend,
    LocalSearchEvidenceStore, LocalSearchOutputValidator, local_search_definition,
};
pub(crate) use platform::native_web_search_requested;
#[cfg(test)]
pub(crate) use runner::{AllowSearchRun, LocalSearchLimits};
pub(crate) use runner::{BackendOutput, SearchBackend, SearchBackendInput, SearchRunAuthorizer};

#[cfg(test)]
mod tests;

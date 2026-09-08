pub mod admin;
mod admission;
pub mod agent;
pub mod auth;
mod compaction;
pub mod config;
pub mod connect_client_apply;
pub mod db;
pub mod error;
mod gateway;
pub(crate) mod generation_chain;
pub mod history_marker;
pub mod hook;
mod interaction_observation;
pub mod mcp;
pub(crate) mod media;
mod migrations;
pub(crate) mod model_turn;
pub mod plugin;
pub mod protocol;
pub mod provider;
pub mod provider_catalog;
pub mod provider_models;
pub mod proxy;
pub(crate) mod reversible_redaction;
pub mod router;
pub mod storage;
pub mod thinking;
pub mod turn_chain;
pub(crate) mod web_access;
pub mod web_search;

#[cfg(test)]
use config::GatewayConfig;

pub use gateway::{CapabilityCacheEntry, Gateway, GatewayBuilder, RuntimeStorageKind};
pub(crate) use gateway::{HistoryMarkerExecutionJob, StartedHistoryMarkerExecution};

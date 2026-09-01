pub mod admin;
mod admission;
pub mod agent;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
mod gateway;
pub(crate) mod generation_chain;
pub mod history_marker;
pub mod hook;
pub mod logging;
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
pub mod router;
pub mod storage;
pub mod thinking;
pub mod turn_chain;
pub(crate) mod web_access;
pub mod web_search;
#[cfg(debug_assertions)]
mod wire_capture;

#[cfg(test)]
use config::GatewayConfig;

pub use gateway::{CapabilityCacheEntry, Gateway, GatewayBuilder, RuntimeStorageKind};
pub(crate) use gateway::{HistoryMarkerExecutionJob, StartedHistoryMarkerExecution};

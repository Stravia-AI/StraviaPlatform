//! Host side of the base vendor's runtime Provider Catalog.
//!
//! The base guest owns catalog fetching, validation, and profile derivation
//! through the `sync-catalog` export. This module feeds it a network scoped to
//! the configured catalog origin, persists the returned last-good snapshot and
//! provider scope bodies, and swaps the vendor's effective descriptor set.
//! Catalog synchronization never receives provider credentials, private state,
//! WebSockets, or model events.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LogLevel,
    OperationScope, RuntimeError, RuntimeEvent,
};
use stravia_vendor_sdk::{CatalogSyncOutcome, CatalogSyncRequest, ErrorKind};
use tokio::sync::Mutex;

use super::lifecycle::VendorOperation;
use super::manager::VendorPlugins;
use super::network::VendorNetwork;
use crate::provider_catalog::{
    CatalogError, CatalogModelList, CatalogModelSource, CatalogProviderScope,
    CatalogRefreshSummary, ProviderCatalog,
};

/// Sync does up to four sequential catalog fetches under one deadline.
const SYNC_DEADLINE: Duration = Duration::from_secs(90);
const SYNC_PROTOCOL: &str = "catalog-sync";

/// Serializes catalog synchronization and owns the host orchestration around
/// it. `VendorOperation` fencing still applies: a base plugin update drains or
/// rejects catalog sync admission like any other vendor operation.
#[derive(Clone)]
pub struct VendorCatalogSync {
    plugins: Arc<VendorPlugins>,
    catalog: ProviderCatalog,
    /// `None` fails remote requests closed — a gateway without a configured
    /// catalog origin never reaches the production service.
    catalog_base_url: Option<String>,
    http: reqwest::Client,
    websocket: reqwest::Client,
    lock: Arc<Mutex<()>>,
}

impl VendorCatalogSync {
    pub(crate) fn new(
        plugins: Arc<VendorPlugins>,
        catalog: ProviderCatalog,
        catalog_base_url: Option<String>,
        http: reqwest::Client,
        websocket: reqwest::Client,
    ) -> Self {
        Self {
            plugins,
            catalog,
            catalog_base_url,
            http,
            websocket,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Restore the effective profile set from the persisted last-good snapshot
    /// (or the guest's embedded list when nothing is persisted). No remote
    /// fetch happens here; failures only lose the runtime overlay.
    pub(crate) async fn bootstrap(&self) -> anyhow::Result<()> {
        let (snapshot_body, version) = self.catalog.provider_index_snapshot().await;
        let outcome = self
            .run(CatalogSyncRequest {
                catalog_base_url: self.catalog_base_url.clone().unwrap_or_default(),
                refresh_remote: false,
                snapshot_body: Some(snapshot_body),
                snapshot_revision: Some(version.revision),
                snapshot_generated_at: Some(version.generated_at),
                scope_provider_id: None,
            })
            .await?;
        // Profiles the catalog dropped while this instance was offline stay
        // retired so their saved connections keep executing after restart.
        let retired = self.catalog.retired_profiles().await;
        self.plugins
            .apply_catalog_overlay(outcome.providers, retired)?;
        Ok(())
    }

    /// Remote provider-index refresh plus the host-owned Canonical Model
    /// refresh. The two documents advance independently; a failure on either
    /// side keeps its last-good data and fails the reported summary.
    pub async fn refresh(&self) -> anyhow::Result<CatalogRefreshSummary> {
        let previous = self.catalog.provider_index_snapshot().await.1.revision;
        let index = self.sync_index().await;
        let canonical = self.catalog.refresh_canonical().await;
        let (outcome, canonical) = match (index, canonical) {
            (Ok(outcome), Ok(canonical)) => (outcome, canonical),
            (Err(index), Err(canonical)) => anyhow::bail!(
                "provider index refresh failed: {index:#}; canonical model refresh failed: {canonical:#}"
            ),
            (Err(index), _) => return Err(index),
            (_, Err(canonical)) => return Err(canonical),
        };
        let changed = outcome.revision != previous || canonical.changed;
        Ok(CatalogRefreshSummary {
            revision: outcome.revision,
            generated_at: outcome.generated_at.unwrap_or_default(),
            provider_count: self.catalog.provider_count().await,
            model_count: canonical.model_count,
            changed,
        })
    }

    /// Fetch `providers/{id}/models.json` through the guest, refreshing the
    /// provider index in the same confirmed revision when the cached scope is
    /// missing. A provider absent from the index or returning no scope is
    /// `ProviderNotFound`, never an empty success.
    pub(crate) async fn provider_scope(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<CatalogProviderScope> {
        if let Some(scope) = self.catalog.cached_provider_scope(provider_id).await? {
            return Ok(scope);
        }
        let _guard = self.lock.lock().await;
        if let Some(scope) = self.catalog.cached_provider_scope(provider_id).await? {
            return Ok(scope);
        }
        let outcome = self
            .sync_index_locked(Some(provider_id))
            .await
            .map_err(|error| scope_failure(provider_id, error))?;
        let body = outcome
            .scope_body
            .ok_or_else(|| scope_refresh(provider_id, "catalog sync returned no scope body"))?;
        self.catalog
            .install_provider_scope(provider_id, body.as_bytes())
            .await
            .map_err(|error| scope_refresh(provider_id, error.to_string()))
    }

    /// Provider Catalog model list for one catalog channel; the scope comes
    /// from the guest-owned store.
    pub async fn catalog_models(
        &self,
        provider_id: &str,
        channel_id: &str,
    ) -> anyhow::Result<CatalogModelList> {
        let scope = self.provider_scope(provider_id).await?;
        self.catalog.models(provider_id, channel_id, scope).await
    }

    /// Raw metadata for one Provider Catalog Entry, used by reimport.
    pub(crate) async fn catalog_model_source(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<CatalogModelSource> {
        let scope = self.provider_scope(provider_id).await?;
        self.catalog.model_source(provider_id, model_id, scope)
    }

    /// Full remote sync: the guest refreshes the index, the host persists it
    /// as last-good and swaps the effective descriptor set.
    async fn sync_index(&self) -> anyhow::Result<CatalogSyncOutcome> {
        let _guard = self.lock.lock().await;
        let outcome = self.sync_index_locked(None).await?;
        Ok(outcome)
    }

    /// Remote index sync while `lock` is held; `provider_id` additionally
    /// fetches that provider's scope document in the same confirmed revision.
    async fn sync_index_locked(
        &self,
        provider_id: Option<&str>,
    ) -> anyhow::Result<CatalogSyncOutcome> {
        let catalog_base_url = self
            .catalog_base_url
            .clone()
            .context("provider catalog remote origin is not configured")?;
        let (snapshot_body, version) = self.catalog.provider_index_snapshot().await;
        let outcome = self
            .run(CatalogSyncRequest {
                catalog_base_url,
                refresh_remote: true,
                snapshot_body: Some(snapshot_body),
                snapshot_revision: Some(version.revision),
                snapshot_generated_at: Some(version.generated_at),
                scope_provider_id: provider_id.map(str::to_owned),
            })
            .await?;
        self.install_index(&outcome).await?;
        Ok(outcome)
    }

    async fn install_index(&self, outcome: &CatalogSyncOutcome) -> anyhow::Result<()> {
        if let Some(body) = &outcome.snapshot_body {
            self.catalog
                .install_provider_index(
                    body,
                    outcome.revision.clone(),
                    outcome.generated_at.clone().unwrap_or_default(),
                )
                .await?;
        }
        let retired = self
            .plugins
            .apply_catalog_overlay(outcome.providers.clone(), Vec::new())?;
        self.catalog.persist_retired_profiles(&retired).await
    }

    /// Invoke the guest `sync-catalog` export under a catalog-origin-only
    /// operation. The store admits `http-start` and `log` only; state, events,
    /// and WebSockets stay unavailable and no provider identity reaches the
    /// guest through host imports.
    async fn run(&self, request: CatalogSyncRequest) -> anyhow::Result<CatalogSyncOutcome> {
        let (plugin, operation, _epoch) = self.plugins.acquire_package("base")?;
        let cancellation = CancellationToken::new();
        let deadline = Deadline::fixed(Instant::now() + SYNC_DEADLINE);
        let origins = self
            .catalog_base_url
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let network = VendorNetwork::new(
            self.http.clone(),
            self.websocket.clone(),
            origins,
            operation.clone(),
            cancellation.clone(),
            SYNC_PROTOCOL.to_owned(),
        );
        let services = Arc::new(CatalogSyncServices {
            network,
            operation,
            cancellation: cancellation.clone(),
        });
        let scope = OperationScope::new(services, cancellation, deadline, 0);
        let input = serde_json::to_vec(&request)?;
        let body = self
            .plugins
            .runtime
            .sync_catalog(&plugin, input, scope)
            .await?;
        serde_json::from_slice(&body).context("decode catalog sync outcome")
    }
}

/// Maps a `sync-catalog` failure to the catalog failure facts: a missing
/// provider stays `ProviderNotFound`; everything else is a scope refresh
/// failure that must not be recorded as a successful refresh.
fn scope_failure(provider_id: &str, error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::Plugin {
            kind: ErrorKind::ProviderNotFound,
            ..
        }) => CatalogError::ProviderNotFound {
            provider_id: provider_id.to_owned(),
        }
        .into(),
        _ => scope_refresh(provider_id, error.to_string()),
    }
}

fn scope_refresh(provider_id: &str, message: impl Into<String>) -> anyhow::Error {
    CatalogError::ScopeRefresh {
        provider_id: provider_id.to_owned(),
        message: message.into(),
    }
    .into()
}

/// Catalog-sync services: catalog-origin HTTP and logging only. Every
/// operation-scoped import (WebSocket, private state, model events) is denied
/// at the host boundary as well as by the runtime store.
struct CatalogSyncServices {
    network: VendorNetwork,
    operation: Arc<VendorOperation>,
    cancellation: CancellationToken,
}

impl CatalogSyncServices {
    fn ensure_current(&self) -> Result<(), HostFailure> {
        if self.cancellation.is_cancelled() {
            return Err(cancelled_failure());
        }
        self.operation
            .ensure_current()
            .map_err(|_| cancelled_failure())
    }

    fn unavailable(&self, capability: &str) -> HostFailure {
        HostFailure::new(
            ErrorKind::Unsupported,
            format!("{capability} is unavailable during catalog sync"),
        )
    }
}

#[async_trait]
impl HostServices for CatalogSyncServices {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        self.ensure_current()?;
        self.network.http_start(request)
    }

    async fn ws_connect(
        &self,
        _url: String,
        _headers: Vec<(String, String)>,
        _protocols: Vec<String>,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        Err(self.unavailable("WebSocket access"))
    }

    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Err(self.unavailable("private state"))
    }

    async fn write_private_state(&self, _bytes: Vec<u8>) -> Result<(), HostFailure> {
        Err(self.unavailable("private state"))
    }

    async fn emit_event(&self, _event: RuntimeEvent) -> Result<(), HostFailure> {
        Err(self.unavailable("model events"))
    }

    fn log(&self, level: LogLevel, message: &str) {
        let message = crate::interaction_observation::redact_text(message);
        match level {
            LogLevel::Debug => {
                tracing::debug!(target: "stravia::vendor", message = %message, "vendor catalog sync log")
            }
            LogLevel::Info => {
                tracing::info!(target: "stravia::vendor", message = %message, "vendor catalog sync log")
            }
            LogLevel::Warn => {
                tracing::warn!(target: "stravia::vendor", message = %message, "vendor catalog sync log")
            }
            LogLevel::Error => {
                tracing::error!(target: "stravia::vendor", message = %message, "vendor catalog sync log")
            }
        }
    }

    fn generation_is_current(&self, _generation: u64) -> bool {
        self.ensure_current().is_ok()
    }
}

fn cancelled_failure() -> HostFailure {
    HostFailure::new(ErrorKind::Cancelled, "vendor operation was cancelled")
}

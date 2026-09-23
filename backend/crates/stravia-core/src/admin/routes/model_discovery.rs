use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use thiserror::Error;

use super::*;
use crate::plugin::{VendorCallContext, VendorPublicationFence, VendorRequest};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DISCOVERY_PAGES: usize = 1024;

#[derive(Debug, Error)]
pub(crate) enum RouteModelDiscoveryError {
    #[error("Provider Model discovery setup failed for Provider {provider_id}: {source}")]
    DiscoverySetup {
        provider_id: String,
        #[source]
        source: anyhow::Error,
    },
    #[error(
        "Provider Model discovery returned an invalid or empty list for Provider {provider_id}"
    )]
    InvalidDiscoveryResponse { provider_id: String },
}

impl RouteModelDiscoveryError {
    fn setup(provider_id: &str, source: anyhow::Error) -> Self {
        Self::DiscoverySetup {
            provider_id: provider_id.to_string(),
            source,
        }
    }
}

pub(super) struct DiscoveredModels {
    pub(super) models: Vec<stravia_vendor_sdk::DiscoveredModel>,
    publications: Vec<VendorPublicationFence>,
}

impl DiscoveredModels {
    /// Validate every page, then acquire the shared vendor publication fence
    /// before publishing a combined inventory. An incompatible update
    /// invalidates old pages and no partial or late model set reaches storage.
    pub(super) async fn write_fence(
        &self,
    ) -> anyhow::Result<tokio::sync::OwnedRwLockReadGuard<()>> {
        // Every page belongs to the same vendor activity. Validate them all,
        // then take one read guard; taking several sequential guards can lock
        // invert with a queued incompatible-update writer.
        for publication in &self.publications {
            publication.ensure_current()?;
        }
        self.publications
            .first()
            .ok_or_else(|| anyhow::anyhow!("model discovery produced no publication fence"))?
            .write_fence()
            .await
    }
}

pub(super) async fn discover_provider_models(
    admin: &AdminService,
    provider_id: &str,
) -> Result<DiscoveredModels, RouteModelDiscoveryError> {
    let cancellation = stravia_runtime_contract::CancellationToken::new();
    let deadline = stravia_runtime_contract::Deadline::fixed(Instant::now() + DISCOVERY_TIMEOUT);
    let prepared = admin
        .gw
        .prepare_vendor_execution(
            provider_id,
            None,
            stravia_vendor_sdk::Operation::Discover,
            &VendorCallContext::new(cancellation.clone(), deadline.clone()),
        )
        .await
        .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?;
    let channel_id = prepared.provider().channel.trim();
    let channel = prepared
        .descriptor()
        .channels
        .iter()
        .find(|channel| channel.id == channel_id)
        .ok_or_else(|| {
            RouteModelDiscoveryError::setup(
                provider_id,
                anyhow::anyhow!("Vendor channel `{channel_id}` is not installed"),
            )
        })?;
    if !channel
        .capabilities
        .contains(&stravia_vendor_sdk::Capability::ModelDiscovery)
    {
        return Err(RouteModelDiscoveryError::setup(
            provider_id,
            anyhow::anyhow!("Vendor channel `{channel_id}` does not support model discovery"),
        ));
    }

    let mut models = BTreeMap::new();
    let mut publications = Vec::new();
    let mut seen_cursors = HashSet::new();
    let mut cursor = None;

    for _ in 0..MAX_DISCOVERY_PAGES {
        let execution = match admin
            .gw
            .execute_prepared_vendor(
                prepared.clone(),
                VendorRequest::Discover(stravia_vendor_sdk::DiscoverRequest {
                    cursor: cursor.clone(),
                }),
                VendorCallContext::new(cancellation.clone(), deadline.clone()),
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) => {
                // ADR-0073：Discover 与 Infer 携带同一份凭据，上游拒绝同样
                // 算失效证据；按准备时锁定的代际条件写。
                if crate::plugin::execution::is_credential_rejection(&error) {
                    admin
                        .gw
                        .mark_provider_credential_invalid(
                            provider_id,
                            prepared.credential_version(),
                        )
                        .await;
                }
                return Err(RouteModelDiscoveryError::setup(provider_id, error));
            }
        };
        let response = match execution.output {
            stravia_vendor_sdk::OperationOutput::Discover(response) => response,
            _ => {
                return Err(RouteModelDiscoveryError::setup(
                    provider_id,
                    anyhow::anyhow!("Vendor returned a non-discovery result"),
                ));
            }
        };
        publications.push(execution.publication);

        for model in response.models {
            let id = model.id.trim();
            if id.is_empty() || model.display_name.trim().is_empty() {
                return Err(RouteModelDiscoveryError::InvalidDiscoveryResponse {
                    provider_id: provider_id.to_owned(),
                });
            }
            match models.entry(id.to_owned()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(model);
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    return Err(RouteModelDiscoveryError::setup(
                        provider_id,
                        anyhow::anyhow!(
                            "Vendor model discovery returned duplicate model `{}`",
                            entry.key()
                        ),
                    ));
                }
            }
        }

        cursor = response
            .next_cursor
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let Some(next) = cursor.as_ref() else {
            if models.is_empty() {
                return Err(RouteModelDiscoveryError::InvalidDiscoveryResponse {
                    provider_id: provider_id.to_owned(),
                });
            }
            return Ok(DiscoveredModels {
                models: models.into_values().collect(),
                publications,
            });
        };
        if !seen_cursors.insert(next.clone()) {
            return Err(RouteModelDiscoveryError::setup(
                provider_id,
                anyhow::anyhow!("Vendor model discovery repeated its pagination cursor"),
            ));
        }
    }

    Err(RouteModelDiscoveryError::setup(
        provider_id,
        anyhow::anyhow!("Vendor model discovery exceeded {MAX_DISCOVERY_PAGES} pages"),
    ))
}

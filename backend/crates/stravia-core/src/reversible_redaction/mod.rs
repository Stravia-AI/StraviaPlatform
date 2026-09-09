//! Core settings and observation adapters for the credential protection capability.
use std::sync::Arc;

use crate::interaction_observation::{RunEvent, RunObserver};
use crate::storage::DynStorage;
use stravia_credential_protection::store::{Mapping, MappingStore};
use stravia_credential_protection::{CredentialDiscovery, RedactionHost, RedactionObserver};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::model_turn::CanonicalEventStream;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::redaction::{RedactionError, RedactionTrace};

struct SettingsHost(DynStorage);

#[async_trait::async_trait]
impl RedactionHost for SettingsHost {
    async fn enabled(&self) -> Result<bool, RedactionError> {
        match self
            .0
            .settings()
            .get(stravia_credential_protection::SETTING_KEY)
            .await
        {
            Ok(None) => Ok(false),
            Ok(Some(value)) if value == "false" => Ok(false),
            Ok(Some(value)) if value == "true" => Ok(true),
            _ => Err(RedactionError::Storage),
        }
    }
}

struct ObservationHost(RunObserver);

impl RedactionObserver for ObservationHost {
    fn protect_secrets(&self, mappings: &[Mapping]) {
        self.0
            .protect_secrets(mappings.iter().map(|mapping| mapping.secret.as_str()));
    }

    fn mappings_created(&self, discoveries: Vec<CredentialDiscovery>) {
        self.0.record(RunEvent::CredentialMappingsCreated {
            discoveries: discoveries
                .into_iter()
                .map(
                    |discovery| crate::interaction_observation::CredentialDiscovery {
                        rule_ids: discovery.rule_ids,
                        source_types: discovery.source_types,
                    },
                )
                .collect(),
        });
    }
}

#[derive(Clone)]
pub(crate) struct ReversibleRedaction {
    capability: stravia_credential_protection::ReversibleRedaction,
    pub(crate) mappings: Arc<dyn MappingStore>,
}

impl ReversibleRedaction {
    pub(crate) fn new(storage: DynStorage, mappings: Arc<dyn MappingStore>) -> Self {
        Self {
            capability: stravia_credential_protection::ReversibleRedaction::new(
                Arc::new(SettingsHost(storage)),
                mappings.clone(),
            ),
            mappings,
        }
    }

    pub(crate) async fn protect(
        &self,
        principal: &Principal,
        request: &mut AiRequest,
        observer: Option<&RunObserver>,
    ) -> Result<Vec<Mapping>, RedactionError> {
        let observer = observer.map(|observer| {
            Arc::new(ObservationHost(observer.clone())) as Arc<dyn RedactionObserver>
        });
        self.capability.protect(principal, request, observer).await
    }

    pub(crate) async fn publish(
        &self,
        principal: &Principal,
        trace: &RedactionTrace,
    ) -> Result<(), RedactionError> {
        self.capability.publish(principal, trace).await
    }

    pub(crate) fn restore_stream(
        &self,
        input: CanonicalEventStream,
        mappings: Vec<Mapping>,
        trace: RedactionTrace,
    ) -> CanonicalEventStream {
        self.capability.restore_stream(input, mappings, trace)
    }
}

#[cfg(test)]
mod mapping_tests;

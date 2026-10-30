//! Core settings and observation adapters for the credential protection capability.
use std::sync::Arc;

use crate::interaction_observation::{RunObserver, RunPublicationGuard};
use crate::storage::DynStorage;
use stravia_credential_protection::store::{Mapping, MappingStore};
use stravia_credential_protection::{
    CredentialDiscovery, RedactionHost, RedactionObserver, SqlCustomRuleStore,
};
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

struct ObservationHost(RunPublicationGuard);

fn observed(
    discoveries: Vec<CredentialDiscovery>,
) -> Vec<crate::interaction_observation::CredentialDiscovery> {
    discoveries
        .into_iter()
        .map(
            |discovery| crate::interaction_observation::CredentialDiscovery {
                rule_ids: discovery.rule_ids,
                source_types: discovery.source_types,
                locators: discovery
                    .locators
                    .into_iter()
                    .map(
                        |locator| crate::interaction_observation::CredentialLocator {
                            rule_id: locator.rule_id,
                            source_type: locator.source_type,
                            tool_id: locator.tool_id,
                            latest_input: locator.latest_input,
                        },
                    )
                    .collect(),
                omitted_locators: 0,
                // 占位符对所有 Run 记录，命中记录据此关联后续复用。
                marker: Some(discovery.marker),
            },
        )
        .collect()
}

impl RedactionObserver for ObservationHost {
    fn protect_secrets(&self, mappings: &[Mapping]) {
        self.0
            .protect_secrets(mappings.iter().map(|mapping| mapping.secret.as_str()));
    }

    fn mappings_created(&self, discoveries: Vec<CredentialDiscovery>) {
        self.0.credential_mappings_created(observed(discoveries));
    }

    fn mappings_reused(&self, discoveries: Vec<CredentialDiscovery>) {
        self.0.credential_mappings_reused(observed(discoveries));
    }
}

fn observation_host(observer: Option<&RunObserver>) -> Option<Arc<dyn RedactionObserver>> {
    observer
        .and_then(RunObserver::publication_guard)
        .map(|publication| Arc::new(ObservationHost(publication)) as Arc<dyn RedactionObserver>)
}

#[derive(Clone)]
pub(crate) struct ReversibleRedaction {
    capability: stravia_credential_protection::ReversibleRedaction,
    pub(crate) mappings: Arc<dyn MappingStore>,
    pub(crate) custom_rules: Arc<SqlCustomRuleStore>,
}

impl ReversibleRedaction {
    pub(crate) fn new(
        storage: DynStorage,
        mappings: Arc<dyn MappingStore>,
        custom_rules: Arc<SqlCustomRuleStore>,
    ) -> Self {
        Self {
            capability: stravia_credential_protection::ReversibleRedaction::new(
                Arc::new(SettingsHost(storage)),
                mappings.clone(),
                custom_rules.clone(),
            ),
            mappings,
            custom_rules,
        }
    }

    pub(crate) async fn test_text(
        &self,
        text: String,
    ) -> Result<Vec<stravia_credential_protection::CredentialMatch>, RedactionError> {
        self.capability.test_text(text).await
    }

    pub(crate) async fn locate_text(
        &self,
        text: String,
        secret: Option<&str>,
    ) -> Result<Vec<stravia_credential_protection::CredentialTextMatch>, RedactionError> {
        self.capability
            .locate_text(text, secret.map(str::to_owned))
            .await
    }

    pub(crate) async fn protect(
        &self,
        principal: &Principal,
        request: &mut Arc<AiRequest>,
        observer: Option<&RunObserver>,
    ) -> Result<Vec<Mapping>, RedactionError> {
        self.capability
            .protect(principal, request, observation_host(observer))
            .await
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
        observer: Option<&RunObserver>,
    ) -> CanonicalEventStream {
        self.capability
            .restore_stream(input, mappings, trace, observation_host(observer))
    }
}

#[cfg(test)]
mod custom_rule_tests;
#[cfg(test)]
mod mapping_tests;

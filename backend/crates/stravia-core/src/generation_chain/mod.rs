use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::protocol::transform::ProtocolTransform;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::OpenResponsesExt;
use stravia_runtime_contract::protocol::ir::ProtocolExt;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::request::VERIFIED_HISTORY_REPLAY_META;
use stravia_runtime_contract::turn_chain::ReusablePrefixMetadata;
use stravia_runtime_contract::turn_chain::ReusablePrefixQuery;
use stravia_runtime_contract::turn_chain::TurnChainStore;
use stravia_runtime_contract::turn_chain::TurnCommit;
use stravia_runtime_contract::turn_chain::TurnCommitError;
use stravia_runtime_contract::turn_chain::TurnNodeId;
use stravia_runtime_contract::turn_chain::TurnNodeKind;

mod materialize;
mod project;
mod store;
mod write;

use materialize::*;
use project::*;
use store::*;

pub(crate) use project::{
    generation_node_is_completed, generation_session_fingerprint, project_client_history,
    set_generation_session_id,
};
pub(crate) use store::{
    hydrate_response_artifact_references, request_has_item_references,
    request_preserves_upstream_response,
};

// Version 5 reserves tool-result semantics metadata; earlier vendor meta is untrusted.
const RESPONSE_PAYLOAD_VERSION: u32 = 6;
const LEGACY_RESPONSE_PAYLOAD_VERSION: u32 = 1;
const GENERATION_MATERIALIZATION_CACHE_BYTES: usize = 64 * 1024 * 1024;
const GENERATION_SESSION_ID_META: &str = "__stravia_generation_session_id";

#[cfg(test)]
const DEFAULT_GENERATION_CHAIN_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone)]
pub(crate) struct GenerationChain {
    store: GenerationChainStore,
    artifacts: Option<Arc<dyn stravia_runtime_contract::artifact::ArtifactStore>>,
    history_markers: Option<Arc<dyn crate::history_marker::HistoryMarkerStore>>,
    redaction_mappings: Option<Arc<dyn stravia_credential_protection::store::MappingStore>>,
    compaction: Option<crate::compaction::Compaction>,
    ttl: Duration,
}

#[derive(Clone)]
pub(crate) struct GenerationChainWrite {
    chain: GenerationChain,
    principal: Principal,
    parent: ActiveGenerationChain,
    request_delta: AiRequest,
    request: AiRequest,
    id: String,
    staged: Option<StagedGeneration>,
}

#[derive(Clone)]
pub(crate) enum GenerationSource {
    Target {
        namespace: String,
        protocol: ProtocolId,
        actual_model: String,
        selected_target_key: String,
    },
    Hook {
        protocol: ProtocolId,
    },
}

impl GenerationSource {
    pub(crate) fn thinking_source(&self) -> Option<crate::history_marker::ThinkingSource> {
        match self {
            Self::Target {
                namespace,
                protocol,
                actual_model,
                selected_target_key,
            } => Some(crate::history_marker::ThinkingSource {
                namespace: namespace.clone(),
                protocol: *protocol,
                actual_model: actual_model.clone(),
                target_id: selected_target_key.clone(),
            }),
            Self::Hook { .. } => None,
        }
    }
}

#[derive(Clone)]
struct StagedGeneration {
    response: AiResponse,
    upstream_response_id: Option<String>,
    effective_state: GenerationChainState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeginError {
    PreviousResponseNotFound,
    ItemReferenceNotFound,
    CompactionConflict,
    CompactionUnavailable,
    CompactionStorageFailed,
}

impl std::fmt::Display for BeginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::PreviousResponseNotFound => "previous_response_not_found",
            Self::ItemReferenceNotFound => "item_reference_not_found",
            Self::CompactionConflict => "compaction_conflict",
            Self::CompactionUnavailable => "compaction_unavailable",
            Self::CompactionStorageFailed => "compaction_storage_failed",
        })
    }
}

impl std::error::Error for BeginError {}

#[derive(Debug)]
pub(crate) enum PersistError {
    NotStaged,
    Store(TurnCommitError),
    HistoryMarker(crate::history_marker::HistoryMarkerError),
    Redaction(stravia_runtime_contract::redaction::RedactionError),
    Compaction(crate::compaction::CompactionError),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStaged => formatter.write_str("generation chain write was not staged"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::HistoryMarker(error) => write!(formatter, "{error}"),
            Self::Redaction(error) => write!(formatter, "{error}"),
            Self::Compaction(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for PersistError {}

pub(crate) struct PreparedCompactionInput {
    pub request: AiRequest,
    pub parent_id: Option<String>,
    pub root_id: Option<String>,
    pub has_new_user: bool,
}

impl GenerationChain {
    pub(crate) fn from_turn_chain(
        turn_chain: Arc<dyn TurnChainStore>,
        ttl: Duration,
        artifacts: Option<Arc<dyn stravia_runtime_contract::artifact::ArtifactStore>>,
    ) -> Self {
        Self {
            store: GenerationChainStore::from_turn_chain(turn_chain, ttl),
            artifacts,
            history_markers: None,
            redaction_mappings: None,
            compaction: None,
            ttl,
        }
    }

    pub(crate) fn with_history_markers(
        mut self,
        history_markers: Arc<dyn crate::history_marker::HistoryMarkerStore>,
    ) -> Self {
        self.history_markers = Some(history_markers);
        self
    }

    pub(crate) fn with_redaction_mappings(
        mut self,
        mappings: Arc<dyn stravia_credential_protection::store::MappingStore>,
    ) -> Self {
        self.redaction_mappings = Some(mappings);
        self
    }

    pub(crate) fn with_compaction(mut self, compaction: crate::compaction::Compaction) -> Self {
        self.compaction = Some(compaction);
        self
    }

    pub(crate) async fn prepare_compaction(
        &self,
        principal: Principal,
        request: AiRequest,
    ) -> Result<PreparedCompactionInput, BeginError> {
        // 只复用引用和来源准备，不 stage/persist，也不公开临时 Generation identity。
        let write = self.begin_native_compaction(principal, request).await?;
        let has_new_user = write
            .request_delta
            .items
            .iter()
            .any(|item| item.role == stravia_runtime_contract::protocol::ir::Role::User);
        let parent_id = write.parent.parent_id;
        let root_id = if parent_id.is_some() {
            write.parent.root_id
        } else {
            None
        };
        Ok(PreparedCompactionInput {
            request: write.request,
            parent_id,
            root_id,
            has_new_user,
        })
    }

    async fn compaction_source_prefix(
        &self,
        principal: &Principal,
        request: &AiRequest,
    ) -> Result<Option<(usize, String)>, BeginError> {
        let canonical = canonical_client_history_request(request);
        let identity = self
            .store
            .compaction_source_from_items(principal, request)
            .await?;
        if identity
            .as_ref()
            .is_some_and(|(count, _)| *count == canonical.items.len())
        {
            return Ok(identity);
        }
        let leading_controls = request.items.len() - canonical.items.len();
        let mut probe = request.clone();
        let strict = self
            .store
            .discover_prefix(principal, &mut probe, true)
            .await
            .map_err(|_| BeginError::CompactionUnavailable)?
            .and_then(|prefix| {
                prefix
                    .active
                    .parent_id
                    .map(|id| (prefix.matched_items - leading_controls, id))
            });
        Ok(match (identity, strict) {
            (Some(identity), Some(strict)) if strict.0 > identity.0 => Some(strict),
            (Some(identity), _) => Some(identity),
            (None, strict) => strict,
        })
    }

    pub(crate) async fn compaction_source(
        &self,
        principal: &Principal,
        request: &AiRequest,
    ) -> Result<Option<String>, BeginError> {
        let mut request = request.clone();
        request.items.retain(|item| !item.is_compaction_trigger());
        let native = self.resolve_compaction(principal, &request).await?;
        let explicit = crate::model_turn::parent_id_from_request(&request);
        let source_prefix = self.compaction_source_prefix(principal, &request).await?;
        let parent = explicit
            .or_else(|| source_prefix.map(|(_, id)| id))
            .or_else(|| {
                native
                    .as_ref()
                    .and_then(|native| native.source_generation_id.clone())
            });
        if let Some(parent) = parent.as_deref() {
            self.store
                .source_parent(principal, parent)
                .await
                .map_err(|_| BeginError::PreviousResponseNotFound)?;
            if let Some(source) = native
                .as_ref()
                .and_then(|native| native.source_generation_id.as_deref())
            {
                self.require_ancestor(principal, parent, source).await?;
            }
        }
        Ok(parent)
    }

    async fn resolve_compaction(
        &self,
        principal: &Principal,
        request: &AiRequest,
    ) -> Result<Option<crate::compaction::ResolvedCompaction>, BeginError> {
        let Some(compaction) = &self.compaction else {
            return Ok(None);
        };
        compaction
            .resolve(principal, &request.items)
            .await
            .map_err(|error| match error {
                crate::compaction::CompactionError::Conflict
                | crate::compaction::CompactionError::Invalid => BeginError::CompactionConflict,
                crate::compaction::CompactionError::Unavailable => {
                    BeginError::CompactionUnavailable
                }
                crate::compaction::CompactionError::Storage => BeginError::CompactionStorageFailed,
            })
    }

    async fn require_ancestor(
        &self,
        principal: &Principal,
        parent: &str,
        source: &str,
    ) -> Result<(), BeginError> {
        let nodes = self
            .store
            .turn_chain
            .materialize(principal, TurnNodeKind::Response, &TurnNodeId::new(parent))
            .await
            .map_err(|_| BeginError::CompactionUnavailable)?;
        if nodes.iter().any(|node| node.id.as_str() == source) {
            Ok(())
        } else {
            Err(BeginError::CompactionConflict)
        }
    }

    pub(crate) async fn begin(
        &self,
        principal: Principal,
        mut request: AiRequest,
    ) -> Result<GenerationChainWrite, BeginError> {
        let mut request_delta = request.clone();
        let native = self.resolve_compaction(&principal, &request).await?;
        let has_explicit_parent = matches!(
            request.ext.as_ref(),
            Some(ProtocolExt::OpenResponses(extension))
                if extension.previous_response_id.is_some()
        );

        if request_has_item_references(&request) {
            let ingress = ProtocolTransform::inferred_ingress(&request)
                .ok_or(BeginError::ItemReferenceNotFound)?;
            self.store
                .resolve_available_item_references(&principal, &mut request.items, ingress)
                .await
                .map_err(|_| BeginError::ItemReferenceNotFound)?;
        }

        let mut parent = if let Some(native) = native.as_ref() {
            let matched_start = request
                .items
                .windows(native.window.len())
                .position(|window| items_equal(window, &native.window))
                .ok_or(BeginError::CompactionConflict)?;
            let native_range = matched_start..matched_start + native.window.len();
            let explicit = crate::model_turn::parent_id_from_request(&request);
            let mut discovered_request = request.clone();
            crate::model_turn::clear_previous_response_id(&mut discovered_request);
            let discovered = self
                .store
                .discover_parent(&principal, &mut discovered_request)
                .await
                .map_err(|_| BeginError::CompactionUnavailable)?
                .filter(|prefix| prefix.matched_items >= native_range.end);
            if let Some(discovered) = discovered {
                let parent_id = discovered
                    .active
                    .parent_id
                    .as_deref()
                    .ok_or(BeginError::CompactionConflict)?;
                if explicit.as_deref().is_some_and(|id| id != parent_id) {
                    return Err(BeginError::CompactionConflict);
                }
                if let Some(source) = native.source_generation_id.as_deref() {
                    self.require_ancestor(&principal, parent_id, source).await?;
                }
                request_delta.items = request_delta.items[discovered.matched_items..].to_vec();
                request = discovered_request;
                let mut active = discovered.active;
                active.compaction_input_range = Some(native_range);
                active
            } else {
                if explicit
                    .as_deref()
                    .is_some_and(|id| Some(id) != native.source_generation_id.as_deref())
                {
                    return Err(BeginError::CompactionConflict);
                }
                let mut parent = match native.source_generation_id.as_deref() {
                    Some(id) => self
                        .store
                        .source_parent(&principal, id)
                        .await
                        .map_err(|_| BeginError::CompactionUnavailable)?,
                    None => ActiveGenerationChain::default(),
                };
                parent.replace_effective_history = true;
                parent.replacement_client_items =
                    Some(canonical_client_history_request(&request).items);
                parent.compaction_input_range = Some(native_range.clone());
                request_delta.items.drain(native_range);
                crate::model_turn::clear_previous_response_id(&mut request);
                parent
            }
        } else if has_explicit_parent {
            self.store
                .materialize_parent(&principal, &mut request)
                .await
                .map_err(|error| {
                    if error == "item_reference_not_found" {
                        BeginError::ItemReferenceNotFound
                    } else {
                        BeginError::PreviousResponseNotFound
                    }
                })?
        } else if request_has_item_references(&request) {
            return Err(BeginError::ItemReferenceNotFound);
        } else {
            match self.store.discover_parent(&principal, &mut request).await {
                Ok(Some(discovered)) => {
                    request_delta.items = request_delta.items[discovered.matched_items..].to_vec();
                    discovered.active
                }
                Ok(None) | Err(_) => ActiveGenerationChain::default(),
            }
        };
        if let Some(native) = native {
            parent.compaction_record_ids.extend(native.record_ids);
            parent.compaction_record_ids.sort();
            parent.compaction_record_ids.dedup();
        }

        hydrate_response_artifact_references(&principal, &mut request, self.artifacts.as_deref())
            .await
            .map_err(|_| BeginError::ItemReferenceNotFound)?;
        if let Some(parent_id) = parent.parent_id.as_deref() {
            crate::model_turn::stamp_previous_response_id(&mut request, parent_id);
        }

        Ok(GenerationChainWrite {
            chain: self.clone(),
            principal,
            parent,
            request_delta,
            request,
            id: self.store.allocate_id(),
            staged: None,
        })
    }

    pub(crate) async fn begin_native_compaction(
        &self,
        principal: Principal,
        request: AiRequest,
    ) -> Result<GenerationChainWrite, BeginError> {
        let explicit_parent = crate::model_turn::parent_id_from_request(&request).is_some();
        if explicit_parent {
            return self.begin(principal, request).await;
        }

        // Native controls authorize a new operation, not replay of the source's effective
        // history or settings. Independently verified client history supplies only ancestry.
        let triggers = request
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_compaction_trigger())
            .map(|(index, item)| (index, item.clone()))
            .collect::<Vec<_>>();
        let mut source_request = request.clone();
        source_request
            .items
            .retain(|item| !item.is_compaction_trigger());
        let client_request = canonical_client_history_request(&source_request);
        let leading_controls = source_request.items.len() - client_request.items.len();
        let source_prefix = self
            .compaction_source_prefix(&principal, &source_request)
            .await?;
        let mut write = self.begin(principal, request).await?;
        let Some((matched_items, source_id)) = source_prefix else {
            return Ok(write);
        };
        if let Some(boundary_source) = write.parent.parent_id.as_deref() {
            if boundary_source == source_id && write.parent.replacement_client_items.is_none() {
                return Ok(write);
            }
            if boundary_source != source_id {
                self.require_ancestor(&write.principal, &source_id, boundary_source)
                    .await?;
            }
        }
        for (index, item) in triggers {
            source_request.items.insert(index, item);
        }
        write.request = source_request;
        let mut parent = self
            .store
            .source_parent(&write.principal, &source_id)
            .await
            .map_err(|_| BeginError::CompactionUnavailable)?;
        parent.replace_effective_history = true;
        parent.replacement_client_items = Some(client_request.items);
        parent.compaction_input_range = write.parent.compaction_input_range.clone();
        parent
            .compaction_record_ids
            .extend(std::mem::take(&mut write.parent.compaction_record_ids));
        parent.compaction_record_ids.sort();
        parent.compaction_record_ids.dedup();

        // Count only non-trigger items: the trigger and every unproven input remain new.
        let mut position = 0;
        write.request_delta.items = write
            .request
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let old_source = if item.is_compaction_trigger() {
                    false
                } else {
                    let old =
                        (leading_controls..leading_controls + matched_items).contains(&position);
                    position += 1;
                    old
                };
                let old_window = parent
                    .compaction_input_range
                    .as_ref()
                    .is_some_and(|range| range.contains(&index));
                (!old_source && !old_window).then(|| item.clone())
            })
            .collect();
        if request_has_item_references(&write.request) {
            let ingress = ProtocolTransform::inferred_ingress(&write.request)
                .ok_or(BeginError::ItemReferenceNotFound)?;
            self.store
                .resolve_available_item_references(
                    &write.principal,
                    &mut write.request.items,
                    ingress,
                )
                .await
                .map_err(|_| BeginError::ItemReferenceNotFound)?;
        }
        hydrate_response_artifact_references(
            &write.principal,
            &mut write.request,
            self.artifacts.as_deref(),
        )
        .await
        .map_err(|_| BeginError::ItemReferenceNotFound)?;
        crate::model_turn::stamp_previous_response_id(&mut write.request, &source_id);
        write.parent = parent;
        Ok(write)
    }

    pub(crate) fn continuation_lookup(&self) -> Arc<dyn crate::model_turn::ContinuationLookup> {
        Arc::new(GenerationChainContinuationLookup {
            chain: self.clone(),
        })
    }
}

struct GenerationChainContinuationLookup {
    chain: GenerationChain,
}

#[async_trait]
impl crate::model_turn::ContinuationLookup for GenerationChainContinuationLookup {
    async fn preferred_target(&self, principal: &Principal, request: &AiRequest) -> Option<String> {
        let parent_id = crate::model_turn::parent_id_from_request(request)?;
        let materialized = self
            .chain
            .store
            .materialize_generation(principal, &TurnNodeId::new(parent_id))
            .await
            .ok()?;
        (!materialized.effective_state.selected_target_key.is_empty())
            .then_some(materialized.effective_state.selected_target_key)
    }

    async fn prepare(
        &self,
        principal: &Principal,
        target: crate::model_turn::ContinuationTarget<'_>,
        request: &mut AiRequest,
    ) -> Option<String> {
        let parent_id = crate::model_turn::parent_id_from_request(request)?;
        let mut candidate_state =
            GenerationChainState::from_request(request, target.namespace, target.protocol)
                .with_provider_model(target.actual_model);
        candidate_state.model = target.logical_model.to_owned();
        if self
            .chain
            .store
            .prepare_target_continuation(
                principal,
                &parent_id,
                request,
                &candidate_state,
                target.allow_ephemeral_response,
            )
            .await
        {
            crate::model_turn::parent_id_from_request(request)
        } else {
            crate::model_turn::clear_previous_response_id(request);
            None
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct GenerationChainState {
    pub context_fingerprint: String,
    pub context_messages: usize,
    pub model: String,
    pub instructions_fingerprint: String,
    #[serde(default)]
    pub provider_model: String,
    #[serde(default)]
    pub selected_target_key: String,
    pub tools_fingerprint: String,
    pub request_settings_fingerprint: String,
    #[serde(default)]
    canonical_controls_fingerprint: String,
    #[serde(rename = "provider")]
    pub target_namespace: String,
    pub protocol: String,
}

impl GenerationChainState {
    pub(crate) fn from_request(
        request: &AiRequest,
        target_namespace: &str,
        egress: ProtocolId,
    ) -> Self {
        let mut state = Self {
            context_fingerprint: history_context_fingerprint(&request.items),
            context_messages: request.items.len(),
            provider_model: request.model.clone(),
            target_namespace: target_namespace.to_owned(),
            protocol: egress.to_string(),
            ..Default::default()
        };
        state.refresh_request_semantics(request);
        state
    }

    pub(crate) fn refresh_request_semantics(&mut self, request: &AiRequest) {
        self.model.clone_from(&request.model);
        let mut canonical_request = request.clone();
        if canonical_request.tools.as_ref().is_some_and(Vec::is_empty) {
            canonical_request.tools = None;
        }
        self.canonical_controls_fingerprint =
            stravia_runtime_contract::protocol::ir::canonical::hash_hex(
                &stravia_runtime_contract::protocol::ir::canonical::history_request_controls_hash(
                    &canonical_request,
                ),
            );
        // Keep writing the legacy proof fields until every durable v1-v3 node has
        // expired. They are read only when a persisted node predates the canonical
        // controls proof.
        self.instructions_fingerprint = legacy_payload_fingerprint(&request.instructions);
        self.tools_fingerprint =
            legacy_payload_fingerprint(&request.tools.as_ref().filter(|tools| !tools.is_empty()));
        self.request_settings_fingerprint = legacy_payload_fingerprint(&serde_json::json!({
            "generation": &request.generation,
            "tool_choice": &request.tool_choice,
            "parallel_tool_calls": request.parallel_tool_calls,
            "disable_parallel_tool_calls": request.disable_parallel_tool_calls,
            "reasoning": &request.reasoning,
            "response_format": &request.response_format,
            "safety_settings": &request.safety_settings,
        }));
    }
    pub(crate) fn with_provider_model(mut self, provider_model: &str) -> Self {
        self.provider_model = provider_model.to_owned();
        self
    }

    pub(crate) fn with_selected_target_key(mut self, target_key: &str) -> Self {
        self.selected_target_key = target_key.to_owned();
        self
    }

    fn compatible_continuation(&self, candidate: &Self) -> bool {
        self.same_target(candidate)
            && if self.canonical_controls_fingerprint.is_empty()
                || candidate.canonical_controls_fingerprint.is_empty()
            {
                self.instructions_fingerprint == candidate.instructions_fingerprint
                    && self.tools_fingerprint == candidate.tools_fingerprint
                    && self.request_settings_fingerprint == candidate.request_settings_fingerprint
            } else {
                self.canonical_controls_fingerprint == candidate.canonical_controls_fingerprint
            }
    }

    fn same_target(&self, candidate: &Self) -> bool {
        self.model == candidate.model
            && self.provider_model == candidate.provider_model
            && self.target_namespace == candidate.target_namespace
            && self.protocol == candidate.protocol
    }

    pub(crate) fn supports_open_responses_continuation(&self) -> bool {
        self.protocol
            == stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24.to_string()
    }

    fn append_output(&mut self, response: &AiResponse) {
        for item in &response.items {
            self.context_fingerprint =
                append_history_context_fingerprint(&self.context_fingerprint, item);
        }
        self.context_messages += response.items.len();
    }
}

#[derive(Clone, Debug, Default)]
struct ActiveGenerationChain {
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub parent_upstream_response_id: Option<String>,
    pub parent_state: Option<GenerationChainState>,
    pub media_turn_messages: Vec<(usize, Vec<String>)>,
    parent_effective_items: Vec<AiItem>,
    parent_client_items: Vec<AiItem>,
    replace_effective_history: bool,
    replacement_client_items: Option<Vec<AiItem>>,
    compaction_input_range: Option<std::ops::Range<usize>>,
    fresh_inline_states: Vec<AiItem>,
    compaction_record_ids: Vec<String>,
}
#[derive(Clone, Debug)]
struct DiscoveredGenerationPrefix {
    pub active: ActiveGenerationChain,
    pub matched_items: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct RequestDelta {
    messages: Vec<AiItem>,
    system: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EffectiveHistoryMutation {
    Append { items: Vec<AiItem> },
    Replace { items: Vec<AiItem> },
}

#[derive(Clone, Serialize, Deserialize)]
struct ClientHistoryState {
    controls_fingerprint: String,
    context_fingerprint: String,
    context_messages: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_fingerprint: Option<String>,
}

impl ClientHistoryState {
    fn from_request(request: &AiRequest, items: &[AiItem]) -> Self {
        Self {
            controls_fingerprint: stravia_runtime_contract::protocol::ir::canonical::hash_hex(
                &stravia_runtime_contract::protocol::ir::canonical::history_request_controls_hash(
                    request,
                ),
            ),
            context_fingerprint: stravia_runtime_contract::protocol::ir::canonical::hash_hex(
                &stravia_runtime_contract::protocol::ir::canonical::history_context_hash(items),
            ),
            context_messages: items.len(),
            session_fingerprint: generation_session_fingerprint(request),
        }
    }

    fn reusable_namespace(&self) -> String {
        if self.session_fingerprint.is_some() {
            "stravia-generation-session-v1".into()
        } else {
            self.controls_fingerprint.clone()
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct EffectiveRequestConfig {
    model: String,
    instructions: Option<String>,
    generation: stravia_runtime_contract::protocol::ir::GenerationConfig,
    tools: Option<Vec<stravia_runtime_contract::protocol::ir::ToolSpec>>,
    tool_choice: Option<stravia_runtime_contract::protocol::ir::ToolChoice>,
    parallel_tool_calls: Option<bool>,
    disable_parallel_tool_calls: Option<bool>,
    reasoning: stravia_runtime_contract::protocol::ir::ReasoningConfig,
    response_format: Option<stravia_runtime_contract::protocol::ir::ResponseFormat>,
    safety_settings: Option<Vec<stravia_runtime_contract::protocol::ir::SafetySettings>>,
    ext: Option<OpenResponsesExt>,
}

impl EffectiveRequestConfig {
    fn from_request(request: &AiRequest) -> Self {
        Self {
            model: request.model.clone(),
            instructions: request.instructions.clone(),
            generation: request.generation.clone(),
            tools: request.tools.clone(),
            tool_choice: request.tool_choice.clone(),
            parallel_tool_calls: request.parallel_tool_calls,
            disable_parallel_tool_calls: request.disable_parallel_tool_calls,
            reasoning: request.reasoning.clone(),
            response_format: request.response_format.clone(),
            safety_settings: request.safety_settings.clone(),
            ext: match request.ext.as_ref() {
                Some(ProtocolExt::OpenResponses(extension)) => Some(extension.clone()),
                _ => None,
            },
        }
    }

    fn apply_missing_to(&self, request: &mut AiRequest) {
        if request.model.is_empty() {
            request.model.clone_from(&self.model);
        }
        let instructions_present = matches!(
            request.ext.as_ref(),
            Some(ProtocolExt::OpenResponses(extension)) if extension.instructions_present
        );
        if request.instructions.is_none() && !instructions_present {
            request.instructions.clone_from(&self.instructions);
        }
        inherit_option(
            &mut request.generation.temperature,
            &self.generation.temperature,
        );
        inherit_option(
            &mut request.generation.max_tokens,
            &self.generation.max_tokens,
        );
        inherit_option(&mut request.generation.top_p, &self.generation.top_p);
        inherit_option(&mut request.generation.seed, &self.generation.seed);
        inherit_option(&mut request.generation.stop, &self.generation.stop);
        inherit_option(
            &mut request.generation.presence_penalty,
            &self.generation.presence_penalty,
        );
        inherit_option(
            &mut request.generation.frequency_penalty,
            &self.generation.frequency_penalty,
        );
        inherit_option(&mut request.tools, &self.tools);
        inherit_option(&mut request.tool_choice, &self.tool_choice);
        inherit_option(&mut request.parallel_tool_calls, &self.parallel_tool_calls);
        inherit_option(
            &mut request.disable_parallel_tool_calls,
            &self.disable_parallel_tool_calls,
        );
        if !request.reasoning.enabled
            && request.reasoning.budget_tokens.is_none()
            && request.reasoning.effort.is_none()
            && request.reasoning.display.is_none()
        {
            request.reasoning = self.reasoning.clone();
        }
        inherit_option(&mut request.response_format, &self.response_format);
        inherit_option(&mut request.safety_settings, &self.safety_settings);
        inherit_open_responses_ext(&mut request.ext, &self.ext);
    }
}

fn inherit_option<T: Clone>(current: &mut Option<T>, parent: &Option<T>) {
    if current.is_none() {
        current.clone_from(parent);
    }
}
fn inherit_open_responses_ext(
    current: &mut Option<ProtocolExt>,
    parent: &Option<OpenResponsesExt>,
) {
    let (Some(ProtocolExt::OpenResponses(current)), Some(parent)) = (current, parent) else {
        return;
    };
    inherit_option(&mut current.background, &parent.background);
    inherit_option(&mut current.include, &parent.include);
    inherit_option(&mut current.stream_options, &parent.stream_options);
    inherit_option(&mut current.max_tool_calls, &parent.max_tool_calls);
    inherit_option(&mut current.safety_identifier, &parent.safety_identifier);
    inherit_option(&mut current.prompt_cache_key, &parent.prompt_cache_key);
    inherit_option(&mut current.top_logprobs, &parent.top_logprobs);
    inherit_option(&mut current.truncation, &parent.truncation);
    inherit_option(&mut current.metadata, &parent.metadata);
    inherit_option(&mut current.text, &parent.text);
    inherit_option(&mut current.service_tier, &parent.service_tier);
    inherit_option(&mut current.native_web_search, &parent.native_web_search);
    inherit_option(&mut current.tool_choice_ext, &parent.tool_choice_ext);
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedResponseNode {
    client_delta: RequestDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_output: Option<Vec<AiItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_history_mutation: Option<EffectiveHistoryMutation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    compaction_record_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effective_history_mutation: Option<EffectiveHistoryMutation>,
    effective_system: Option<String>,
    effective_output: AiResponse,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    effective_input: Vec<AiItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_history: Option<ClientHistoryState>,
    #[serde(default)]
    trusted_media_turn_ids: Vec<String>,
    upstream_response_id: Option<String>,
    effective_state: GenerationChainState,
    #[serde(default)]
    effective_request: Option<EffectiveRequestConfig>,
}

#[cfg(test)]
mod tests;

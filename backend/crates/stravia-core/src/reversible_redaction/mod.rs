mod detection;
pub(crate) mod store;
mod stream;
mod text;

pub use detection::{
    CredentialMatch, CredentialRule, CredentialRuleCatalog, CredentialRuleComponent,
};
pub(crate) use detection::{rule_catalog, test_text};

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;

use crate::hook::Principal;
use crate::model_turn::{CanonicalEvent, CanonicalEventStream, ModelTurnError};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::storage::DynStorage;
use store::{Mapping, MappingStore};

pub(crate) const SETTING_KEY: &str = "reversible_redaction_enabled";
const PUBLISHED_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum RedactionError {
    #[error("Reversible redaction detection failed")]
    Detection,
    #[error("Reversible redaction storage failed")]
    Storage,
    #[error("Reversible redaction text processing failed")]
    InvalidText,
    #[error("Cannot protect legacy tool-result history with unknown payload semantics")]
    AmbiguousToolResult,
}

impl From<RedactionError> for ModelTurnError {
    fn from(error: RedactionError) -> Self {
        Self::new("reversible_redaction_failed", error.to_string())
    }
}

/// 只保留 Provider 视图的引用和语义证明，不保留映射秘密。
#[derive(Clone, Debug, Default)]
pub(crate) struct RedactionTrace(Arc<Mutex<TraceState>>);

#[derive(Debug, Default)]
struct TraceState {
    references: BTreeSet<String>,
    provider: Option<ProviderProof>,
    tracking: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderProof {
    pub context_hash: [u8; 32],
    pub context_messages: usize,
    pub controls_fingerprint: String,
}

impl RedactionTrace {
    fn record(&self, references: impl IntoIterator<Item = String>) -> Result<(), RedactionError> {
        self.0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .references
            .extend(references);
        Ok(())
    }

    pub(crate) fn references(&self) -> Result<Vec<String>, RedactionError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .references
            .iter()
            .cloned()
            .collect())
    }

    pub(crate) fn provider_proof(&self) -> Result<Option<ProviderProof>, RedactionError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .provider
            .clone())
    }

    pub(crate) fn observe_provider_request(
        &self,
        request: &AiRequest,
    ) -> Result<(), RedactionError> {
        let mut state = self.0.lock().map_err(|_| RedactionError::InvalidText)?;
        if !state.tracking {
            state.provider = None;
            return Ok(());
        }
        let canonical = crate::protocol::ir::canonical::history_context_hash(&request.items);
        let controls = crate::protocol::ir::canonical::history_request_controls_hash(request);
        state.provider = Some(ProviderProof {
            context_hash: canonical,
            context_messages: request.items.len(),
            controls_fingerprint: crate::protocol::ir::canonical::hash_hex(&controls),
        });
        Ok(())
    }

    fn observe_provider_response(&self, response: &AiResponse) -> Result<(), RedactionError> {
        let mut state = self.0.lock().map_err(|_| RedactionError::InvalidText)?;
        if let Some(proof) = state.provider.as_mut() {
            for item in &response.items {
                proof.context_hash = crate::protocol::ir::canonical::append_history_context_hash(
                    &proof.context_hash,
                    item,
                );
            }
            proof.context_messages += response.items.len();
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ReversibleRedaction {
    storage: DynStorage,
    pub(crate) mappings: Arc<dyn MappingStore>,
}

impl ReversibleRedaction {
    pub(crate) fn new(storage: DynStorage, mappings: Arc<dyn MappingStore>) -> Self {
        Self { storage, mappings }
    }

    pub(crate) async fn protect(
        &self,
        principal: &Principal,
        request: &mut AiRequest,
        observer: Option<&crate::interaction_observation::RunObserver>,
    ) -> Result<Vec<Mapping>, RedactionError> {
        let enabled = match self.storage.settings().get(SETTING_KEY).await {
            Ok(None) => false,
            Ok(Some(value)) if value == "false" => false,
            Ok(Some(value)) if value == "true" => true,
            _ => return Err(RedactionError::Storage),
        };
        let mut mappings = self.mappings.active(principal).await?;
        if enabled {
            let (texts, sources) = text::request_texts(request, true)?;
            let detected = detection::detect(texts).await?;
            if !detected.is_empty() {
                let (secrets, findings): (Vec<_>, Vec<_>) = detected
                    .into_iter()
                    .map(|credential| (credential.secret, credential.findings))
                    .unzip();
                let store = self.mappings.clone();
                let principal = principal.clone();
                let observer = observer.cloned();
                // 提交与发现投递属于同一任务；调用方取消只放弃等待，不能中断提交确认。
                // 任务只完成待发布预留，沿用原保留期；不执行 Provider 调用或发布。
                let discovered = tokio::spawn(async move {
                    let discovered = store.intern(&principal, &secrets).await?;
                    // 创建归属由映射事务裁决；后续替换失败也不能抹去已提交的发现。
                    if let Some(observer) = observer {
                        observer.protect_secrets(
                            discovered
                                .mappings
                                .iter()
                                .map(|mapping| mapping.secret.as_str()),
                        );
                        let discoveries = discovered
                            .created
                            .iter()
                            .map(|&index| {
                                let findings = &findings[index];
                                crate::interaction_observation::CredentialDiscovery {
                                    rule_ids: findings
                                        .iter()
                                        .map(|finding| finding.rule_id.clone())
                                        .collect::<BTreeSet<_>>()
                                        .into_iter()
                                        .collect(),
                                    source_types: findings
                                        .iter()
                                        .map(|finding| sources[finding.source_index].to_owned())
                                        .collect::<BTreeSet<_>>()
                                        .into_iter()
                                        .collect(),
                                }
                            })
                            .collect::<Vec<_>>();
                        if !discoveries.is_empty() {
                            observer.record(
                                crate::interaction_observation::RunEvent::CredentialMappingsCreated {
                                    discoveries,
                                },
                            );
                        }
                    }
                    Ok::<_, RedactionError>(discovered)
                })
                .await
                .map_err(|_| RedactionError::Storage)??;
                for mapping in discovered.mappings {
                    if !mappings
                        .iter()
                        .any(|existing| existing.reference == mapping.reference)
                    {
                        mappings.push(mapping);
                    }
                }
            }
            let references = text::redact_request(request, &mappings)?;
            request.meta.redaction.record(references)?;
        }
        if !mappings.is_empty() {
            let (texts, _) = text::request_texts(request, enabled)?;
            request.meta.redaction.record(
                mappings
                    .iter()
                    .filter(|mapping| texts.iter().any(|text| text.contains(&mapping.reference)))
                    .map(|mapping| mapping.reference.clone()),
            )?;
        }
        request
            .meta
            .redaction
            .0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .tracking = !mappings.is_empty();
        Ok(mappings)
    }

    pub(crate) async fn publish(
        &self,
        principal: &Principal,
        trace: &RedactionTrace,
    ) -> Result<(), RedactionError> {
        let references = trace.references()?;
        if !references.is_empty() {
            self.mappings
                .publish(principal, &references, PUBLISHED_RETENTION)
                .await?;
        }
        Ok(())
    }

    pub(crate) fn restore_stream(
        &self,
        input: CanonicalEventStream,
        mappings: Vec<Mapping>,
        trace: RedactionTrace,
    ) -> CanonicalEventStream {
        if mappings.is_empty() {
            return input;
        }
        struct State {
            input: CanonicalEventStream,
            mappings: Vec<Mapping>,
            trace: RedactionTrace,
            restorer: stream::StreamRestorer,
            pending: VecDeque<Result<CanonicalEvent, ModelTurnError>>,
            ended: bool,
        }
        let state = State {
            input,
            mappings,
            trace,
            restorer: stream::StreamRestorer::new(),
            pending: VecDeque::new(),
            ended: false,
        };
        Box::pin(futures::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }
                if state.ended {
                    return None;
                }
                let result: Result<(), ModelTurnError> = match state.input.next().await {
                    Some(Ok(CanonicalEvent::Delta(delta))) => {
                        match state.restorer.push(delta, &state.mappings) {
                            Ok(deltas) => {
                                state.pending.extend(
                                    deltas
                                        .into_iter()
                                        .map(|delta| Ok(CanonicalEvent::Delta(delta))),
                                );
                                state
                                    .trace
                                    .record(state.restorer.take_restored_references())
                                    .map_err(Into::into)
                            }
                            Err(error) => Err(error.into()),
                        }
                    }
                    Some(Ok(CanonicalEvent::Compacted(mut response))) => {
                        let result = (|| {
                            state.trace.record(text::restore_compaction(
                                &mut response,
                                &state.mappings,
                            )?)?;
                            state
                                .pending
                                .push_back(Ok(CanonicalEvent::Compacted(response)));
                            Ok::<_, RedactionError>(())
                        })();
                        result.map_err(Into::into)
                    }
                    Some(Ok(CanonicalEvent::Completed(mut response))) => {
                        let result = (|| {
                            state.trace.observe_provider_response(&response)?;
                            let deltas = state.restorer.finish(&state.mappings)?;
                            state
                                .trace
                                .record(state.restorer.take_restored_references())?;
                            state
                                .trace
                                .record(text::restore_response(&mut response, &state.mappings)?)?;
                            state.pending.extend(
                                deltas
                                    .into_iter()
                                    .map(|delta| Ok(CanonicalEvent::Delta(delta))),
                            );
                            state
                                .pending
                                .push_back(Ok(CanonicalEvent::Completed(response)));
                            Ok::<_, RedactionError>(())
                        })();
                        result.map_err(Into::into)
                    }
                    Some(Err(error)) => Err(error),
                    None => {
                        state.ended = true;
                        Ok(())
                    }
                };
                if let Err(error) = result {
                    state.pending.clear();
                    state.pending.push_back(Err(error));
                    state.ended = true;
                }
            }
        }))
    }
}

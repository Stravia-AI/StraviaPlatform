mod detection;
pub mod store;
mod stream;
mod text;

pub use detection::{
    CredentialMatch, CredentialRule, CredentialRuleCatalog, CredentialRuleComponent,
};
pub use detection::{rule_catalog, test_text};

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use store::{Mapping, MappingStore};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::model_turn::{CanonicalEvent, CanonicalEventStream, ModelTurnError};
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::redaction::{RedactionError, RedactionTrace};

pub const SETTING_KEY: &str = "reversible_redaction_enabled";
const PUBLISHED_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Runtime settings boundary. Invalid or unavailable settings fail closed in the host.
#[async_trait::async_trait]
pub trait RedactionHost: Send + Sync {
    async fn enabled(&self) -> Result<bool, RedactionError>;
}

/// Owned per-run observation boundary; retained by the committed intern task on cancellation.
pub trait RedactionObserver: Send + Sync {
    fn protect_secrets(&self, mappings: &[Mapping]);
    fn mappings_created(&self, discoveries: Vec<CredentialDiscovery>);
}

pub struct CredentialDiscovery {
    pub rule_ids: Vec<String>,
    pub source_types: Vec<String>,
}

#[derive(Clone)]
pub struct ReversibleRedaction {
    host: Arc<dyn RedactionHost>,
    mappings: Arc<dyn MappingStore>,
}

impl ReversibleRedaction {
    pub fn new(host: Arc<dyn RedactionHost>, mappings: Arc<dyn MappingStore>) -> Self {
        Self { host, mappings }
    }

    pub async fn protect(
        &self,
        principal: &Principal,
        request: &mut AiRequest,
        observer: Option<Arc<dyn RedactionObserver>>,
    ) -> Result<Vec<Mapping>, RedactionError> {
        let enabled = self.host.enabled().await?;
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
                // 提交与发现投递属于同一任务；调用方取消只放弃等待，不能中断提交确认。
                // 任务只完成待发布预留，沿用原保留期；不执行 Provider 调用或发布。
                let discovered = tokio::spawn(async move {
                    let discovered = store.intern(&principal, &secrets).await?;
                    // 创建归属由映射事务裁决；后续替换失败也不能抹去已提交的发现。
                    if let Some(observer) = observer {
                        observer.protect_secrets(&discovered.mappings);
                        let discoveries = discovered
                            .created
                            .iter()
                            .map(|&index| {
                                let findings = &findings[index];
                                CredentialDiscovery {
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
                            observer.mappings_created(discoveries);
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
        request.meta.redaction.set_tracking(!mappings.is_empty())?;
        Ok(mappings)
    }

    pub async fn publish(
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

    pub fn restore_stream(
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

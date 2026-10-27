mod custom;
mod detection;
pub mod marker;
pub mod store;
mod stream;
mod text;

pub use custom::{
    CustomRule, CustomRuleError, CustomRuleInput, CustomRuleSpec,
    ID_PREFIX as CUSTOM_RULE_ID_PREFIX, SqlCustomRuleStore,
};
pub use detection::rule_catalog;
pub use detection::{
    CredentialMatch, CredentialRule, CredentialRuleCatalog, CredentialRuleComponent,
};

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
const MARKER_INSTRUCTION: &str =
    "Preserve Stravia redaction markers verbatim when used; Stravia restores their values.";

/// Runtime settings boundary. Invalid or unavailable settings fail closed in the host.
#[async_trait::async_trait]
pub trait RedactionHost: Send + Sync {
    async fn enabled(&self) -> Result<bool, RedactionError>;
}

/// Owned per-run observation boundary; retained by the committed intern task on cancellation.
pub trait RedactionObserver: Send + Sync {
    fn protect_secrets(&self, mappings: &[Mapping]);
    /// 本次请求新建的映射，每个映射一条。
    fn mappings_created(&self, discoveries: Vec<CredentialDiscovery>);
    /// 本次请求命中已有有效映射的情况，每个映射一条。
    fn mappings_reused(&self, discoveries: Vec<CredentialDiscovery>);
}

/// 一个映射在本次请求中的命中。不含明文、指纹、片段或偏移。
pub struct CredentialDiscovery {
    pub rule_ids: Vec<String>,
    pub source_types: Vec<String>,
    /// 规则与位置的逐项配对，已去重。
    pub locators: Vec<CredentialLocator>,
    /// 该映射的不透明占位符；是否记录由观察方按 Debug 状态决定。
    pub marker: String,
}

/// 一条命中的块级位置。`tool_id` 仅工具来源携带，`latest_input` 仅用户消息携带。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CredentialLocator {
    pub rule_id: String,
    pub source_type: String,
    pub tool_id: Option<String>,
    pub latest_input: Option<bool>,
}

/// 同一映射的全部命中。拥有块标识的位置会吸收同规则同来源的无标识位置：
/// 供应商原始副本与规范条目镜像同一段文本，但前者无法判定块。
fn describe(
    findings: &[detection::DetectedFinding],
    origins: &[text::TextOrigin],
) -> CredentialDiscovery {
    let mut locators = BTreeSet::new();
    for finding in findings {
        let origin = &origins[finding.source_index];
        let tool = matches!(origin.source, "tool_arguments" | "tool_result");
        locators.insert(CredentialLocator {
            rule_id: finding.rule_id.clone(),
            source_type: origin.source.to_owned(),
            tool_id: origin.tool_id.clone().filter(|_| tool),
            latest_input: origin
                .latest_input
                .filter(|_| origin.source == "user_message"),
        });
    }
    let refined: BTreeSet<_> = locators
        .iter()
        .filter(|locator| locator.tool_id.is_some() || locator.latest_input.is_some())
        .map(|locator| (locator.rule_id.clone(), locator.source_type.clone()))
        .collect();
    locators.retain(|locator| {
        locator.tool_id.is_some()
            || locator.latest_input.is_some()
            || !refined.contains(&(locator.rule_id.clone(), locator.source_type.clone()))
    });
    CredentialDiscovery {
        rule_ids: locators
            .iter()
            .map(|locator| locator.rule_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        source_types: locators
            .iter()
            .map(|locator| locator.source_type.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        locators: locators.into_iter().collect(),
        marker: String::new(),
    }
}

#[derive(Clone)]
pub struct ReversibleRedaction {
    host: Arc<dyn RedactionHost>,
    mappings: Arc<dyn MappingStore>,
    custom_rules: Arc<SqlCustomRuleStore>,
}

impl ReversibleRedaction {
    pub fn new(
        host: Arc<dyn RedactionHost>,
        mappings: Arc<dyn MappingStore>,
        custom_rules: Arc<SqlCustomRuleStore>,
    ) -> Self {
        Self {
            host,
            mappings,
            custom_rules,
        }
    }

    /// 只检测本次提交的文本，规则集与实际保护一致（内置规则加已启用的自定义规则）。
    pub async fn test_text(&self, text: String) -> Result<Vec<CredentialMatch>, RedactionError> {
        detection::test_text(text, self.custom_rules.compiled().await?).await
    }

    pub async fn protect(
        &self,
        principal: &Principal,
        request: &mut Arc<AiRequest>,
        observer: Option<Arc<dyn RedactionObserver>>,
    ) -> Result<Vec<Mapping>, RedactionError> {
        let enabled = self.host.enabled().await?;
        let mut mappings = self.mappings.active(principal).await?;
        if enabled {
            let (texts, origins) = text::request_text_origins(request, true)?;
            let detected = detection::detect(texts, self.custom_rules.compiled().await?).await?;
            if !detected.is_empty() {
                let (secrets, findings): (Vec<_>, Vec<_>) = detected
                    .into_iter()
                    .map(|credential| (credential.secret, credential.findings))
                    .unzip();
                // 在请求被改写之前完成；位置是自有数据，可以进入提交任务。
                let discoveries: Vec<_> = findings
                    .iter()
                    .map(|findings| describe(findings, &origins))
                    .collect();
                let store = self.mappings.clone();
                let principal = principal.clone();
                // 提交与发现投递属于同一任务；调用方取消只放弃等待，不能中断提交确认。
                // 任务只完成待发布预留，沿用原保留期；不执行 Provider 调用或发布。
                let discovered = tokio::spawn(async move {
                    let discovered = store.intern(&principal, &secrets).await?;
                    // 创建归属由映射事务裁决；后续替换失败也不能抹去已提交的发现。
                    if let Some(observer) = observer {
                        observer.protect_secrets(&discovered.mappings);
                        let mut created = Vec::new();
                        let mut reused = Vec::new();
                        for (index, (mut discovery, mapping)) in discoveries
                            .into_iter()
                            .zip(&discovered.mappings)
                            .enumerate()
                        {
                            discovery.marker.clone_from(&mapping.reference);
                            if discovered.created.contains(&index) {
                                created.push(discovery);
                            } else {
                                reused.push(discovery);
                            }
                        }
                        if !created.is_empty() {
                            observer.mappings_created(created);
                        }
                        if !reused.is_empty() {
                            observer.mappings_reused(reused);
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
            let request = Arc::make_mut(request);
            let references = text::redact_request(request, &mappings)?;
            request.meta.redaction.record(references)?;
        }
        if !mappings.is_empty() {
            let texts = text::request_texts(request, enabled)?;
            let references: Vec<_> = mappings
                .iter()
                .filter(|mapping| {
                    text::active(mapping)
                        && texts.iter().any(|text| text.contains(&mapping.reference))
                })
                .map(|mapping| mapping.reference.clone())
                .collect();
            if !references.is_empty() {
                let request = Arc::make_mut(request);
                request.meta.redaction.record(references)?;
                // 只影响当前模型请求；无引用的请求与客户端历史不携带平台说明。
                let instructions = request.instructions.get_or_insert_with(String::new);
                if !instructions.lines().any(|line| line == MARKER_INSTRUCTION) {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(MARKER_INSTRUCTION);
                }
            }
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

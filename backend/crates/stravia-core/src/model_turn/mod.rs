//! Model Turn Executor: one canonical Model Turn, no Orchestrator concerns.
//!
//! Callers submit a Principal, Effective Model Request, authorization,
//! optional forwarded upstream hints, and cancel / deadline. The live adapter
//! drives the `router::selection` attempt policy, first-output failover, and
//! Wasm Vendor execution through the host-owned transport boundary.

mod capability;
mod live;
mod provider;
pub(crate) mod support;

pub(crate) use live::LiveModelTurnExecutor;

use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use async_trait::async_trait;

use crate::interaction_observation::RunObserver;
use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::Deadline;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::hook::RouteContext;
use stravia_runtime_contract::protocol::ir::AiRequest;
#[cfg(test)]
use stravia_runtime_contract::protocol::ir::{AiResponse, AiStreamDelta};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTurnAuthorization {
    RouteBinding,
    CapabilityGrant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelTurnPurpose {
    #[default]
    Generation,
    Compact,
}

#[derive(Clone)]
pub(crate) struct CompactionPublication {
    pub record_id: String,
    pub operation_id: String,
    pub model_turn_id: String,
    pub mode: crate::interaction_observation::CompactionMode,
    pub source_generation_id: Option<String>,
    pub state: stravia_runtime_contract::protocol::ir::AiItem,
    pub receipt: CompactionReceipt,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CompactionReceipt {
    #[default]
    Pending,
    Delivered,
}

pub(crate) type CompactionPublications = Arc<parking_lot::Mutex<Vec<CompactionPublication>>>;

pub struct TurnInput {
    pub purpose: ModelTurnPurpose,
    pub principal: Principal,
    pub request: AiRequest,
    pub authorization: ModelTurnAuthorization,
    pub extra_headers: reqwest::header::HeaderMap,
    pub cancellation: CancellationToken,
    /// Shared idle deadline; vendor host-boundary activity keeps it alive.
    pub deadline: Deadline,
    pub(crate) allow_responses_websocket: bool,
    pub(crate) attachments_normalized: bool,
    pub(crate) observer: Option<RunObserver>,
    pub(crate) compaction_records: CompactionPublications,
    pub(crate) compaction_source_generation_id: Option<String>,
}

impl TurnInput {
    pub fn new(principal: Principal, request: AiRequest) -> Self {
        Self {
            purpose: ModelTurnPurpose::Generation,
            principal,
            request,
            authorization: ModelTurnAuthorization::RouteBinding,
            extra_headers: reqwest::header::HeaderMap::new(),
            cancellation: CancellationToken::new(),
            deadline: Deadline::from_now(Duration::from_secs(300)),
            allow_responses_websocket: true,
            attachments_normalized: false,
            observer: None,
            compaction_records: Arc::default(),
            compaction_source_generation_id: None,
        }
    }

    pub fn with_authorization(mut self, authorization: ModelTurnAuthorization) -> Self {
        self.authorization = authorization;
        self
    }

    pub fn with_extra_headers(mut self, extra_headers: reqwest::header::HeaderMap) -> Self {
        self.extra_headers = extra_headers;
        self
    }

    pub fn with_execution(mut self, cancellation: CancellationToken, deadline: Deadline) -> Self {
        self.cancellation = cancellation;
        self.deadline = deadline;
        self
    }

    pub(crate) fn with_observer(mut self, observer: RunObserver) -> Self {
        self.observer = Some(observer);
        self
    }
}

use stravia_runtime_contract::model_turn::{CanonicalEvent, CanonicalEventStream, ModelTurnError};

#[derive(Clone)]
pub(crate) struct UpstreamErrorResponse;

#[derive(Clone, Default)]
pub(crate) struct VendorPublication {
    current: Arc<parking_lot::Mutex<Option<crate::plugin::VendorPublicationFence>>>,
}

impl std::fmt::Debug for VendorPublication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VendorPublication")
            .field("available", &self.current.lock().is_some())
            .finish()
    }
}

impl VendorPublication {
    pub(crate) fn publish(&self, publication: crate::plugin::VendorPublicationFence) {
        *self.current.lock() = Some(publication);
    }

    pub(crate) fn current(&self) -> anyhow::Result<crate::plugin::VendorPublicationFence> {
        self.current
            .lock()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vendor result has no publication fence"))
    }

    pub(crate) async fn write_fence(
        &self,
    ) -> anyhow::Result<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.current()?.write_fence().await
    }
}

/// 一次最终写入可包含多轮、甚至多个 Vendor 的结果。先逐结果校验，再按
/// 活动锁的进程内稳定顺序去重取读锁，避免两个反向跨 Vendor 提交与更新写锁互锁；
/// 全部锁到手后再次逐结果校验，保证去重没有跳过各自的 caller/deadline/epoch。
pub(crate) async fn vendor_write_fences(
    publications: &[crate::plugin::VendorPublicationFence],
) -> anyhow::Result<Vec<tokio::sync::OwnedRwLockReadGuard<()>>> {
    for publication in publications {
        publication.ensure_current()?;
    }
    let mut activities = publications.to_vec();
    activities.sort_by_key(crate::plugin::VendorPublicationFence::activity_order_key);
    activities.dedup_by(|right, left| left.same_activity(right));

    let mut guards = Vec::with_capacity(activities.len());
    for activity in &activities {
        guards.push(activity.write_fence().await?);
    }
    for publication in publications {
        publication.ensure_current()?;
    }
    Ok(guards)
}

#[derive(Debug, Clone)]
pub struct TargetIdentity {
    pub actual_model: String,
    pub provider_id: String,
    pub target_id: String,
    pub(crate) namespace: String,
    pub(crate) protocol_hint: String,
    pub(crate) response_continuation_available: Arc<AtomicBool>,
    pub(crate) publication: Option<VendorPublication>,
}

impl TargetIdentity {
    pub(crate) fn protocol_identity(
        &self,
    ) -> Option<stravia_runtime_contract::protocol::ids::ProtocolIdentity> {
        (!self.protocol_hint.is_empty()).then(|| self.protocol_hint.clone().into())
    }
}

pub struct ModelTurn {
    pub(crate) model_turn_id: String,
    pub route: RouteContext,
    pub target: TargetIdentity,
    pub output: CanonicalEventStream,
    pub(crate) streamed: bool,
}

impl ModelTurn {
    #[cfg(test)]
    pub(crate) fn in_memory(
        route: RouteContext,
        request: AiRequest,
        events: impl IntoIterator<Item = Result<CanonicalEvent, ModelTurnError>>,
    ) -> Self {
        let events = events.into_iter().collect::<Vec<_>>();
        Self {
            model_turn_id: stravia_runtime_contract::identifier::new_id(),
            target: TargetIdentity {
                actual_model: request.model.clone(),
                provider_id: route.provider_id.clone(),
                target_id: route.target_id.clone(),
                namespace: String::new(),
                protocol_hint: String::new(),
                response_continuation_available: Arc::new(AtomicBool::new(false)),
                publication: None,
            },
            route,
            output: Box::pin(futures::stream::iter(events)),
            streamed: false,
        }
    }
}

#[async_trait]
pub trait ModelTurnExecutor: Send + Sync {
    async fn execute(&self, input: TurnInput) -> Result<ModelTurn, ModelTurnError>;
}

pub(crate) struct UnreachableModelTurnExecutor;

#[async_trait]
impl ModelTurnExecutor for UnreachableModelTurnExecutor {
    async fn execute(&self, _input: TurnInput) -> Result<ModelTurn, ModelTurnError> {
        Err(ModelTurnError::new(
            "internal_error",
            "Model Turn Executor is not assembled",
        ))
    }
}

pub(crate) fn unreachable_executor() -> Arc<dyn ModelTurnExecutor> {
    Arc::new(UnreachableModelTurnExecutor)
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct InMemoryModelTurnExecutor {
    responses:
        Arc<parking_lot::Mutex<std::collections::VecDeque<Result<AiResponse, ModelTurnError>>>>,
    requests: Arc<parking_lot::Mutex<Vec<AiRequest>>>,
}

#[cfg(test)]
impl InMemoryModelTurnExecutor {
    pub(crate) fn scripted(responses: impl IntoIterator<Item = AiResponse>) -> Self {
        Self {
            responses: Arc::new(parking_lot::Mutex::new(
                responses.into_iter().map(Ok).collect(),
            )),
            requests: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn requests(&self) -> Vec<AiRequest> {
        self.requests.lock().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl ModelTurnExecutor for InMemoryModelTurnExecutor {
    async fn execute(&self, input: TurnInput) -> Result<ModelTurn, ModelTurnError> {
        let request = input.request;
        self.requests.lock().push(request.clone());
        let response = self
            .responses
            .lock()
            .pop_front()
            .expect("scripted Model Turn")?;
        let route = RouteContext {
            model_id: request.model.clone(),
            provider_id: "in-memory".into(),
            target_id: "in-memory".into(),
            egress: None,
        };
        Ok(ModelTurn::in_memory(
            route,
            request,
            [
                Ok(CanonicalEvent::Delta(AiStreamDelta::TextDelta(
                    response.output_text(),
                ))),
                Ok(CanonicalEvent::Completed(Box::new(response))),
            ],
        ))
    }
}

#[cfg(test)]
mod tests;

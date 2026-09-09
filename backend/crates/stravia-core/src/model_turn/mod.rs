//! Model Turn Executor: one canonical Model Turn, no Orchestrator concerns.
//!
//! Callers submit a Principal, Effective Model Request, authorization,
//! optional forwarded upstream hints, and cancel / deadline. The live adapter
//! owns Route / Target selection, first-output failover, and Provider Transport.

mod accumulator;
mod continuation;
mod live;
mod provider;
mod support;

pub(crate) use accumulator::StreamResponseAccumulator;
pub(crate) use continuation::{
    ContinuationLookup, ContinuationTarget, clear_previous_response_id, parent_id_from_request,
    stamp_previous_response_id,
};
pub(crate) use live::LiveModelTurnExecutor;

use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::interaction_observation::RunObserver;
use stravia_runtime_contract::CancellationToken;
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

pub(crate) type CompactionPublications = Arc<std::sync::Mutex<Vec<CompactionPublication>>>;

pub struct TurnInput {
    pub purpose: ModelTurnPurpose,
    pub principal: Principal,
    pub request: AiRequest,
    pub authorization: ModelTurnAuthorization,
    pub extra_headers: reqwest::header::HeaderMap,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
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
            deadline: Instant::now() + Duration::from_secs(300),
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

    pub fn with_execution(mut self, cancellation: CancellationToken, deadline: Instant) -> Self {
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

#[derive(Debug, Clone)]
pub struct TargetIdentity {
    pub actual_model: String,
    pub provider_id: String,
    pub target_id: String,
    pub(crate) namespace: String,
    pub(crate) response_continuation_available: Arc<AtomicBool>,
}

pub struct ModelTurn {
    pub(crate) model_turn_id: String,
    pub route: RouteContext,
    pub target: TargetIdentity,
    pub output: CanonicalEventStream,
    /// The actual upstream request asked the Target to return opaque reasoning state.
    pub(crate) reasoning_encrypted_content_requested: bool,
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
            model_turn_id: uuid::Uuid::new_v4().to_string(),
            target: TargetIdentity {
                actual_model: request.model.clone(),
                provider_id: route.provider_id.clone(),
                target_id: route.target_id.clone(),
                namespace: String::new(),
                response_continuation_available: Arc::new(AtomicBool::new(false)),
            },
            route,
            output: Box::pin(futures::stream::iter(events)),
            reasoning_encrypted_content_requested: false,
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
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct InMemoryModelTurnExecutor {
    responses:
        Arc<std::sync::Mutex<std::collections::VecDeque<Result<AiResponse, ModelTurnError>>>>,
    requests: Arc<std::sync::Mutex<Vec<AiRequest>>>,
}

#[cfg(test)]
impl InMemoryModelTurnExecutor {
    pub(crate) fn scripted(responses: impl IntoIterator<Item = AiResponse>) -> Self {
        Self {
            responses: Arc::new(std::sync::Mutex::new(
                responses.into_iter().map(Ok).collect(),
            )),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn requests(&self) -> Vec<AiRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

#[cfg(test)]
#[async_trait]
impl ModelTurnExecutor for InMemoryModelTurnExecutor {
    async fn execute(&self, input: TurnInput) -> Result<ModelTurn, ModelTurnError> {
        let request = input.request;
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let response = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .expect("scripted Model Turn")?;
        let route = RouteContext {
            model_id: request.model.clone(),
            provider_id: "in-memory".into(),
            target_id: "in-memory".into(),
            egress: OPEN_RESPONSES_2026_04_24,
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

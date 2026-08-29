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

use std::pin::Pin;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::Stream;

use crate::hook::{Principal, RouteContext};
use crate::protocol::ir::{AiRequest, AiResponse, AiStreamDelta};
use crate::proxy::context::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTurnAuthorization {
    RouteBinding,
    CapabilityGrant,
}

pub struct TurnInput {
    pub principal: Principal,
    pub request: AiRequest,
    pub authorization: ModelTurnAuthorization,
    pub extra_headers: reqwest::header::HeaderMap,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    #[cfg(debug_assertions)]
    pub(crate) wire_capture_id: Option<String>,
}

impl TurnInput {
    pub fn new(principal: Principal, request: AiRequest) -> Self {
        Self {
            principal,
            request,
            authorization: ModelTurnAuthorization::RouteBinding,
            extra_headers: reqwest::header::HeaderMap::new(),
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(300),
            #[cfg(debug_assertions)]
            wire_capture_id: None,
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

    #[cfg(debug_assertions)]
    pub(crate) fn with_wire_capture_id(mut self, capture_id: String) -> Self {
        self.wire_capture_id = Some(capture_id);
        self
    }
}

#[derive(Debug, Clone)]
pub enum CanonicalEvent {
    Delta(AiStreamDelta),
    Completed(Box<AiResponse>),
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct ModelTurnError {
    pub code: String,
    pub message: String,
}

impl ModelTurnError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub type CanonicalEventStream =
    Pin<Box<dyn Stream<Item = Result<CanonicalEvent, ModelTurnError>> + Send>>;

#[derive(Debug, Clone)]
pub struct TargetIdentity {
    pub actual_model: String,
    pub provider_id: String,
    pub target_id: String,
    pub(crate) provider_name: String,
    pub(crate) route_name: String,
    pub(crate) namespace: String,
    pub(crate) response_continuation_available: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
pub(crate) struct TurnTransport {
    pub upstream_url: String,
    pub request_headers: Option<String>,
    pub request_body: Option<String>,
    pub response_headers: Arc<std::sync::Mutex<Option<String>>>,
    pub response_body: Arc<std::sync::Mutex<Vec<u8>>>,
    stream_metrics: Arc<std::sync::Mutex<TurnStreamMetrics>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TurnStreamMetrics {
    pub chunks_count: i32,
    pub first_chunk_ms: Option<i64>,
}

impl TurnTransport {
    pub(crate) fn record_stream_chunk(&self, started_at: std::time::Instant, raw: &[u8]) {
        self.response_body
            .lock()
            .expect("response body")
            .extend_from_slice(raw);
        let mut metrics = self.stream_metrics.lock().expect("stream metrics");
        metrics.chunks_count = metrics.chunks_count.saturating_add(1);
        metrics
            .first_chunk_ms
            .get_or_insert(started_at.elapsed().as_millis() as i64);
    }

    pub(crate) fn stream_metrics(&self) -> TurnStreamMetrics {
        *self.stream_metrics.lock().expect("stream metrics")
    }
}

pub struct ModelTurn {
    pub route: RouteContext,
    pub target: TargetIdentity,
    pub output: CanonicalEventStream,
    pub(crate) streamed: bool,
    pub(crate) transport: TurnTransport,
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
            target: TargetIdentity {
                actual_model: request.model.clone(),
                provider_id: route.provider_id.clone(),
                target_id: route.target_id.clone(),
                provider_name: route.provider_id.clone(),
                route_name: route.model_id.clone(),
                namespace: String::new(),
                response_continuation_available: Arc::new(AtomicBool::new(false)),
            },
            route,
            output: Box::pin(futures::stream::iter(events)),
            streamed: false,
            transport: TurnTransport::default(),
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
use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;

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

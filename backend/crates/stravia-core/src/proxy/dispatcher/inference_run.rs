use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

mod engine;

use axum::http::HeaderMap;
use axum::response::Response;
use futures::Stream;

use crate::Gateway;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, RawEnvelope};
use crate::proxy::context::RequestContext;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Created,
    Request,
    Selecting,
    Calling,
    Inspecting,
    HiddenRound,
    SemanticComplete,
    AwaitingDelivery,
    Finished,
}

pub(super) struct PhaseTracker {
    current: Phase,
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self {
            current: Phase::Created,
        }
    }
}

impl PhaseTracker {
    pub(super) fn current(&self) -> Phase {
        self.current
    }

    pub(super) fn at(current: Phase) -> Self {
        Self { current }
    }

    pub(super) fn transition(&mut self, next: Phase) -> Result<(), String> {
        let current = self.current;
        let valid = matches!(
            (current, next),
            (Phase::Created, Phase::Request)
                | (
                    Phase::Request,
                    Phase::Selecting | Phase::SemanticComplete | Phase::Finished
                )
                | (Phase::Selecting, Phase::Calling | Phase::Finished)
                | (
                    Phase::Calling,
                    Phase::Inspecting
                        | Phase::Selecting
                        | Phase::AwaitingDelivery
                        | Phase::Finished
                )
                | (
                    Phase::Inspecting,
                    Phase::HiddenRound
                        | Phase::SemanticComplete
                        | Phase::AwaitingDelivery
                        | Phase::Finished
                )
                | (
                    Phase::HiddenRound,
                    Phase::Selecting | Phase::SemanticComplete | Phase::Finished
                )
                | (
                    Phase::SemanticComplete,
                    Phase::AwaitingDelivery | Phase::Finished
                )
                | (Phase::AwaitingDelivery, Phase::Finished)
        );
        if !valid {
            return Err(format!(
                "invalid Inference Run phase transition: {current:?} -> {next:?}"
            ));
        }
        self.current = next;
        Ok(())
    }

    pub(super) fn finish(&mut self) {
        self.current = Phase::Finished;
    }
}

struct Run {
    input: RunInput,
    phase: PhaseTracker,
    inference_run: Option<crate::hook::InferenceRun>,
}

impl Run {
    async fn execute(mut self) -> Response {
        if let Err(error) = self.phase.transition(Phase::Request) {
            return engine::hook_failure_response(error);
        }
        let cancellation = self.input.context.cancellation.clone();
        let deadline = tokio::time::Instant::from_std(self.input.context.deadline.at());
        let deadline_monitor = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            cancellation.cancel();
        });
        let response =
            engine::orchestrate(self.input, &mut self.inference_run, &mut self.phase).await;
        wrap_deadline_monitor(response, deadline_monitor)
    }
}

struct DeadlineLeaseStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>,
    monitor: Option<tokio::task::JoinHandle<()>>,
}

impl DeadlineLeaseStream {
    fn stop_monitor(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            monitor.abort();
        }
    }
}

impl Stream for DeadlineLeaseStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(None) => {
                self.stop_monitor();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.stop_monitor();
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }
}

impl Drop for DeadlineLeaseStream {
    fn drop(&mut self) {
        self.stop_monitor();
    }
}

fn wrap_deadline_monitor(response: Response, monitor: tokio::task::JoinHandle<()>) -> Response {
    let (parts, body) = response.into_parts();
    let stream = DeadlineLeaseStream {
        inner: Box::pin(body.into_data_stream()),
        monitor: Some(monitor),
    };
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

pub(super) struct RunInput {
    pub gateway: Gateway,
    pub executor: std::sync::Arc<dyn crate::agent::ModelTurnExecutor>,
    pub headers: HeaderMap,
    pub envelope: RawEnvelope,
    pub request: AiRequest,
    pub ingress: ProtocolId,
    pub context: RequestContext,
}

pub(super) async fn execute(input: RunInput) -> Response {
    #[cfg(debug_assertions)]
    let capture = input.gateway.wire_capture.clone();
    #[cfg(debug_assertions)]
    let capture_id = input.context.request_id.clone();
    #[cfg(debug_assertions)]
    let protocol = input.ingress.to_string();
    #[cfg(debug_assertions)]
    if let Some(capture) = &capture {
        capture.record_client_request(&capture_id, &protocol, &input.envelope);
    }
    let response = Run {
        input,
        phase: PhaseTracker::default(),
        inference_run: None,
    }
    .execute()
    .await;
    #[cfg(debug_assertions)]
    if let Some(capture) = capture {
        return capture.wrap_client_response(capture_id, protocol, response);
    }
    response
}

pub(super) fn log_decode_error(
    gateway: &Gateway,
    envelope: &RawEnvelope,
    ingress: ProtocolId,
    error: impl std::fmt::Display,
) -> Response {
    engine::log_decode_error(gateway, envelope, ingress, error)
}

#[cfg(test)]
mod tests;

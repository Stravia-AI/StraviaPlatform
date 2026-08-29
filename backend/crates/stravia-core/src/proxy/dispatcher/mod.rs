//! Dispatcher transport entry points.
//!
//! Ingress modules decode protocol requests and hand one normalized request to
//! the crate-private Inference Run lifecycle module.

mod inference_run;

use axum::http::HeaderMap;
use axum::response::Response;

use crate::Gateway;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, RawEnvelope};
use crate::proxy::context::RequestContext;

/// Execute one complete Inference Run for a normalized ingress request.
pub(crate) async fn dispatch_pipeline(
    gateway: Gateway,
    headers: HeaderMap,
    envelope: RawEnvelope,
    request: AiRequest,
    ingress: ProtocolId,
    context: RequestContext,
) -> Response {
    let executor = std::sync::Arc::clone(&gateway.model_turn);
    inference_run::execute(inference_run::RunInput {
        gateway,
        executor,
        headers,
        envelope,
        request,
        ingress,
        context,
    })
    .await
}

pub(crate) fn log_decode_error(
    gateway: &Gateway,
    envelope: &RawEnvelope,
    ingress: ProtocolId,
    error: impl std::fmt::Display,
) -> Response {
    inference_run::log_decode_error(gateway, envelope, ingress, error)
}

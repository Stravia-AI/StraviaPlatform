//! Dispatcher transport entry points.
//!
//! Ingress modules decode protocol requests and hand one normalized request to
//! the crate-private Inference Run lifecycle module.

mod inference_run;
pub(crate) use inference_run::WebSocketRunDelivery;

use axum::http::HeaderMap;
use axum::response::Response;

use crate::Gateway;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, RawEnvelope};
use crate::proxy::context::RequestContext;

/// Execute one complete Inference Run for a normalized ingress request.
pub(crate) async fn dispatch_pipeline(
    gateway: Gateway,
    ingress_observer: crate::interaction_observation::IngressObserver,
    headers: HeaderMap,
    envelope: RawEnvelope,
    request: AiRequest,
    ingress: ProtocolId,
    context: RequestContext,
) -> Response {
    context.extensions.insert(ingress_observer);
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

pub(crate) fn defer_websocket_delivery(context: &RequestContext) {
    context
        .extensions
        .insert(inference_run::DeferredWebSocketDelivery);
}

pub(crate) fn is_websocket_delivery_deferred(context: &RequestContext) -> bool {
    context
        .extensions
        .contains::<inference_run::DeferredWebSocketDelivery>()
}

pub(crate) fn take_websocket_delivery(
    context: &RequestContext,
) -> Option<inference_run::WebSocketRunDelivery> {
    context.extensions.take()
}

pub(crate) fn decode_error_response(error: impl std::fmt::Display) -> Response {
    inference_run::decode_error_response(error)
}

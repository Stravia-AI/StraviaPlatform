//! Thin ingress shell: POST /v1/messages

use axum::Json;
use axum::extract::{State, rejection::JsonRejection};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use serde_json::Value;

use crate::Gateway;
use crate::protocol::transform::ProtocolTransform;
use crate::proxy::context::RequestContext;
use crate::proxy::dispatcher::dispatch_pipeline;
use crate::proxy::ingress::observation;
use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_runtime_contract::protocol::ir::RawEnvelope;

pub async fn handler(
    State(gw): State<Gateway>,
    mut ctx: axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    ctx.ingress_protocol = ANTHROPIC_MESSAGES_2023_06_01;
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => {
            let observer = observation::begin(
                &gw,
                &ctx,
                "POST",
                "/v1/messages",
                ANTHROPIC_MESSAGES_2023_06_01,
            );
            return observation::reject(
                observer,
                "decode",
                "invalid_json",
                rejection.into_response(),
            );
        }
    };
    let observer = observation::begin(
        &gw,
        &ctx,
        "POST",
        "/v1/messages",
        ANTHROPIC_MESSAGES_2023_06_01,
    );
    let flat_headers: std::collections::HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|vs| (k.as_str().to_lowercase(), vs.to_string()))
        })
        .collect();
    let envelope = RawEnvelope::new(Some(body.clone()), flat_headers, "POST", "/v1/messages");
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered ingress adapter");
    let request = match pair.decode_request(body) {
        Ok(request) => request,
        Err(error) => {
            return observation::reject(
                observer,
                "decode",
                "invalid_request",
                crate::proxy::dispatcher::decode_error_response(format!(
                    "invalid request: {error}"
                )),
            );
        }
    };
    dispatch_pipeline(
        gw,
        observer,
        headers,
        envelope,
        request,
        ANTHROPIC_MESSAGES_2023_06_01,
        ctx.0,
    )
    .await
}

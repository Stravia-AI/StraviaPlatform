use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::middleware;
use axum::routing::{get, post};
use base64::Engine;
use futures::Stream;
use tower_http::trace::TraceLayer;

use axum::http::StatusCode;
use axum::response::Response;

use crate::interaction_observation::{IngressCapture, RunEvent};

use super::context::inject_context;
use super::handler;
use super::ingress;
pub use super::ingress::open_responses::websocket::AllowedWebSocketOrigins;
use crate::Gateway;

// Multimodal Gemini/OpenAI-compatible requests commonly carry base64 media in JSON.
const PROXY_JSON_BODY_LIMIT_BYTES: usize = 100 * 1024 * 1024;

pub fn create_router(gateway: Gateway) -> Router {
    let mcp_router = crate::mcp::router(gateway.clone());
    let observation_gateway = gateway.clone();
    let router = Router::new()
        .route(
            "/v1/artifacts/uploads",
            post(super::artifacts::create_upload),
        )
        .route(
            "/v1/artifacts/uploads/{upload_id}/parts/{part_number}",
            axum::routing::put(super::artifacts::upload_part),
        )
        .route(
            "/v1/artifacts/uploads/{upload_id}/complete",
            post(super::artifacts::complete_upload),
        )
        .route(
            "/v1/chat/completions",
            post(ingress::openai_compatible::chat_completions::handler),
        )
        .route(
            "/v1/responses",
            post(ingress::open_responses::responses::handler)
                .get(ingress::open_responses::websocket::handler),
        )
        .route(
            "/v1/responses/compact",
            post(ingress::open_responses::responses::compact),
        )
        .route(
            "/v1/messages",
            post(ingress::anthropic_messages::messages::handler),
        )
        .route(
            "/v1/embeddings",
            post(ingress::openai_compatible::embeddings::handler),
        )
        .route(
            "/v1beta/models/{model_action}",
            post(ingress::google_generative::generate_content::handler),
        )
        .route("/v1/models", get(handler::models_list))
        .with_state(gateway)
        .merge(mcp_router)
        .fallback(protocol_not_found)
        .method_not_allowed_fallback(protocol_not_found);

    router
        .layer(DefaultBodyLimit::max(PROXY_JSON_BODY_LIMIT_BYTES))
        .layer(middleware::from_fn_with_state(
            observation_gateway,
            observe_inference_ingress,
        ))
        .layer(middleware::from_fn(inject_context))
        .layer(TraceLayer::new_for_http().make_span_with(|request: &axum::extract::Request| {
            let (uri, _) = crate::interaction_observation::redaction::redact_url(&request.uri().to_string());
            tracing::debug_span!("request", method=%request.method(), uri=%uri, version=?request.version())
        }))
}

async fn observe_inference_ingress(
    State(gateway): State<Gateway>,
    mut request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let protocol = if request.method() != axum::http::Method::POST {
        None
    } else {
        match request.uri().path() {
            "/v1/chat/completions" => {
                Some(crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
            }
            "/v1/responses" => Some(crate::protocol::ids::OPEN_RESPONSES_2026_04_24),
            "/v1/messages" => Some(crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01),
            "/v1/embeddings" => Some(crate::protocol::ids::OPENAI_COMPATIBLE_EMBEDDINGS_V1),
            path if path.starts_with("/v1beta/models/") => {
                Some(crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
            }
            _ => None,
        }
    };
    if let Some(protocol) = protocol
        && let Some(context) = request
            .extensions()
            .get::<super::context::RequestContext>()
            .cloned()
    {
        let observer =
            gateway
                .observation
                .observe_ingress(crate::interaction_observation::IngressStart {
                    id: context.request_id.clone(),
                    method: request.method().to_string(),
                    path: request.uri().to_string(),
                    protocol: protocol.to_string(),
                });
        observer.record_debug(|| RunEvent::Wire {
            direction: "client_to_platform".into(),
            transport: "http".into(),
            protocol: protocol.to_string(),
            message_type: "request_head".into(),
            model_turn_id: None,
            attempt_id: None,
            status_code: None,
            url: Some(request.uri().to_string()),
            headers: serde_json::Value::Object(
                request
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        value.to_str().ok().map(|value| {
                            (
                                name.as_str().to_owned(),
                                serde_json::Value::String(value.to_owned()),
                            )
                        })
                    })
                    .collect(),
            ),
            payload: serde_json::Value::Null,
        });
        if let Some(capture) = observer.capture() {
            let body = std::mem::replace(request.body_mut(), Body::empty());
            *request.body_mut() =
                Body::from_stream(CapturedBodyStream::new(body, capture, protocol.to_string()));
        }
        context.extensions.insert(observer);
    }
    next.run(request).await
}

// 先收齐应用层请求体再统一脱敏，避免单独持久化分块时泄露跨块凭据。
struct CapturedBodyStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>,
    capture: IngressCapture,
    protocol: String,
    bytes: Vec<u8>,
    overflowed: bool,
    finalized: bool,
}

impl CapturedBodyStream {
    fn new(body: Body, capture: IngressCapture, protocol: String) -> Self {
        Self {
            inner: Box::pin(body.into_data_stream()),
            capture,
            protocol,
            bytes: Vec::new(),
            overflowed: false,
            finalized: false,
        }
    }

    fn capture_chunk(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        if self
            .bytes
            .len()
            .checked_add(chunk.len())
            .is_some_and(|length| {
                length <= PROXY_JSON_BODY_LIMIT_BYTES.min(IngressCapture::MAX_BODY_BYTES)
            })
        {
            self.bytes.extend_from_slice(chunk);
        } else {
            self.overflowed = true;
            self.bytes = Vec::new();
        }
    }

    fn finish_complete(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        if self.overflowed {
            self.capture.mark_partial("run_size_limit");
            return;
        }
        let payload = match String::from_utf8(std::mem::take(&mut self.bytes)) {
            Ok(text) => serde_json::Value::String(text),
            Err(error) => serde_json::json!({
                "encoding": "base64",
                "data": base64::engine::general_purpose::STANDARD.encode(error.as_bytes()),
            }),
        };
        self.capture.record(RunEvent::Wire {
            direction: "client_to_platform".into(),
            transport: "http".into(),
            protocol: self.protocol.clone(),
            message_type: "request_body".into(),
            model_turn_id: None,
            attempt_id: None,
            status_code: None,
            url: None,
            headers: serde_json::Value::Null,
            payload,
        });
    }

    fn finish_partial(&mut self, reason: &'static str) {
        if !self.finalized {
            self.finalized = true;
            self.bytes = Vec::new();
            self.capture.mark_partial(reason);
        }
    }
}

impl Stream for CapturedBodyStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.capture_chunk(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish_partial("request_body_read_failed");
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.finish_complete();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for CapturedBodyStream {
    fn drop(&mut self) {
        self.finish_partial("request_body_incomplete");
    }
}

async fn protocol_not_found() -> Response {
    ingress::open_responses::responses::protocol_error(
        StatusCode::NOT_FOUND,
        "not_found",
        None,
        "The requested protocol resource was not found.",
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod compaction_tests;

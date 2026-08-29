use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use axum::http::StatusCode;
use axum::response::Response;

use super::context::inject_context;
use super::handler;
use super::ingress;
pub use super::ingress::open_responses::websocket::AllowedWebSocketOrigins;
use crate::Gateway;

// Multimodal Gemini/OpenAI-compatible requests commonly carry base64 media in JSON.
const PROXY_JSON_BODY_LIMIT_BYTES: usize = 100 * 1024 * 1024;

pub fn create_router(gateway: Gateway) -> Router {
    let mcp_router = crate::mcp::router(gateway.clone());
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
        .layer(middleware::from_fn(inject_context))
        .layer(TraceLayer::new_for_http())
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

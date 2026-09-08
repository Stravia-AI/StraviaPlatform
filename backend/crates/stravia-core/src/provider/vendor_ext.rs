//! `VendorExtension` trait — per-vendor hook points for the request/response
//! pipeline.
//!
//! Moved from `protocol/vendor/mod.rs` (PR-15). All vendor implementations
//! that previously lived under `protocol/vendor/<vendor>/` now live under
//! `provider/<vendor>/` and still register via `inventory::submit!`.
//!
//! ## Hook surface
//!
//! Request/response codec hooks live here together with Target capability
//! declaration and Vendor-owned Responses WebSocket frame construction.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::Gateway;
use crate::auth::types::StoredCredential;
use crate::db::models::Provider;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse, AiStreamDelta};
use crate::provider::registry::VendorScope;

/// Runtime context handed to every `VendorExtension` hook.
pub struct VendorCtx<'a> {
    pub provider: &'a Provider,
    pub protocol_id: ProtocolId,
    pub api_key: &'a str,
    pub actual_model: &'a str,
    pub credential: Option<&'a StoredCredential>,
}

/// Request facts that exist only for inference or only for model discovery.
#[derive(Clone, Copy)]
pub enum RequestPurpose<'a> {
    Inference {
        protocol: ProtocolId,
        base_url: &'a str,
        path: &'a str,
        actual_model: &'a str,
    },
    Models {
        endpoint: &'a str,
    },
}

impl RequestPurpose<'_> {
    pub(crate) fn endpoint(self) -> String {
        match self {
            Self::Inference { base_url, path, .. } => {
                format!("{}{}", base_url.trim_end_matches('/'), path)
            }
            Self::Models { endpoint } => endpoint.to_string(),
        }
    }
}

/// Resolved credentials; discovery never fabricates inference model context.
pub struct RequestContext<'a> {
    pub provider: &'a Provider,
    pub api_key: &'a str,
    pub credential: Option<&'a StoredCredential>,
    pub disable_default_auth: bool,
}

pub struct ConstructedRequest {
    pub url: String,
    pub headers: HeaderMap,
}

impl ConstructedRequest {
    /// Apply the shared default-auth policy before callers merge explicit headers.
    pub(crate) fn new(
        ctx: &RequestContext<'_>,
        purpose: RequestPurpose<'_>,
        url: String,
        mut headers: HeaderMap,
    ) -> anyhow::Result<Self> {
        reqwest::Url::parse(&url)?;
        if ctx.disable_default_auth {
            headers.clear();
        } else if matches!(purpose, RequestPurpose::Inference { protocol, .. } if protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages)
            && !headers.contains_key("x-api-key")
        {
            headers.remove(reqwest::header::AUTHORIZATION);
            headers.insert(
                "x-api-key",
                reqwest::header::HeaderValue::from_str(ctx.api_key)?,
            );
        }
        Ok(Self { url, headers })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolvedTargetCapabilities {
    pub stream_only: bool,
    pub responses_websocket: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ResponsesWebSocketConnectionMetadata<'a> {
    pub session_id: &'a str,
    pub thread_id: &'a str,
    pub window_id: &'a str,
}

/// Per-vendor / per-channel extension. Implementations register via
/// `inventory::submit!` from their own module.
#[async_trait]
pub trait VendorExtension: Send + Sync + 'static {
    /// Identifies which provider rows this extension applies to.
    fn scope(&self) -> VendorScope;

    /// Static metadata for the WebUI / preset list. Channel-scoped
    /// extensions return `None` because their data is folded into the
    /// vendor-scoped `VendorMetadata`.
    fn metadata(&self) -> Option<&'static crate::provider::metadata::VendorMetadata> {
        None
    }
    fn target_capabilities(&self, _protocol: ProtocolId) -> ResolvedTargetCapabilities {
        ResolvedTargetCapabilities::default()
    }
    fn responses_websocket_headers(
        &self,
        _ctx: &VendorCtx<'_>,
        _headers: &mut HeaderMap,
        _connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn responses_websocket_request(
        &self,
        _ctx: &VendorCtx<'_>,
        _body: &Value,
        _connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<Value> {
        anyhow::bail!("Vendor does not support Responses WebSocket")
    }
    fn normalize_responses_websocket_event(
        &self,
        _ctx: &VendorCtx<'_>,
        _event: &mut Value,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn construct_request(
        &self,
        ctx: &crate::provider::vendor_ext::RequestContext<'_>,
        purpose: crate::provider::vendor_ext::RequestPurpose<'_>,
    ) -> anyhow::Result<crate::provider::vendor_ext::ConstructedRequest> {
        crate::provider::vendor_ext::ConstructedRequest::new(
            ctx,
            purpose,
            purpose.endpoint(),
            HeaderMap::new(),
        )
    }

    async fn pre_encode(&self, _ctx: &VendorCtx<'_>, _req: &mut AiRequest) -> anyhow::Result<()> {
        Ok(())
    }

    async fn post_encode(
        &self,
        _ctx: &VendorCtx<'_>,
        _body: &mut Value,
        _headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn pre_parse(&self, _ctx: &VendorCtx<'_>, _resp: &mut Value) -> anyhow::Result<()> {
        Ok(())
    }

    async fn post_parse(&self, _ctx: &VendorCtx<'_>, _resp: &mut AiResponse) -> anyhow::Result<()> {
        Ok(())
    }

    fn normalizes_stream_raw_chunks(&self) -> bool {
        false
    }

    fn retain_responses_websocket_event(&self, _ctx: &VendorCtx<'_>, _event: &Value) -> bool {
        true
    }

    async fn on_stream_raw_chunk(
        &self,
        _ctx: &VendorCtx<'_>,
        _chunk: &mut String,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn on_stream_delta(
        &self,
        _ctx: &VendorCtx<'_>,
        _delta: &mut AiStreamDelta,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Async pre-flight hook. Used by Ollama to probe `/api/show` and
    /// strip tool definitions when the model lacks tool support.
    async fn pre_request(
        &self,
        _ctx: &VendorCtx<'_>,
        _req: &mut AiRequest,
        _gw: &Gateway,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

// ── Blanket impl: Vendor → VendorExtension ────────────────────────────────────

/// Any type that implements [`Vendor`] automatically satisfies
/// `VendorExtension`.  This lets pipeline free-functions keep their
/// `V: VendorExtension` bound without change.
#[async_trait]
impl<T: crate::provider::vendor::Vendor> VendorExtension for T {
    fn scope(&self) -> VendorScope {
        crate::provider::vendor::Vendor::scope(self)
    }
    fn metadata(&self) -> Option<&'static crate::provider::metadata::VendorMetadata> {
        crate::provider::vendor::Vendor::metadata(self)
    }
    fn target_capabilities(&self, protocol: ProtocolId) -> ResolvedTargetCapabilities {
        crate::provider::vendor::Vendor::target_capabilities(self, protocol)
    }
    fn responses_websocket_headers(
        &self,
        ctx: &VendorCtx<'_>,
        headers: &mut HeaderMap,
        connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::responses_websocket_headers(self, ctx, headers, connection)
    }
    fn responses_websocket_request(
        &self,
        ctx: &VendorCtx<'_>,
        body: &Value,
        connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<Value> {
        crate::provider::vendor::Vendor::responses_websocket_request(self, ctx, body, connection)
    }

    fn normalize_responses_websocket_event(
        &self,
        ctx: &VendorCtx<'_>,
        event: &mut Value,
    ) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::normalize_responses_websocket_event(self, ctx, event)
    }
    fn retain_responses_websocket_event(&self, ctx: &VendorCtx<'_>, event: &Value) -> bool {
        crate::provider::vendor::Vendor::retain_responses_websocket_event(self, ctx, event)
    }
    fn construct_request(
        &self,
        ctx: &crate::provider::vendor_ext::RequestContext<'_>,
        purpose: crate::provider::vendor_ext::RequestPurpose<'_>,
    ) -> anyhow::Result<crate::provider::vendor_ext::ConstructedRequest> {
        crate::provider::vendor::Vendor::construct_request(self, ctx, purpose)
    }
    async fn pre_encode(&self, ctx: &VendorCtx<'_>, req: &mut AiRequest) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::pre_encode(self, ctx, req).await
    }
    async fn post_encode(
        &self,
        ctx: &VendorCtx<'_>,
        body: &mut serde_json::Value,
        headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::post_encode(self, ctx, body, headers).await
    }
    async fn pre_parse(
        &self,
        ctx: &VendorCtx<'_>,
        resp: &mut serde_json::Value,
    ) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::pre_parse(self, ctx, resp).await
    }
    async fn post_parse(&self, ctx: &VendorCtx<'_>, resp: &mut AiResponse) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::post_parse(self, ctx, resp).await
    }
    fn normalizes_stream_raw_chunks(&self) -> bool {
        crate::provider::vendor::Vendor::normalizes_stream_raw_chunks(self)
    }

    async fn on_stream_raw_chunk(
        &self,
        ctx: &VendorCtx<'_>,
        chunk: &mut String,
    ) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::on_stream_raw_chunk(self, ctx, chunk).await
    }
    async fn on_stream_delta(
        &self,
        ctx: &VendorCtx<'_>,
        delta: &mut AiStreamDelta,
    ) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::on_stream_delta(self, ctx, delta).await
    }
    async fn pre_request(
        &self,
        ctx: &VendorCtx<'_>,
        req: &mut AiRequest,
        gw: &Gateway,
    ) -> anyhow::Result<()> {
        crate::provider::vendor::Vendor::pre_request(self, ctx, req, gw).await
    }
}

// ── VendorAsExt ───────────────────────────────────────────────────────────────

/// Thin proxy: presents the `VendorExtension` interface over an
/// `Arc<dyn Vendor>`.  Used by the registry's `resolve()` method so it can
/// return `Arc<dyn VendorExtension>` for both extension-only registrations
/// and full `Vendor` registrations without allocating on every call.
pub(crate) struct VendorAsExt(pub Arc<dyn crate::provider::vendor::Vendor>);

use std::sync::Arc;

#[async_trait]
impl VendorExtension for VendorAsExt {
    fn scope(&self) -> VendorScope {
        self.0.scope()
    }
    fn metadata(&self) -> Option<&'static crate::provider::metadata::VendorMetadata> {
        self.0.metadata()
    }
    fn target_capabilities(&self, protocol: ProtocolId) -> ResolvedTargetCapabilities {
        self.0.target_capabilities(protocol)
    }
    fn responses_websocket_request(
        &self,
        ctx: &VendorCtx<'_>,
        body: &Value,
        connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<Value> {
        self.0.responses_websocket_request(ctx, body, connection)
    }
    fn normalize_responses_websocket_event(
        &self,
        ctx: &VendorCtx<'_>,
        event: &mut Value,
    ) -> anyhow::Result<()> {
        self.0.normalize_responses_websocket_event(ctx, event)
    }
    fn construct_request(
        &self,
        ctx: &crate::provider::vendor_ext::RequestContext<'_>,
        purpose: crate::provider::vendor_ext::RequestPurpose<'_>,
    ) -> anyhow::Result<crate::provider::vendor_ext::ConstructedRequest> {
        self.0.construct_request(ctx, purpose)
    }
    // All async hooks use VendorExtension defaults (Ok(())).
    // VendorAsExt is only used for admin-path sync lookups.
}

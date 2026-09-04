//! Unified `Vendor` trait — merges `VendorExtension` hooks and
//! `ProviderAdapter` orchestration into a single abstraction.
//!
//! # Design
//!
//! Every vendor struct implements `Vendor` once.  The standard 7-step
//! pipeline lives in [`super::common::pipeline`]; vendor impls delegate
//! there via free-function calls:
//!
//! ```rust,ignore
//! use crate::provider::common::pipeline;
//!
//! async fn build_request(&self, req, ctx) -> Result<OutboundRequest> {
//!     pipeline::build_request(self, req, ctx).await
//! }
//! ```
//!
//! Channel-scoped extensions (e.g. `claude-code`, `codex`) keep
//! implementing `VendorExtension` and register via `ExtensionRegistration`.
//!
//! # Where to place field adjustments — layering rules
//!
//! ## Global codec  (`protocol/codec/{anthropic,openai,google}/`)
//!
//! Put logic here when the adjustment applies to **all** vendors that speak
//! the same wire protocol, regardless of who they are:
//! * Normalising enum spellings that differ across protocol versions
//!   (`tool_use` ↔ `tool_calls`, `stop_reason` ↔ `finish_reason`).
//! * Parsing / emitting standard event types defined in the spec
//!   (`content_block_start`, `message_delta`, `text_delta`, …).
//! * Forwarding unknown event types as [`StreamDelta::RawEvent`] so
//!   the downstream client receives them verbatim instead of losing them.
//!
//! ## Vendor hook  (`pre_encode` / `post_encode` / `pre_parse` / `post_parse`)
//!
//! Put logic here when the adjustment is **vendor-specific**, i.e. required
//! because a particular provider deviates from the spec or adds proprietary
//! fields:
//! * Adding / stripping provider-only top-level keys in the request body.
//! * Renaming a field that the provider misnamed relative to the spec.
//! * Injecting a vendor token or signing headers.
//!
//! If the same deviation appears in ≥ 2 unrelated providers, promote it to
//! the global codec instead.
//!
//! ## Decision flowchart
//!
//! ```text
//! Is the field/event defined in the protocol spec?
//!   YES → global codec.
//!   NO  → Is it specific to one vendor?
//!           YES → vendor hook (pre_/post_encode or pre_/post_parse).
//!           NO  → global codec with a feature-flag or a RawEvent fallback.
//! ```
//!

use std::collections::BTreeMap;

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::Gateway;
use crate::auth::types::StoredCredential;
use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse, AiStreamDelta};
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::VendorMetadata;
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::VendorScope;
use crate::provider::vendor_ext::{
    ResolvedTargetCapabilities, ResponsesWebSocketConnectionMetadata, VendorCtx,
};

// ── ProviderCtx ──────────────────────────────────────────────────────────────

/// Runtime context handed to every [`Vendor`] orchestration method.
pub struct ProviderCtx<'a> {
    pub provider: &'a Provider,
    /// Resolved egress protocol (from `ProviderProtocols::resolve_egress`).
    pub protocol: ProtocolId,
    /// Resolved egress base URL.
    pub egress_base_url: &'a str,
    pub api_key: &'a str,
    pub actual_model: &'a str,
    pub credential: Option<&'a StoredCredential>,
    pub gw: &'a Gateway,
    /// When `true`, the vendor's default `auth_headers` and the Anthropic
    /// Bearer→x-api-key rewrite are suppressed.  Set by OAuth drivers that
    /// inject their own credentials via `RuntimeBinding.extra_headers`.
    pub disable_default_auth: bool,
}

impl<'a> ProviderCtx<'a> {
    /// Build a lightweight `VendorCtx` for passing to extension hooks.
    pub fn to_vendor_ctx(&self) -> VendorCtx<'a> {
        VendorCtx {
            provider: self.provider,
            protocol_id: self.protocol,
            api_key: self.api_key,
            actual_model: self.actual_model,
            credential: self.credential,
        }
    }
}

// ── Vendor trait ─────────────────────────────────────────────────────────────

/// Unified vendor trait combining extension hooks with request orchestration.
///
/// Any type that implements `Vendor` automatically satisfies
/// [`VendorExtension`][super::vendor_ext::VendorExtension] via a blanket impl
/// in `vendor_ext.rs`, so it can be passed to `pipeline::build_request` and
/// friends without any extra boilerplate.
#[async_trait]
pub trait Vendor: Send + Sync + 'static {
    // ── Scope & metadata ──────────────────────────────────────────────────────

    /// Identifies which provider rows this vendor handles.
    fn scope(&self) -> VendorScope;

    /// Static metadata for the WebUI / preset list.
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        None
    }

    /// Validate and normalize the Adapter Credentials persisted on a Provider.
    fn validate_credentials(
        &self,
        values: BTreeMap<String, String>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        validate_declared_credentials(self.vendor_id(), self.metadata(), values)
    }

    /// Resolve the Provider base URL from a configured URL or Adapter Credentials.
    fn assemble_base_url(
        &self,
        _credentials: &BTreeMap<String, String>,
        configured_base_url: Option<&str>,
    ) -> anyhow::Result<String> {
        resolve_base_url(self.vendor_id(), configured_base_url, String::new)
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
    fn retain_responses_websocket_event(&self, _ctx: &VendorCtx<'_>, _event: &Value) -> bool {
        true
    }

    // ── Extension hooks ───────────────────────────────────────────────────────

    fn auth_headers(&self, _ctx: &VendorCtx<'_>) -> HeaderMap {
        HeaderMap::new()
    }

    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        format!("{}{}", base_url.trim_end_matches('/'), path)
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

    async fn pre_request(
        &self,
        _ctx: &VendorCtx<'_>,
        _req: &mut AiRequest,
        _gw: &Gateway,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Refreshes request authentication after an upstream 401.
    ///
    /// Returning `true` asks the transport to replay the original request
    /// exactly once with the updated headers. Vendors that do not own
    /// renewable request credentials leave the default `false`.
    async fn refresh_auth_on_unauthorized(
        &self,
        _ctx: &ProviderCtx<'_>,
        _outbound: &mut OutboundRequest,
    ) -> Result<bool, GatewayError> {
        Ok(false)
    }

    /// Returns true only when the upstream proves that a continuation reference
    /// is unavailable before starting the requested execution.
    fn is_continuation_not_found(&self, status: u16, body: &Value) -> bool {
        matches!(status, 400 | 404)
            && body
                .pointer("/error/code")
                .or_else(|| body.get("code"))
                .and_then(Value::as_str)
                == Some("previous_response_not_found")
    }

    // ── Orchestration (required) ──────────────────────────────────────────────

    /// Vendor identifier (matches `Provider.vendor` DB column).
    fn vendor_id(&self) -> &'static str;

    /// Protocols this vendor supports as egress.
    fn supported_protocols(&self) -> &'static [ProtocolId];

    /// Build the outbound request via the standard 7-step pipeline.
    async fn build_request(
        &self,
        req: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError>;

    /// Parse a non-streaming upstream response.
    async fn parse_response(
        &self,
        resp: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError>;

    /// Map a non-2xx upstream response to a `GatewayError`.
    fn map_error(&self, status: u16, body: Value) -> GatewayError;

    /// Validate pre-conditions before any request is attempted.
    fn validate_environment(&self, _provider: &Provider) -> Result<(), GatewayError> {
        Ok(())
    }
}

pub(crate) fn validate_declared_credentials(
    vendor_id: &str,
    metadata: Option<&'static VendorMetadata>,
    values: BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let metadata =
        metadata.ok_or_else(|| anyhow::anyhow!("Vendor `{vendor_id}` has no metadata"))?;
    for key in values.keys() {
        if !metadata
            .credential_fields
            .iter()
            .any(|field| field.key == key)
        {
            anyhow::bail!("credential field `{key}` is not supported by Vendor `{vendor_id}`");
        }
    }
    for field in metadata.credential_fields {
        if field.required
            && values
                .get(field.key)
                .is_none_or(|value| value.trim().is_empty())
        {
            anyhow::bail!(
                "credential field `{}` is required by Vendor `{vendor_id}`",
                field.key,
            );
        }
    }
    Ok(values
        .into_iter()
        .filter_map(|(key, value)| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some((key, value))
        })
        .collect())
}

pub(crate) fn resolve_base_url(
    vendor_id: &str,
    configured_base_url: Option<&str>,
    derive: impl FnOnce() -> String,
) -> anyhow::Result<String> {
    let base_url = configured_base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(derive);
    if base_url.is_empty() {
        anyhow::bail!(
            "Provider Base URL is empty; supply a base URL or the required Adapter Credentials for Vendor `{vendor_id}`"
        );
    }
    Ok(base_url)
}

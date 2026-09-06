//! Standard request/response pipeline shared by every
//! OpenAI-compatible vendor.
//!
//! # Usage
//!
//! Delegate `build_request` and `parse_response` to the free functions here:
//!
//! ```rust,ignore
//! use crate::provider::common::pipeline;
//!
//! async fn build_request(&self, req, ctx) -> Result<OutboundRequest> {
//!     pipeline::build_request(self, req, ctx).await
//! }
//! async fn parse_response(&self, resp, ctx) -> Result<AiResponse> {
//!     pipeline::parse_response(self, resp, ctx).await
//! }
//! ```

use reqwest::header::HeaderMap;

use crate::error::GatewayError;
fn resolve_channel_extension<'a, V>(
    vendor: &'a V,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> &'a dyn crate::provider::vendor_ext::VendorExtension
where
    V: crate::provider::vendor::Vendor,
{
    let provider_vendor = ctx.provider.vendor.as_deref().map(str::trim);
    let provider_channel = ctx.provider.channel.as_deref().map(str::trim);
    if let Some(extension) =
        crate::provider::registry::VendorRegistry::global().resolve(ctx.provider, ctx.protocol)
        && let crate::provider::registry::VendorScope::Channel {
            vendor_id,
            channel_id,
        } = extension.scope()
        && provider_vendor.is_some_and(|value| value.eq_ignore_ascii_case(vendor_id))
        && provider_channel.is_some_and(|value| value.eq_ignore_ascii_case(channel_id))
    {
        return extension.as_ref();
    }
    vendor
}

/// Standard `build_request` pipeline:
/// `pre_request → normalize_tool_results → pre_encode →
///  openai_compatible_thinking → codec_encode → post_encode → auth_headers →
///  build_url`.
pub async fn build_request<V>(
    vendor: &V,
    req: &mut crate::protocol::ir::AiRequest,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<crate::provider::outbound::OutboundRequest, GatewayError>
where
    V: crate::provider::vendor::Vendor,
{
    req.model = ctx.actual_model.to_string();

    let vendor_ctx = ctx.to_vendor_ctx();
    let extension = resolve_channel_extension(vendor, ctx);

    // 1. pre_request hook
    extension
        .pre_request(&vendor_ctx, req, ctx.gw)
        .await
        .map_err(GatewayError::internal)?;

    // 2. normalize tool results
    crate::protocol::codec::tool_correlation::normalize_request_tool_results(req);

    // 3. pre_encode hook
    extension
        .pre_encode(&vendor_ctx, req)
        .await
        .map_err(GatewayError::internal)?;

    // 4. Provider-specific OpenAI-compatible thinking controls
    super::openai_compatible_thinking::apply(ctx, req);

    // 5. canonical pair-bound encode and representability gate
    let ingress = req.meta.source_protocol.unwrap_or(ctx.protocol);
    let pair = crate::protocol::transform::ProtocolTransform::global()
        .bind(ingress, ctx.protocol)
        .map_err(|error| GatewayError::internal(error.into()))?;
    let crate::protocol::transform::EncodedRequest {
        mut body,
        headers: mut extra_headers,
        path: egress_path,
    } = pair.encode_request(req).map_err(transform_gateway_error)?;

    // 6. post_encode hook
    extension
        .post_encode(&vendor_ctx, &mut body, &mut extra_headers)
        .await
        .map_err(GatewayError::internal)?;

    // 7. auth headers
    //
    // OAuth drivers (codex, claude-code) stash their Bearer + provider-
    // specific headers in `RuntimeBinding.extra_headers` and ask the
    // dispatcher to skip the vendor's default `auth_headers` via
    // `ctx.disable_default_auth`. Skipping unconditionally would break
    // every API-key path; gating here keeps the OAuth invariant
    // ("no leaked empty x-api-key") in a single seam shared by every
    // openai-compatible adapter.
    let mut headers = if ctx.disable_default_auth {
        HeaderMap::new()
    } else {
        extension.auth_headers(&vendor_ctx)
    };
    // Anthropic-protocol upstreams require `x-api-key` instead of
    // `Authorization: Bearer`. Most OpenAI-compatible vendors blindly emit
    // Bearer; rewrite here so any vendor with a declared anthropic endpoint
    // works out of the box.
    //
    // Skipped under `disable_default_auth`: when an OAuth driver owns auth
    // (claude-code uses `Bearer <oauth_token>` + `anthropic-beta=
    // oauth-2025-04-20`), `ctx.api_key` is the OAuth Bearer token, NOT a
    // real Anthropic API key. Rewriting it here would forward the Bearer
    // as a fake `x-api-key` and break the OAuth handshake.
    if !ctx.disable_default_auth
        && ctx.protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages
        && !headers.contains_key("x-api-key")
    {
        headers.remove(reqwest::header::AUTHORIZATION);
        if let Ok(v) = reqwest::header::HeaderValue::from_str(ctx.api_key) {
            headers.insert("x-api-key", v);
        }
    }
    headers.extend(extra_headers);

    // 8. build URL
    let url = extension.build_url(&vendor_ctx, ctx.egress_base_url, &egress_path);

    Ok(crate::provider::outbound::OutboundRequest { url, headers, body })
}

fn transform_gateway_error(error: crate::protocol::transform::TransformError) -> GatewayError {
    match error {
        crate::protocol::transform::TransformError::Unrepresentable { lost, .. } => {
            GatewayError::ProtocolLossyRejected { lost }
        }
        error => GatewayError::internal(error.into()),
    }
}
pub(crate) fn prepare_responses_websocket_headers(
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    headers: &mut reqwest::header::HeaderMap,
    connection: crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata<'_>,
) -> anyhow::Result<()> {
    let vendor_ctx = ctx.to_vendor_ctx();
    if let Some(extension) = resolve_channel_override(ctx) {
        extension.responses_websocket_headers(&vendor_ctx, headers, connection)?;
    }
    Ok(())
}

pub(crate) fn build_responses_websocket_request(
    vendor: &dyn crate::provider::vendor::Vendor,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    body: &serde_json::Value,
    connection: crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata<'_>,
) -> anyhow::Result<serde_json::Value> {
    let vendor_ctx = ctx.to_vendor_ctx();
    if let Some(extension) = resolve_channel_override(ctx) {
        extension.responses_websocket_request(&vendor_ctx, body, connection)
    } else {
        vendor.responses_websocket_request(&vendor_ctx, body, connection)
    }
}
pub(crate) fn normalize_responses_websocket_event(
    vendor: &dyn crate::provider::vendor::Vendor,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    event: &mut serde_json::Value,
) -> anyhow::Result<()> {
    let vendor_ctx = ctx.to_vendor_ctx();
    if let Some(extension) = resolve_channel_override(ctx) {
        extension.normalize_responses_websocket_event(&vendor_ctx, event)
    } else {
        vendor.normalize_responses_websocket_event(&vendor_ctx, event)
    }
}
pub(crate) fn retain_responses_websocket_event(
    vendor: &dyn crate::provider::vendor::Vendor,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    event: &serde_json::Value,
) -> bool {
    let vendor_ctx = ctx.to_vendor_ctx();
    if let Some(extension) = resolve_channel_override(ctx) {
        extension.retain_responses_websocket_event(&vendor_ctx, event)
    } else {
        vendor.retain_responses_websocket_event(&vendor_ctx, event)
    }
}

/// Standard `parse_response` pipeline:
/// `pre_parse → codec_parse → reasoning_normalization → post_parse`.
pub async fn parse_response<V>(
    vendor: &V,
    resp: crate::provider::inbound::InboundResponse,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<crate::protocol::ir::AiResponse, GatewayError>
where
    V: crate::provider::vendor::Vendor,
{
    let vendor_ctx = ctx.to_vendor_ctx();
    let extension = resolve_channel_extension(vendor, ctx);
    let mut body = resp.body;

    // 1. pre_parse hook
    extension
        .pre_parse(&vendor_ctx, &mut body)
        .await
        .map_err(GatewayError::internal)?;

    // 2. canonical pair-bound decode
    let pair = crate::protocol::transform::ProtocolTransform::global()
        .bind(ctx.protocol, ctx.protocol)
        .map_err(|error| GatewayError::internal(error.into()))?;
    let mut ai_resp = pair
        .decode_response(body)
        .map_err(|error| GatewayError::internal(error.into()))?;

    // 3. reasoning normalization
    crate::protocol::codec::reasoning::normalize_response_reasoning(&mut ai_resp);

    // 4. post_parse hook
    extension
        .post_parse(&vendor_ctx, &mut ai_resp)
        .await
        .map_err(GatewayError::internal)?;

    Ok(ai_resp)
}

pub(crate) fn normalizes_stream_raw_chunks(
    vendor: &dyn crate::provider::vendor::Vendor,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> bool {
    resolve_channel_override(ctx).map_or_else(
        || vendor.normalizes_stream_raw_chunks(),
        crate::provider::vendor_ext::VendorExtension::normalizes_stream_raw_chunks,
    )
}

/// Apply the selected Vendor's raw stream normalization before protocol
/// decoding. Channel-specific extensions take precedence over the Vendor
/// implementation, matching the unary pipeline.
pub(crate) async fn normalize_stream_chunk(
    vendor: &dyn crate::provider::vendor::Vendor,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    chunk: &mut String,
) -> Result<(), GatewayError> {
    let vendor_ctx = ctx.to_vendor_ctx();
    if let Some(extension) = resolve_channel_override(ctx) {
        extension
            .on_stream_raw_chunk(&vendor_ctx, chunk)
            .await
            .map_err(GatewayError::internal)
    } else {
        vendor
            .on_stream_raw_chunk(&vendor_ctx, chunk)
            .await
            .map_err(GatewayError::internal)
    }
}

/// Apply the selected Vendor's canonical delta normalization after protocol
/// decoding and before HookRuntime observes the stream.
pub(crate) async fn normalize_stream_deltas(
    vendor: &dyn crate::provider::vendor::Vendor,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    deltas: &mut [crate::protocol::ir::AiStreamDelta],
) -> Result<(), GatewayError> {
    let vendor_ctx = ctx.to_vendor_ctx();
    if let Some(extension) = resolve_channel_override(ctx) {
        for delta in deltas {
            extension
                .on_stream_delta(&vendor_ctx, delta)
                .await
                .map_err(GatewayError::internal)?;
        }
    } else {
        for delta in deltas {
            vendor
                .on_stream_delta(&vendor_ctx, delta)
                .await
                .map_err(GatewayError::internal)?;
        }
    }
    Ok(())
}

fn resolve_channel_override(
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Option<&'static dyn crate::provider::vendor_ext::VendorExtension> {
    let provider_vendor = ctx.provider.vendor.as_deref().map(str::trim);
    let provider_channel = ctx.provider.channel.as_deref().map(str::trim);
    let extension =
        crate::provider::registry::VendorRegistry::global().resolve(ctx.provider, ctx.protocol)?;
    if let crate::provider::registry::VendorScope::Channel {
        vendor_id,
        channel_id,
    } = extension.scope()
        && provider_vendor.is_some_and(|value| value.eq_ignore_ascii_case(vendor_id))
        && provider_channel.is_some_and(|value| value.eq_ignore_ascii_case(channel_id))
    {
        Some(extension.as_ref())
    } else {
        None
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Tests cover the `disable_default_auth` gate inside `build_request`.
    //! When `ProviderCtx.disable_default_auth` is set, the vendor's default
    //! `auth_headers` AND the Anthropic-egress `Authorization → x-api-key`
    //! rewrite MUST be suppressed. Both directions are pinned so a future
    //! refactor that flips a gate fails loudly.
    use super::*;
    use crate::Gateway;
    use crate::GatewayConfig;
    use crate::db::models::Provider;
    use crate::error::GatewayError;
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId,
    };
    use crate::protocol::ir::{AiRequest, AiResponse};
    use crate::provider::inbound::InboundResponse;
    use crate::provider::openai::OpenAiVendor;
    use crate::provider::outbound::OutboundRequest;
    use crate::provider::registry::VendorScope;
    use crate::provider::vendor::{ProviderCtx, Vendor};
    use crate::provider::vendor_ext::VendorCtx;
    use async_trait::async_trait;
    use reqwest::header::HeaderMap as ExtHeaderMap;
    use serde_json::Value;
    use uuid::Uuid;

    /// Stand-in vendor: injects `x-api-key: <ctx.api_key>`, mirroring
    /// how `AnthropicVendor::auth_headers` behaves.
    struct FakeApiKeyVendor;

    #[async_trait]
    impl Vendor for FakeApiKeyVendor {
        fn scope(&self) -> VendorScope {
            VendorScope::Vendor {
                vendor_id: "fake-test",
            }
        }
        fn auth_headers(&self, ctx: &VendorCtx<'_>) -> ExtHeaderMap {
            let mut h = ExtHeaderMap::new();
            if !ctx.api_key.is_empty() {
                h.insert(
                    "x-api-key",
                    reqwest::header::HeaderValue::from_str(ctx.api_key).unwrap(),
                );
            }
            h
        }
        fn vendor_id(&self) -> &'static str {
            "fake-test"
        }
        fn supported_protocols(&self) -> &'static [ProtocolId] {
            &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
        }
        async fn build_request(
            &self,
            _req: &mut AiRequest,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<OutboundRequest, GatewayError> {
            unreachable!()
        }
        async fn parse_response(
            &self,
            _resp: InboundResponse,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<AiResponse, GatewayError> {
            unreachable!()
        }
        fn map_error(&self, status: u16, _body: Value) -> GatewayError {
            GatewayError::upstream_status("fake-test", status, None)
        }
    }

    /// Emits `Authorization: Bearer <ctx.api_key>`, mirroring OpenAI-compat
    /// vendors. PR #105's rewrite turns this into `x-api-key` on Anthropic egress.
    struct FakeBearerVendor;

    #[async_trait]
    impl Vendor for FakeBearerVendor {
        fn scope(&self) -> VendorScope {
            VendorScope::Vendor {
                vendor_id: "fake-bearer",
            }
        }
        fn auth_headers(&self, ctx: &VendorCtx<'_>) -> ExtHeaderMap {
            let mut h = ExtHeaderMap::new();
            if !ctx.api_key.is_empty() {
                h.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", ctx.api_key))
                        .unwrap(),
                );
            }
            h
        }
        fn vendor_id(&self) -> &'static str {
            "fake-bearer"
        }
        fn supported_protocols(&self) -> &'static [ProtocolId] {
            &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
        }
        async fn build_request(
            &self,
            _req: &mut AiRequest,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<OutboundRequest, GatewayError> {
            unreachable!()
        }
        async fn parse_response(
            &self,
            _resp: InboundResponse,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<AiResponse, GatewayError> {
            unreachable!()
        }
        fn map_error(&self, status: u16, _body: Value) -> GatewayError {
            GatewayError::upstream_status("fake-bearer", status, None)
        }
    }

    fn provider_with_api_key(api_key: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some("fake-test".into()),
            protocol: "openai".into(),
            base_url: "https://upstream.local".into(),
            preset_key: Some("fake-test".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: api_key.into(),
            adapter_credentials: format!(r#"{{"apiKey":"{api_key}"}}"#),
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn minimal_chat_request() -> AiRequest {
        use crate::protocol::ir::{AiItem, MessageContent, Role};
        let messages = vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }];
        let mut req = AiRequest::new("ignored-by-actual-model", messages);
        req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        req
    }

    async fn build_test_gateway() -> Gateway {
        let config = GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("stravia-pipeline-test-{}", Uuid::new_v4())),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        gw
    }

    #[tokio::test]
    async fn build_request_suppresses_default_auth_when_oauth_owns_it() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("would-leak-if-bypassed");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            actual_model: "gpt-test",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert!(
            out.headers.get("x-api-key").is_none(),
            "OAuth provider must not emit fallback x-api-key, got: {:?}",
            out.headers.get("x-api-key"),
        );
    }

    #[tokio::test]
    async fn build_request_keeps_default_auth_when_no_oauth() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            actual_model: "gpt-test",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert_eq!(
            out.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("apikey-abc"),
            "API-key path must still propagate x-api-key to upstream",
        );
    }

    /// Pins the interaction: when an OAuth driver owns auth
    /// (`disable_default_auth=true`) AND the egress family is Anthropic, the
    /// `Authorization → x-api-key` rewrite must NOT fire.
    #[tokio::test]
    async fn build_request_does_not_rewrite_oauth_bearer_to_xapikey_on_anthropic_egress() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: "https://api.anthropic.com",
            api_key: "oauth_bearer_token_should_not_become_xapikey",
            actual_model: "claude-sonnet-4-6",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let out = build_request(&FakeBearerVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert!(
            out.headers.get("x-api-key").is_none(),
            "OAuth Bearer must not be rewritten as x-api-key, got: {:?}",
            out.headers.get("x-api-key"),
        );
        assert!(
            out.headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "default Authorization must be suppressed under disable_default_auth too, got: {:?}",
            out.headers.get(reqwest::header::AUTHORIZATION),
        );
    }

    /// Mirror of #105's main use case: API-key-mode OpenAI-compat vendor
    /// hitting Anthropic egress — the rewrite block MUST fire and turn
    /// `Authorization: Bearer` into `x-api-key`.
    #[tokio::test]
    async fn build_request_rewrites_bearer_to_xapikey_on_anthropic_egress_for_apikey_path() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("real-anthropic-key");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: "https://api.anthropic.com",
            api_key: &provider.api_key,
            actual_model: "claude-sonnet-4-6",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = build_request(&FakeBearerVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert_eq!(
            out.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("real-anthropic-key"),
            "API-key path on Anthropic egress must produce x-api-key",
        );
        assert!(
            out.headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "Authorization must be removed once x-api-key is set",
        );
    }
    #[tokio::test]
    async fn build_request_uses_codex_channel_url_contract() {
        let gw = build_test_gateway().await;
        let mut provider = provider_with_api_key("oauth-token");
        provider.vendor = Some("openai".into());
        provider.preset_key = Some("openai".into());
        provider.channel = Some("codex".into());
        provider.protocol = "open-responses".into();
        provider.base_url = "https://chatgpt.com/backend-api/codex".into();
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: OPEN_RESPONSES_2026_04_24,
            egress_base_url: &provider.base_url,
            api_key: &provider.api_key,
            actual_model: "gpt-6-astra",
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };

        let out = build_request(&OpenAiVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");

        assert_eq!(out.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(
            out.headers
                .get("x-codex-routing-hint")
                .and_then(|value| value.to_str().ok()),
            Some("model=gpt-6-astra")
        );
    }
}

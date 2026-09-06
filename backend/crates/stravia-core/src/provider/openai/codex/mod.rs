//! OpenAI Codex channel (ChatGPT-backed, OAuth).

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::provider::common::openai_compat::{openai_bearer_auth_headers, openai_build_url};
use crate::provider::registry::{ExtensionRegistration, VendorScope};
use crate::provider::vendor_ext::{
    ResolvedTargetCapabilities, ResponsesWebSocketConnectionMetadata, VendorCtx, VendorExtension,
};

pub struct OpenAiCodexChannel;

/// Retain only client hints that are part of the Codex upstream contract.
///
/// Authentication and client identity are intentionally absent: the OAuth
/// runtime binding owns Authorization, ChatGPT account, User-Agent,
/// originator, and version so caller-controlled values cannot conflict.
pub(crate) fn forwarded_client_headers(headers: &HeaderMap) -> HeaderMap {
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if matches!(
            name.as_str(),
            "accept"
                | "accept-language"
                | "idempotency-key"
                | "openai-beta"
                | "session-id"
                | "session_id"
                | "conversation_id"
                | "thread-id"
                | "x-client-request-id"
                | "x-codex-beta-features"
                | "x-codex-installation-id"
                | "x-codex-turn-metadata"
                | "x-codex-turn-state"
                | "x-codex-window-id"
        ) {
            forwarded.append(name.clone(), value.clone());
        }
    }
    forwarded
}

fn codex_build_url(base_url: &str, path: &str) -> String {
    // ChatGPT's Codex backend exposes `/responses`, not the public
    // Platform API's `/v1/responses` route emitted by the shared codec.
    let path = if path.starts_with("/v1/") {
        &path[3..]
    } else {
        path
    };
    openai_build_url(base_url, path)
}

fn codex_routing_hint(body: &serde_json::Value) -> anyhow::Result<HeaderValue> {
    let model = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Codex Responses request is missing model"))?;
    let service_tier = body
        .get("service_tier")
        .and_then(serde_json::Value::as_str)
        .filter(|tier| !tier.is_empty());
    let hint = match service_tier {
        Some(tier) => format!("model={model};tier={tier}"),
        None => format!("model={model}"),
    };
    Ok(HeaderValue::from_str(&hint)?)
}

#[async_trait]
impl VendorExtension for OpenAiCodexChannel {
    fn scope(&self) -> VendorScope {
        VendorScope::Channel {
            vendor_id: "openai",
            channel_id: "codex",
        }
    }
    fn target_capabilities(
        &self,
        protocol: crate::protocol::ids::ProtocolId,
    ) -> ResolvedTargetCapabilities {
        ResolvedTargetCapabilities {
            stream_only: true,
            responses_websocket: protocol == crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
        }
    }
    async fn pre_encode(
        &self,
        _ctx: &VendorCtx<'_>,
        request: &mut crate::protocol::ir::AiRequest,
    ) -> anyhow::Result<()> {
        for item in &mut request.items {
            if item.role == crate::protocol::ir::Role::System {
                item.role = crate::protocol::ir::Role::Developer;
            }
        }
        let extension = request.ext.get_or_insert_with(|| {
            crate::protocol::ir::ProtocolExt::OpenResponses(Default::default())
        });
        if let crate::protocol::ir::ProtocolExt::OpenResponses(extension) = extension {
            extension.store = Some(false);
        }
        Ok(())
    }

    async fn post_encode(
        &self,
        _ctx: &VendorCtx<'_>,
        body: &mut serde_json::Value,
        headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        body["store"] = serde_json::Value::Bool(false);
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("responses_websockets=2026-02-06"),
        );
        headers.insert(
            HeaderName::from_static("x-client-request-id"),
            HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())?,
        );
        // Newer Codex models are routed from this header even when the
        // WebSocket frame omits service_tier.
        headers.insert(
            HeaderName::from_static("x-codex-routing-hint"),
            codex_routing_hint(body)?,
        );
        Ok(())
    }
    fn responses_websocket_headers(
        &self,
        _ctx: &VendorCtx<'_>,
        headers: &mut HeaderMap,
        connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<()> {
        headers.insert("session-id", connection.session_id.parse()?);
        headers.insert("thread-id", connection.thread_id.parse()?);
        headers.insert("x-client-request-id", connection.thread_id.parse()?);
        headers.insert("x-codex-window-id", connection.window_id.parse()?);
        Ok(())
    }
    fn responses_websocket_request(
        &self,
        _ctx: &VendorCtx<'_>,
        body: &serde_json::Value,
        connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut request = super::openai_responses_websocket_request(body)?;
        let object = request
            .as_object_mut()
            .expect("OpenAI Responses WebSocket request is an object");
        for field in [
            "frequency_penalty",
            "presence_penalty",
            "temperature",
            "top_p",
            "top_logprobs",
            "truncation",
            "max_output_tokens",
            "max_tool_calls",
            "service_tier",
        ] {
            object.remove(field);
        }
        request["client_metadata"] = serde_json::json!({
            "session_id": connection.session_id,
            "thread_id": connection.thread_id,
            "x-codex-window-id": connection.window_id,
            "turn_id": uuid::Uuid::new_v4().to_string(),
        });
        Ok(request)
    }
    fn normalize_responses_websocket_event(
        &self,
        _ctx: &VendorCtx<'_>,
        event: &mut serde_json::Value,
    ) -> anyhow::Result<()> {
        super::normalize_openai_responses_websocket_event(event)
    }
    fn retain_responses_websocket_event(
        &self,
        _ctx: &VendorCtx<'_>,
        event: &serde_json::Value,
    ) -> bool {
        !event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|event_type| {
                event_type == "response.metadata"
                    || event_type.starts_with("codex.")
                    || event_type.starts_with("responsesapi.")
            })
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        codex_build_url(base_url, path)
    }
}

inventory::submit! {
    ExtensionRegistration { make: || Box::new(OpenAiCodexChannel) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_client_headers_use_an_explicit_allowlist() {
        let mut headers = HeaderMap::new();
        headers.insert("session-id", HeaderValue::from_static("session"));
        headers.insert(
            "x-codex-installation-id",
            HeaderValue::from_static("installation"),
        );
        headers.insert("idempotency-key", HeaderValue::from_static("request"));
        headers.insert("user-agent", HeaderValue::from_static("caller/1.0"));
        headers.insert("originator", HeaderValue::from_static("caller"));
        headers.insert("version", HeaderValue::from_static("1.0"));
        headers.insert("x-unknown-client-hint", HeaderValue::from_static("drop"));

        let forwarded = forwarded_client_headers(&headers);

        assert_eq!(forwarded.get("session-id").unwrap(), "session");
        assert_eq!(
            forwarded.get("x-codex-installation-id").unwrap(),
            "installation"
        );
        assert_eq!(forwarded.get("idempotency-key").unwrap(), "request");
        for name in [
            "user-agent",
            "originator",
            "version",
            "x-unknown-client-hint",
        ] {
            assert!(forwarded.get(name).is_none(), "{name} must not pass");
        }
    }

    #[test]
    fn codex_backend_omits_public_api_version_prefix() {
        assert_eq!(
            codex_build_url("https://chatgpt.com/backend-api/codex", "/v1/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }
    #[test]
    fn codex_target_declares_stream_only_execution() {
        let capabilities =
            OpenAiCodexChannel.target_capabilities(crate::protocol::ids::OPEN_RESPONSES_2026_04_24);

        assert!(capabilities.stream_only);
        assert!(capabilities.responses_websocket);
    }
    #[tokio::test]
    async fn codex_encodes_system_messages_as_developer_messages() {
        use crate::protocol::codec::open_responses::encoder::ResponsesEncoder;
        use crate::protocol::ir::{AiItem, AiRequest, MessageContent, Role};

        let provider = crate::db::models::Provider {
            id: "provider".into(),
            name: "Codex".into(),
            vendor: Some("openai".into()),
            protocol: "open-responses".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            preset_key: Some("openai".into()),
            channel: Some("codex".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: "now".into(),
            updated_at: "now".into(),
        };
        let context = VendorCtx {
            provider: &provider,
            protocol_id: crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            api_key: "token",
            actual_model: "gpt-test",
            credential: None,
        };
        let item = |role, text: &str| AiItem {
            role,
            content: MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let mut request = AiRequest::new(
            "gpt-test",
            vec![
                item(Role::System, "system one"),
                item(Role::User, "first"),
                item(Role::System, "system two"),
                item(Role::Developer, "existing developer"),
            ],
        );
        request.instructions = Some("stable instructions".into());

        OpenAiCodexChannel
            .pre_encode(&context, &mut request)
            .await
            .expect("pre-encode");
        let (body, _) = ResponsesEncoder
            .encode_request(&request)
            .expect("encode request");

        assert_eq!(body["instructions"], "stable instructions");
        assert_eq!(
            body["input"]
                .as_array()
                .expect("input items")
                .iter()
                .map(|item| item["role"].as_str().expect("message role"))
                .collect::<Vec<_>>(),
            ["developer", "user", "developer", "developer"]
        );
        assert_eq!(body["input"][0]["content"][0]["text"], "system one");
        assert_eq!(body["input"][2]["content"][0]["text"], "system two");
    }
    #[test]
    fn codex_websocket_filters_rate_limit_side_channel_events() {
        let provider = crate::db::models::Provider {
            id: "provider".into(),
            name: "Codex".into(),
            vendor: Some("openai".into()),
            protocol: "open-responses".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            preset_key: Some("openai".into()),
            channel: Some("codex".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: "now".into(),
            updated_at: "now".into(),
        };
        let context = VendorCtx {
            provider: &provider,
            protocol_id: crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            api_key: "",
            actual_model: "gpt-5.4",
            credential: None,
        };
        assert!(!OpenAiCodexChannel.retain_responses_websocket_event(
            &context,
            &serde_json::json!({"type": "codex.rate_limits"})
        ));
        assert!(!OpenAiCodexChannel.retain_responses_websocket_event(
            &context,
            &serde_json::json!({"type": "codex.response.metadata"})
        ));
        assert!(!OpenAiCodexChannel.retain_responses_websocket_event(
            &context,
            &serde_json::json!({"type": "responsesapi.websocket_timing"})
        ));
        assert!(!OpenAiCodexChannel.retain_responses_websocket_event(
            &context,
            &serde_json::json!({"type": "response.metadata"})
        ));
        assert!(OpenAiCodexChannel.retain_responses_websocket_event(
            &context,
            &serde_json::json!({"type": "response.output_text.delta"})
        ));
    }
    #[tokio::test]
    async fn codex_websocket_contract_forces_connection_local_store_and_beta_headers() {
        let provider = crate::db::models::Provider {
            id: "provider".into(),
            name: "Codex".into(),
            vendor: Some("openai".into()),
            protocol: "open-responses".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            preset_key: Some("openai".into()),
            channel: Some("codex".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: "now".into(),
            updated_at: "now".into(),
        };
        let context = VendorCtx {
            provider: &provider,
            protocol_id: crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            api_key: "token",
            actual_model: "gpt-test",
            credential: None,
        };
        let mut request = crate::protocol::ir::AiRequest::new("gpt-test", Vec::new());
        request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
            crate::protocol::ir::OpenResponsesExt {
                store: Some(true),
                ..Default::default()
            },
        ));
        OpenAiCodexChannel
            .pre_encode(&context, &mut request)
            .await
            .expect("pre-encode");
        let Some(crate::protocol::ir::ProtocolExt::OpenResponses(extension)) = request.ext.as_ref()
        else {
            panic!("Open Responses extension");
        };
        assert_eq!(extension.store, Some(false));

        let mut body = serde_json::json!({
            "model": "gpt-6-astra",
            "service_tier": "default",
            "store": true,
            "frequency_penalty": 0.0,
            "presence_penalty": 0.0,
            "temperature": 1.0,
            "top_p": 0.98,
            "top_logprobs": 0,
            "truncation": "disabled",
            "max_output_tokens": 1024,
            "max_tool_calls": 10
        });
        let mut headers = HeaderMap::new();
        OpenAiCodexChannel
            .post_encode(&context, &mut body, &mut headers)
            .await
            .expect("post-encode");
        assert_eq!(body["store"], false);
        assert_eq!(
            headers
                .get("openai-beta")
                .and_then(|value| value.to_str().ok()),
            Some("responses_websockets=2026-02-06")
        );
        assert!(headers.get("x-client-request-id").is_some());
        assert_eq!(
            headers
                .get("x-codex-routing-hint")
                .and_then(|value| value.to_str().ok()),
            Some("model=gpt-6-astra;tier=default")
        );
        let connection = ResponsesWebSocketConnectionMetadata {
            session_id: "session",
            thread_id: "thread",
            window_id: "window",
        };
        OpenAiCodexChannel
            .responses_websocket_headers(&context, &mut headers, connection)
            .expect("WebSocket headers");
        assert_eq!(headers["session-id"], "session");
        assert_eq!(headers["thread-id"], "thread");
        assert_eq!(headers["x-client-request-id"], "thread");
        assert_eq!(headers["x-codex-window-id"], "window");
        let websocket_request = OpenAiCodexChannel
            .responses_websocket_request(&context, &body, connection)
            .expect("WebSocket request");
        assert_eq!(websocket_request["type"], "response.create");
        for field in [
            "frequency_penalty",
            "presence_penalty",
            "temperature",
            "top_p",
            "top_logprobs",
            "truncation",
            "max_output_tokens",
            "max_tool_calls",
        ] {
            assert!(websocket_request.get(field).is_none(), "{field}");
        }
        assert_eq!(
            websocket_request["client_metadata"]["session_id"],
            "session"
        );
        assert_eq!(websocket_request["client_metadata"]["thread_id"], "thread");
        assert_eq!(
            websocket_request["client_metadata"]["x-codex-window-id"],
            "window"
        );
        assert!(websocket_request["client_metadata"]["turn_id"].is_string());
        let continuation = OpenAiCodexChannel
            .responses_websocket_request(
                &context,
                &serde_json::json!({
                    "previous_response_id": "resp_parent",
                    "input": [],
                    "store": false,
                    "frequency_penalty": 0.0,
                    "presence_penalty": 0.0,
                    "temperature": 1.0,
                    "top_p": 0.98,
                    "top_logprobs": 0,
                    "truncation": "disabled",
                    "max_output_tokens": 100,
                    "max_tool_calls": 10,
                    "service_tier": "auto",
                    "instructions": "stable"
                }),
                connection,
            )
            .expect("continuation WebSocket request");
        for field in [
            "frequency_penalty",
            "presence_penalty",
            "temperature",
            "top_p",
            "top_logprobs",
            "truncation",
            "max_output_tokens",
            "max_tool_calls",
            "service_tier",
        ] {
            assert!(continuation.get(field).is_none(), "{field}");
        }
        assert_eq!(continuation["instructions"], "stable");
    }
}

use std::sync::Arc;

use reqwest::header::{HeaderMap as ReqwestHeaderMap, HeaderValue as ReqwestHeaderValue};

use crate::db::models::Provider;
use crate::provider::VendorRegistry;
use crate::provider::vendor::Vendor;
use stravia_runtime_contract::protocol::ids::Protocol;
use stravia_runtime_contract::protocol::ir::AiResponse;

pub(super) fn runtime_binding_headers(
    binding: &crate::auth::RuntimeBinding,
) -> anyhow::Result<ReqwestHeaderMap> {
    let mut headers = ReqwestHeaderMap::new();
    for (key, value) in &binding.extra_headers {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
            ReqwestHeaderValue::from_str(value)?,
        );
    }
    Ok(headers)
}

pub(super) fn merge_provider_headers(
    mut client_headers: ReqwestHeaderMap,
    adapter_headers: ReqwestHeaderMap,
    binding_headers: ReqwestHeaderMap,
) -> ReqwestHeaderMap {
    client_headers.extend(adapter_headers);
    client_headers.extend(binding_headers);
    client_headers
}

pub(super) fn resolve_vendor_adapter(
    provider: &Provider,
    protocol: Protocol,
) -> Option<Arc<dyn Vendor>> {
    let registry = VendorRegistry::global();
    let vendor_id = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|vendor| !vendor.is_empty());

    if vendor_id.is_none() && protocol == Protocol::OpenResponses {
        return registry
            .get_vendor(crate::provider::registry::protocol_default_vendor(protocol))
            .cloned();
    }

    registry
        .get_vendor(vendor_id.unwrap_or("custom"))
        .cloned()
        .or_else(|| {
            registry
                .get_vendor(crate::provider::registry::protocol_default_vendor(protocol))
                .cloned()
        })
}

pub(crate) fn ai_response_to_deltas(
    resp: &AiResponse,
) -> Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta> {
    use stravia_runtime_contract::protocol::ir::AiStreamDelta;
    let mut deltas = Vec::new();
    let mut response_profile = serde_json::Map::new();
    for key in [
        "__open_responses_effective_request",
        "__open_responses_response_profile",
    ] {
        if let Some(profile) = resp
            .vendor
            .ingress
            .get(key)
            .and_then(serde_json::Value::as_object)
        {
            response_profile.extend(profile.clone());
        }
    }
    if !response_profile.is_empty() {
        deltas.push(AiStreamDelta::ResponseMetadata {
            metadata: serde_json::Value::Object(response_profile),
        });
    }
    deltas.push(AiStreamDelta::MessageStart {
        id: if resp.id.is_empty() {
            stravia_runtime_contract::identifier::new_id()
        } else {
            resp.id.clone()
        },
        model: resp.model.clone(),
    });
    for (output_index, item) in resp.items.iter().enumerate() {
        if let Some(text) = item.output_text_ref()
            && !text.is_empty()
        {
            deltas.push(AiStreamDelta::TextDeltaWithMetadata {
                text: text.to_owned(),
                logprobs: Vec::new(),
                obfuscation: None,
                output_index: Some(output_index),
                content_index: Some(0),
            });
        } else if let Some(refusal) = item.refusal_ref()
            && !refusal.is_empty()
        {
            deltas.push(AiStreamDelta::RefusalDeltaWithIndex {
                text: refusal.to_owned(),
                output_index,
                content_index: 0,
            });
        } else if let Some((summary, content, _)) = item.reasoning_ref() {
            for (content_index, text) in summary.iter().enumerate() {
                deltas.push(AiStreamDelta::ReasoningSummaryDelta {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                });
            }
            for (content_index, text) in content.iter().enumerate() {
                deltas.push(AiStreamDelta::ThinkingDeltaWithMetadata {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                });
            }
        } else if let Some((text, signature)) = item.thinking_ref()
            && !text.is_empty()
        {
            deltas.push(AiStreamDelta::ThinkingDelta(text.to_owned()));
            if let Some(signature) = signature.filter(|value| !value.is_empty()) {
                deltas.push(AiStreamDelta::ThinkingSignature(signature.to_owned()));
            }
        } else if let Some(call) = item.function_call_ref() {
            deltas.push(AiStreamDelta::ToolCallStart {
                index: output_index,
                id: call.id.clone(),
                name: call.name.clone(),
            });
            if !call.arguments.is_empty() {
                deltas.push(AiStreamDelta::ToolCallDelta {
                    index: output_index,
                    arguments: call.arguments.clone(),
                });
            }
        } else if let Some(raw) = item.unknown_ref() {
            deltas.push(AiStreamDelta::Unknown {
                raw: raw.to_string(),
            });
        }
        deltas.push(AiStreamDelta::ItemDone {
            index: output_index,
            item: item.clone(),
        });
    }

    if let Some(metadata) = resp.vendor.ingress.get("__google_response_metadata") {
        deltas.push(AiStreamDelta::Unknown {
            raw: serde_json::json!({"__google_response_metadata": metadata}).to_string(),
        });
    }
    deltas.push(AiStreamDelta::Usage(resp.usage.clone()));
    if let Some(terminal) = resp.vendor.egress.get("__open_responses_terminal")
        && let Some(status) = terminal.get("status").and_then(serde_json::Value::as_str)
    {
        deltas.push(AiStreamDelta::ResponseTerminal {
            status: status.to_owned(),
            incomplete_details: terminal
                .get("incomplete_details")
                .filter(|value| !value.is_null())
                .cloned(),
        });
    }
    deltas.push(AiStreamDelta::Done {
        stop_reason: resp
            .stop_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string()),
    });
    deltas
}

pub(super) fn is_openai_generation_target(
    vendor: Option<&str>,
    preset_key: Option<&str>,
    is_embedding_request: bool,
) -> bool {
    if is_embedding_request {
        return false;
    }

    vendor
        .map(str::trim)
        .filter(|vendor| !vendor.is_empty())
        .is_some_and(|vendor| vendor.eq_ignore_ascii_case("openai"))
        && preset_key.map(str::trim).is_none_or(|preset_key| {
            preset_key.is_empty() || preset_key.eq_ignore_ascii_case("openai")
        })
}

#[cfg(test)]
mod tests {
    use super::{
        ai_response_to_deltas, is_openai_generation_target, merge_provider_headers,
        resolve_vendor_adapter,
    };
    use crate::db::models::Provider;
    use reqwest::header::{HeaderMap as ReqwestHeaderMap, HeaderValue as ReqwestHeaderValue};
    use stravia_runtime_contract::protocol::ids::Protocol;
    use stravia_runtime_contract::protocol::ir::AiResponse;

    fn unlabelled_provider() -> Provider {
        Provider {
            id: "provider".into(),
            name: "Custom Provider".into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "https://example.com/v1".into(),
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            adapter_credentials: r#"{"apiKey":"secret"}"#.into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn unlabelled_open_responses_target_uses_openai_vendor_adapter() {
        let adapter = resolve_vendor_adapter(&unlabelled_provider(), Protocol::OpenResponses)
            .expect("Open Responses vendor adapter");

        assert_eq!(adapter.vendor_id(), "openai");
    }

    #[test]
    fn unlabelled_chat_target_keeps_custom_vendor_adapter() {
        let adapter = resolve_vendor_adapter(&unlabelled_provider(), Protocol::OpenAICompatible)
            .expect("custom vendor adapter");

        assert_eq!(adapter.vendor_id(), "custom");
    }

    #[test]
    fn runtime_binding_headers_override_client_identity_hints() {
        let mut client = ReqwestHeaderMap::new();
        client.insert(
            reqwest::header::USER_AGENT,
            ReqwestHeaderValue::from_static("curl/8.21.0"),
        );
        let mut binding = ReqwestHeaderMap::new();
        binding.insert(
            reqwest::header::USER_AGENT,
            ReqwestHeaderValue::from_static("codex_cli_rs/0.145.0"),
        );

        let merged = merge_provider_headers(client, ReqwestHeaderMap::new(), binding);

        assert_eq!(
            merged
                .get(reqwest::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("codex_cli_rs/0.145.0")
        );
    }

    #[test]
    fn canonical_reencoding_preserves_dated_incomplete_terminal() {
        let mut response = AiResponse::new("resp_1", "logical-model");
        response.vendor.egress.insert(
            "__open_responses_terminal".into(),
            serde_json::json!({
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
            }),
        );

        let deltas = ai_response_to_deltas(&response);

        assert!(matches!(
            deltas.as_slice(),
            [
                stravia_runtime_contract::protocol::ir::AiStreamDelta::MessageStart { .. },
                stravia_runtime_contract::protocol::ir::AiStreamDelta::Usage(_),
                stravia_runtime_contract::protocol::ir::AiStreamDelta::ResponseTerminal {
                    status,
                    incomplete_details: Some(details),
                },
                stravia_runtime_contract::protocol::ir::AiStreamDelta::Done { .. },
            ] if status == "incomplete"
                && details["reason"] == "max_output_tokens"
        ));
    }

    #[test]
    fn unlabelled_open_responses_target_does_not_enable_generation_transport() {
        assert!(!is_openai_generation_target(None, None, false));
    }

    #[test]
    fn unlabelled_chat_target_does_not_change_protocol_negotiation() {
        assert!(!is_openai_generation_target(None, None, false));
    }

    #[test]
    fn explicit_openai_target_keeps_generation_transport() {
        assert!(is_openai_generation_target(
            Some("openai"),
            Some("openai"),
            false
        ));
    }

    #[test]
    fn embeddings_never_use_responses_generation_transport() {
        assert!(!is_openai_generation_target(Some("openai"), None, true));
    }

    #[test]
    fn catalog_openai_vendor_does_not_enable_generation_transport() {
        assert!(!is_openai_generation_target(
            Some("openai"),
            Some("meta"),
            false
        ));
    }
}

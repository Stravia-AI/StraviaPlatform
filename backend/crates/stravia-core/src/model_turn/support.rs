use std::sync::Arc;

use reqwest::header::{HeaderMap as ReqwestHeaderMap, HeaderValue as ReqwestHeaderValue};

use crate::Gateway;
use crate::db::models::{Provider, Route, Target};
use crate::protocol::ids::Protocol;
use crate::protocol::ir::AiResponse;
use crate::provider::VendorRegistry;
use crate::provider::vendor::Vendor;

pub(super) async fn load_route_targets(_gw: &Gateway, model: &Route) -> Vec<Target> {
    model.targets.clone()
}

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

pub(crate) fn ai_response_to_deltas(resp: &AiResponse) -> Vec<crate::protocol::ir::AiStreamDelta> {
    use crate::protocol::ir::AiStreamDelta;
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
            format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
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
    use super::is_openai_generation_target;

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

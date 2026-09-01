use super::*;

pub(super) fn ai_response_to_deltas(resp: &AiResponse) -> Vec<crate::protocol::ir::AiStreamDelta> {
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

/// Emit a `LogEntry` for a request that failed to decode at the ingress
/// boundary (before `orchestrate` runs) and return the corresponding
/// 400 `Response`. Ensures decode failures show up in the in-app log module
/// rather than only in stdout tracing.

#[cfg(test)]
mod canonical_stream_tests {
    use super::*;

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
                crate::protocol::ir::AiStreamDelta::MessageStart { .. },
                crate::protocol::ir::AiStreamDelta::Usage(_),
                crate::protocol::ir::AiStreamDelta::ResponseTerminal {
                    status,
                    incomplete_details: Some(details),
                },
                crate::protocol::ir::AiStreamDelta::Done { .. },
            ] if status == "incomplete"
                && details["reason"] == "max_output_tokens"
        ));
    }
}

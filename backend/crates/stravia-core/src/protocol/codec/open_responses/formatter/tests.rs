use super::*;
use crate::protocol::ir::Usage;

#[test]
fn formats_cache_write_tokens_when_known() {
    let mut response = AiResponse::new("resp_usage", "model");
    response.usage = Usage {
        prompt_tokens: 12,
        completion_tokens: 3,
        total_tokens: 15,
        required_components_known: true,
        cache_read_tokens: Some(4),
        cache_creation_tokens: Some(5),
        ..Usage::default()
    };

    let formatted = ResponsesResponseFormatter.format_response(&response);
    assert_eq!(
        formatted["usage"]["input_tokens_details"],
        serde_json::json!({"cached_tokens": 4, "cache_write_tokens": 5})
    );
}

#[test]
fn preserves_platform_owned_response_items() {
    let mut response = AiResponse::new("response-1", "model-1");
    response.items = vec![
        crate::protocol::ir::AiItem::unknown(serde_json::json!({
            "type": "stravia:agent_result",
            "turn_id": "aturn_1",
        })),
        crate::protocol::ir::AiItem::output_text("done"),
    ];

    let formatted = ResponsesResponseFormatter.format_response(&response);

    assert_eq!(formatted["output"][0]["type"], "stravia:agent_result");
    assert_eq!(formatted["output"][0]["turn_id"], "aturn_1");
    assert_eq!(formatted["output"][1]["content"][0]["text"], "done");
}

#[test]
fn response_resource_has_dated_required_keys_and_effective_defaults() {
    let response = AiResponse::new("resp_gateway", "logical-model");
    let formatted = ResponsesResponseFormatter.format_response(&response);

    for key in [
        "id",
        "object",
        "created_at",
        "completed_at",
        "status",
        "incomplete_details",
        "model",
        "previous_response_id",
        "instructions",
        "output",
        "error",
        "tools",
        "tool_choice",
        "truncation",
        "parallel_tool_calls",
        "text",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "top_logprobs",
        "temperature",
        "reasoning",
        "usage",
        "max_output_tokens",
        "max_tool_calls",
        "store",
        "background",
        "service_tier",
        "metadata",
        "safety_identifier",
        "prompt_cache_key",
    ] {
        assert!(formatted.get(key).is_some(), "missing required key {key}");
    }
    assert!(formatted.get("output_text").is_none());
    assert_eq!(formatted["id"], "resp_gateway");
    assert_eq!(formatted["model"], "logical-model");
    assert_eq!(formatted["temperature"], 1.0);
    assert_eq!(formatted["top_p"], 1.0);
    assert_eq!(formatted["presence_penalty"], 0.0);
    assert_eq!(formatted["frequency_penalty"], 0.0);
    assert_eq!(formatted["top_logprobs"], 0);
    assert_eq!(formatted["parallel_tool_calls"], true);
    assert_eq!(formatted["truncation"], "disabled");
    assert_eq!(formatted["service_tier"], "default");
    assert_eq!(formatted["tool_choice"], "auto");
    assert_eq!(
        formatted["text"],
        serde_json::json!({"format": {"type": "text"}})
    );
    assert_eq!(formatted["tools"], serde_json::json!([]));
    assert_eq!(formatted["metadata"], serde_json::json!({}));
    assert_eq!(formatted["store"], true);
    assert_eq!(formatted["background"], false);
    assert!(formatted["usage"].is_null());
}

#[test]
fn response_resource_echoes_effective_request_and_incomplete_status() {
    let mut response = AiResponse::new("resp_gateway", "logical-model");
    response.stop_reason = Some("length".into());
    response.vendor.ingress.insert(
        "__open_responses_effective_request".into(),
        serde_json::json!({
            "previous_response_id": "resp_parent",
            "instructions": "Be concise.",
            "temperature": 0.2,
            "store": false,
            "metadata": {"tenant": "acme"},
            "safety_identifier": "safe-user"
        }),
    );

    let formatted = ResponsesResponseFormatter.format_response(&response);

    assert_eq!(formatted["status"], "incomplete");
    assert_eq!(formatted["previous_response_id"], "resp_parent");
    assert_eq!(formatted["instructions"], "Be concise.");
    assert_eq!(formatted["temperature"], 0.2);
    assert_eq!(formatted["store"], false);
    assert_eq!(formatted["metadata"]["tenant"], "acme");
    assert_eq!(formatted["safety_identifier"], "safe-user");
}

#[test]
fn response_resource_preserves_failed_terminal_state_without_raw_provider_error() {
    let mut response = AiResponse::new("resp_gateway", "logical-model");
    response.vendor.egress.insert(
        "__open_responses_terminal".into(),
        serde_json::json!({
            "status": "failed",
            "error": {"message": "secret provider payload"}
        }),
    );

    let formatted = ResponsesResponseFormatter.format_response(&response);

    assert_eq!(formatted["status"], "failed");
    assert_eq!(formatted["error"]["code"], "upstream_response_failed");
    assert_eq!(
        formatted["error"]["message"],
        "The selected Provider failed the response."
    );
    assert!(!formatted.to_string().contains("secret provider payload"));
}
#[test]
fn clears_text_metadata_when_canonical_text_changes() {
    let mut response = AiResponse::new("resp_gateway", "logical-model");
    response.items = vec![crate::protocol::ir::AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::Text {
            text: "after".into(),
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(serde_json::json!({
            "id": "msg_provider",
            "status": "completed",
            "phase": "final_answer",
            "__open_responses_content": [{
                "type": "output_text",
                "text": "before",
                "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                "logprobs": [{"token": "before", "logprob": -0.1}]
            }]
        })),
    }];

    let formatted = ResponsesResponseFormatter.format_response(&response);

    assert_eq!(formatted["output"][0]["id"], "msg_provider");
    assert_eq!(formatted["output"][0]["phase"], "final_answer");
    assert_eq!(formatted["output"][0]["content"][0]["text"], "after");
    assert_eq!(
        formatted["output"][0]["content"][0]["annotations"],
        serde_json::json!([])
    );
    assert_eq!(
        formatted["output"][0]["content"][0]["logprobs"],
        serde_json::json!([])
    );
}

#[test]
fn preserves_canonical_item_order_without_collapsing_messages() {
    let mut response = AiResponse::new("resp_gateway", "logical-model");
    response.items = vec![
        crate::protocol::ir::AiItem::output_text("answer"),
        crate::protocol::ir::AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        }),
        crate::protocol::ir::AiItem::thinking("reasoning", Some("opaque".into())),
    ];

    let formatted = ResponsesResponseFormatter.format_response(&response);
    let types = formatted["output"]
        .as_array()
        .expect("output")
        .iter()
        .map(|item| item["type"].as_str().expect("type"))
        .collect::<Vec<_>>();

    assert_eq!(types, ["message", "function_call", "reasoning"]);
    assert_eq!(formatted["output"][2]["encrypted_content"], "opaque");
}
#[test]
fn encodes_function_output_arrays_with_dated_content_shapes() {
    let mut response = AiResponse::new("resp_gateway", "logical-model");
    response.items = vec![crate::protocol::ir::AiItem {
        role: Role::Tool,
        content: MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "tool text".into(),
                cache_control: None,
            },
            ContentBlock::Image {
                source: crate::protocol::ir::MediaSource::Url(
                    "https://example.test/image.png".into(),
                ),
                detail: Some("high".into()),
                cache_control: None,
            },
        ]),
        tool_calls: None,
        tool_call_id: Some("call_1".into()),
        meta: None,
    }];

    let formatted = ResponsesResponseFormatter.format_response(&response);

    assert_eq!(
        formatted["output"][0]["output"][0],
        serde_json::json!({"type": "input_text", "text": "tool text"})
    );
    assert_eq!(
        formatted["output"][0]["output"][1],
        serde_json::json!({
            "type": "input_image",
            "image_url": "https://example.test/image.png",
            "detail": "high"
        })
    );
}

#[test]
fn recovers_response_ids_from_function_call_output_item_ids() {
    assert_eq!(
        response_id_from_gateway_item_id("fco_abc_3"),
        Some("resp_abc".into())
    );
}

#[test]
fn preserves_nonterminal_dated_response_states() {
    for status in ["queued", "in_progress"] {
        let mut response = AiResponse::new("resp_gateway", "logical-model");
        response.vendor.egress.insert(
            "__open_responses_terminal".into(),
            serde_json::json!({
                "status": status,
                "incomplete_details": null
            }),
        );

        let formatted = ResponsesResponseFormatter.format_response(&response);
        assert_eq!(formatted["status"], status);
        assert_eq!(formatted["completed_at"], Value::Null);
    }
}

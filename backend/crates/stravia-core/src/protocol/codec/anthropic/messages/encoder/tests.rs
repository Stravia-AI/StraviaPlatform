use super::*;

#[test]
fn server_tool_results_keep_the_anthropic_wire_discriminator() {
    let encoded = encode_content_block_for_anthropic(&ContentBlock::ServerToolResult {
        tool_use_id: "srv_123".into(),
        content: serde_json::json!([{"type": "text", "text": "result"}]),
        server_type: Some("web_search_tool_result".into()),
        cache_control: None,
    });

    assert_eq!(encoded["type"], "web_search_tool_result");
    assert_eq!(encoded["tool_use_id"], "srv_123");
    assert_eq!(encoded["content"][0]["text"], "result");
}

#[test]
fn server_tool_uses_keep_the_anthropic_wire_discriminator() {
    let encoded = encode_content_block_for_anthropic(&ContentBlock::ServerToolUse {
        id: "srv_123".into(),
        name: "web_search".into(),
        input: serde_json::json!({"query": "weather"}),
        server_type: Some("web_search_tool_use".into()),
        cache_control: None,
    });

    assert_eq!(encoded["type"], "web_search_tool_use");
    assert_eq!(encoded["id"], "srv_123");
    assert_eq!(encoded["name"], "web_search");
    assert_eq!(encoded["input"]["query"], "weather");
}

#[test]
fn rejects_unrepresentable_none_tool_choice() {
    let mut request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.tool_choice = Some(ToolChoice::None);

    let error = AnthropicEncoder
        .encode_request(&request)
        .expect_err("Anthropic cannot represent tool_choice none");
    assert!(error.to_string().contains("tool_choice"));
}

#[test]
fn effort_control_encodes_adaptive_without_replaying_raw_thinking() {
    let mut request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "high".into(),
    });
    request.meta.vendor.ingress.insert(
        "__anthropic_thinking".into(),
        serde_json::json!({"type": "disabled"}),
    );

    let (body, _) = AnthropicEncoder
        .encode_request(&request)
        .expect("adaptive thinking");
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
}

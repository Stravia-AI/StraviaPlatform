use super::*;

#[test]
fn encodes_cohere_tool_schema_without_openai_shape() {
    let mut request = AiRequest::new("command-a", vec![AiItem::output_text("hello")]);
    request.items[0].role = Role::User;
    request.tools = Some(vec![crate::protocol::ir::ToolSpec {
        name: "weather".into(),
        description: Some("Get weather".into()),
        parameters: json!({"type": "object"}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let (body, _) = CohereChatV2.encode_request(&request).unwrap();
    assert_eq!(body["tools"][0]["function"]["name"], "weather");
    assert!(body.get("messages").is_some());
    assert!(body.get("choices").is_none());
}

#[test]
fn normalizes_cohere_null_tool_arguments() {
    let response = CohereChatV2
        .decode_response(json!({
            "generation_id": "gen_1",
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "weather", "arguments": "null"}
                }]
            },
            "finish_reason": "TOOL_CALL",
            "usage": {"tokens": {"input_tokens": 3, "output_tokens": 4}}
        }))
        .unwrap();

    assert_eq!(response.tool_calls().next().unwrap().arguments, "{}");
}

#[test]
fn omits_assistant_text_when_replaying_cohere_tool_calls() {
    let item = AiItem {
        role: Role::Assistant,
        content: MessageContent::Text("I will call weather.".into()),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            name: "weather".into(),
            arguments: "{}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };

    let message = encode_message(&item).unwrap();
    assert!(message.get("content").is_none());
    assert_eq!(message["tool_calls"][0]["function"]["name"], "weather");
}

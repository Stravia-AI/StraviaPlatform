use super::*;

#[test]
fn encodes_cohere_tool_schema_without_openai_shape() {
    let mut request = AiRequest::new("command-a", vec![AiItem::output_text("hello")]);
    request.items[0].role = Role::User;
    request.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "weather".into(),
        description: Some("Get weather".into()),
        parameters: json!({"type": "object"}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let (body, _) = CohereChatV2.encode_request(request).unwrap();
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
        content: MessageContent::Text("I will call weather.".to_owned().into()),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            name: "weather".into(),
            arguments: "{}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };

    let message = encode_message(&item)
        .unwrap()
        .expect("tool-call turn still encodes");
    assert!(message.get("content").is_none());
    assert_eq!(message["tool_calls"][0]["function"]["name"], "weather");
}

#[test]
fn replays_reasoning_as_thinking_parts_without_protected_payloads() {
    let block = |block| AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![block]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new(
        "command-a",
        vec![
            block(ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: Some("sig_protected".into()),
            }),
            block(ContentBlock::Reasoning {
                summary: vec!["summary".into()],
                content: vec![String::new(), "detail".into()],
                encrypted_content: Some("enc_protected".into()),
            }),
        ],
    );

    let (body, _) = CohereChatV2.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[0]["content"],
        json!([{"type": "thinking", "thinking": "let me think"}])
    );
    assert_eq!(
        messages[1]["content"],
        json!([
            {"type": "thinking", "thinking": "summary"},
            {"type": "thinking", "thinking": "detail"},
        ])
    );
    // 受保护载荷无原生载体，绝不能出现在线上 body 里。
    let body_text = serde_json::to_string(&body).unwrap();
    assert!(!body_text.contains("sig_protected"));
    assert!(!body_text.contains("enc_protected"));
}

#[test]
fn drops_protected_only_assistant_without_breaking_tool_pairing() {
    let protected_only = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::RedactedThinking {
                data: "redacted".into(),
            },
            ContentBlock::Reasoning {
                summary: Vec::new(),
                content: Vec::new(),
                encrypted_content: Some("enc".into()),
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let with_call = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
            data: "redacted".into(),
        }]),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            name: "weather".into(),
            arguments: "{}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    let result = AiItem::function_call_output("call_1", json!("sunny"));
    let mut request = AiRequest::new(
        "command-a",
        vec![AiItem::output_text("hi"), protected_only, with_call, result],
    );
    request.items[0].role = Role::User;

    let (body, _) = CohereChatV2.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    // 空 assistant 条目被整条跳过；带 tool_call 的条目保留以维持配对。
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_1");
    let body_text = serde_json::to_string(&body).unwrap();
    assert!(!body_text.contains("redacted"));
}

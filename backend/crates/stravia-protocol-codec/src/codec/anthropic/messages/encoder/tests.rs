use super::*;

#[test]
fn server_tool_results_keep_the_anthropic_wire_discriminator() {
    let encoded = encode_content_block_for_anthropic(&ContentBlock::ServerToolResult {
        tool_use_id: "srv_123".into(),
        content: serde_json::json!([{"type": "text", "text": "result"}]),
        content_kind: Some(
            stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
        ),
        server_type: Some("web_search_tool_result".into()),
        cache_control: None,
    });

    assert_eq!(encoded[0]["type"], "web_search_tool_result");
    assert_eq!(encoded[0]["tool_use_id"], "srv_123");
    assert_eq!(encoded[0]["content"][0]["text"], "result");
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

    assert_eq!(encoded[0]["type"], "web_search_tool_use");
    assert_eq!(encoded[0]["id"], "srv_123");
    assert_eq!(encoded[0]["name"], "web_search");
    assert_eq!(encoded[0]["input"]["query"], "weather");
}

#[test]
fn synthetic_tool_ids_are_request_local_distinct_and_skip_supplied_ids() {
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::Assistant,
            content: MessageContent::Text(String::new()),
            tool_calls: Some(vec![
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "".into(),
                    name: "first".into(),
                    arguments: "{}".into(),
                },
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "tc_1".into(),
                    name: "external".into(),
                    arguments: "{}".into(),
                },
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "".into(),
                    name: "second".into(),
                    arguments: "{}".into(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        }],
    );

    let (first, _) = AnthropicEncoder.encode_request(&request).expect("encode");
    let (repeated, _) = AnthropicEncoder.encode_request(&request).expect("encode");
    let ids: Vec<&str> = first["messages"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["id"].as_str().unwrap())
        .collect();

    assert_eq!(ids, ["tc_2", "tc_1", "tc_3"]);
    assert_eq!(repeated["messages"], first["messages"]);
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
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "high".into(),
        },
    );
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

fn assistant_blocks_request(blocks: Vec<ContentBlock>) -> AiRequest {
    AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(blocks),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    )
}

#[test]
fn signed_thinking_keeps_native_thinking_block_with_signature() {
    let request = assistant_blocks_request(vec![
        ContentBlock::Thinking {
            thinking: "visible reasoning".into(),
            signature: Some("opaque-sig".into()),
        },
        ContentBlock::Text {
            text: "answer".into(),
            cache_control: None,
        },
    ]);

    let (body, _) = AnthropicEncoder.encode_request(&request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content[0],
        serde_json::json!({
            "type": "thinking",
            "thinking": "visible reasoning",
            "signature": "opaque-sig",
        })
    );
    assert_eq!(
        content[1],
        serde_json::json!({"type": "text", "text": "answer"})
    );
}

#[test]
fn unsigned_thinking_degrades_to_text_block() {
    let request = assistant_blocks_request(vec![ContentBlock::Thinking {
        thinking: "unsigned reasoning".into(),
        signature: None,
    }]);

    let (body, _) = AnthropicEncoder.encode_request(&request).expect("encode");

    // Anthropic 上游拒绝无签名 thinking 块，明文只能出现在 text 块里。
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [serde_json::json!({"type": "text", "text": "unsigned reasoning"})]
    );
}

#[test]
fn encrypted_reasoning_encodes_as_signed_thinking_block() {
    let request = assistant_blocks_request(vec![ContentBlock::Reasoning {
        summary: vec!["summary".into()],
        content: vec!["detail".into()],
        encrypted_content: Some("ciphertext".into()),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(&request).expect("encode");

    // 与 stream.rs 的 Reasoning→Thinking 一致：summary+content 顺序拼接，
    // encrypted_content 作为 signature。
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [serde_json::json!({
            "type": "thinking",
            "thinking": "summarydetail",
            "signature": "ciphertext",
        })]
    );
}

#[test]
fn unencrypted_reasoning_degrades_to_text_blocks_per_segment() {
    let request = assistant_blocks_request(vec![ContentBlock::Reasoning {
        summary: vec!["summary".into(), String::new()],
        content: vec!["detail".into()],
        encrypted_content: None,
    }]);

    let (body, _) = AnthropicEncoder.encode_request(&request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [
            serde_json::json!({"type": "text", "text": "summary"}),
            serde_json::json!({"type": "text", "text": "detail"}),
        ]
    );
}

#[test]
fn redacted_thinking_keeps_native_redacted_block() {
    let request = assistant_blocks_request(vec![ContentBlock::RedactedThinking {
        data: "redacted-data".into(),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(&request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [serde_json::json!({"type": "redacted_thinking", "data": "redacted-data"})]
    );
}

#[test]
fn meta_reasoning_without_signature_degrades_to_text() {
    let request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: Some(
                    stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
                        serde_json::json!({"reasoning_content": "unsigned reasoning"}),
                    ),
                ),
            },
        ],
    );

    let (body, _) = AnthropicEncoder.encode_request(&request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [
            serde_json::json!({"type": "text", "text": "unsigned reasoning"}),
            serde_json::json!({"type": "text", "text": "answer"}),
        ]
    );
    assert!(!body.to_string().contains("\"thinking\""));
}

#[test]
fn blocks_item_with_tool_calls_keeps_calls_and_degrades_unsigned_thinking() {
    let mut request = assistant_blocks_request(vec![
        ContentBlock::Thinking {
            thinking: "unsigned reasoning".into(),
            signature: None,
        },
        ContentBlock::Text {
            text: "answer".into(),
            cache_control: None,
        },
    ]);
    request.items[1].tool_calls = Some(vec![stravia_runtime_contract::protocol::ir::ToolCall {
        id: "call_1".into(),
        name: "glob".into(),
        arguments: "{}".into(),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(&request).expect("encode");

    // thinking 明文降级为 text，tool_calls 以 tool_use 块补齐在末尾。
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [
            serde_json::json!({"type": "text", "text": "unsigned reasoning"}),
            serde_json::json!({"type": "text", "text": "answer"}),
            serde_json::json!({"type": "tool_use", "id": "call_1", "name": "glob", "input": {}}),
        ]
    );
}

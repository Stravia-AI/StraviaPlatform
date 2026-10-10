use super::*;

#[test]
fn generic_anthropic_does_not_invent_summary_controls_for_compatible_targets() {
    use stravia_runtime_contract::thinking::TargetThinkingControl;
    let mut request = crate::codec::anthropic::messages::decoder::AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "claude",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
    let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
    assert!(body.get("thinking").is_none());
    for display in [None, Some("omitted"), Some("concise")] {
        request.reasoning.display = display.map(str::to_owned);
        request.reasoning.target_control = Some(TargetThinkingControl::Budget { value: 2048 });
        let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
        assert!(body["thinking"].get("display").is_none());
    }
    request.reasoning.target_control = Some(TargetThinkingControl::Disabled);
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body["thinking"].get("display").is_none());
}

#[test]
fn server_tool_results_keep_the_anthropic_wire_discriminator() {
    let encoded = encode_content_block_for_anthropic(ContentBlock::ServerToolResult {
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
    let encoded = encode_content_block_for_anthropic(ContentBlock::ServerToolUse {
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
            content: MessageContent::Text(String::new().into()),
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

    let (first, _) = AnthropicEncoder
        .encode_request(request.clone())
        .expect("encode");
    let (repeated, _) = AnthropicEncoder.encode_request(request).expect("encode");
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
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.tool_choice = Some(ToolChoice::None);

    let error = AnthropicEncoder
        .encode_request(request)
        .expect_err("Anthropic cannot represent tool_choice none");
    assert!(error.to_string().contains("tool_choice"));
}

#[test]
fn effort_control_encodes_adaptive_without_replaying_raw_thinking() {
    let mut request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
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
        .encode_request(request)
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
                content: MessageContent::Text("hello".to_owned().into()),
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
            text: "answer".to_owned().into(),
            cache_control: None,
        },
    ]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

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

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    // Anthropic 上游拒绝无签名 thinking 块，明文只能出现在 text 块里。
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [serde_json::json!({"type": "text", "text": "unsigned reasoning"})]
    );
}

#[test]
fn thinking_replay_responses_ciphertext_degrades_per_readable_segment() {
    let request = assistant_blocks_request(vec![ContentBlock::Reasoning {
        summary: vec!["summary-one".into(), "summary-two".into()],
        content: vec!["detail-one".into(), "detail-two".into()],
        encrypted_content: Some("ciphertext".into()),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");
    assert_eq!(
        body["messages"][1]["content"],
        serde_json::json!([
            {"type": "text", "text": "summary-one"},
            {"type": "text", "text": "summary-two"},
            {"type": "text", "text": "detail-one"},
            {"type": "text", "text": "detail-two"}
        ])
    );
}

#[test]
fn unencrypted_reasoning_degrades_to_text_blocks_per_segment() {
    let request = assistant_blocks_request(vec![ContentBlock::Reasoning {
        summary: vec!["summary".into(), String::new()],
        content: vec!["detail".into()],
        encrypted_content: None,
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

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

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

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
                content: MessageContent::Text("hello".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text("answer".to_owned().into()),
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

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

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
            text: "answer".to_owned().into(),
            cache_control: None,
        },
    ]);
    request.items[1].tool_calls = Some(vec![stravia_runtime_contract::protocol::ir::ToolCall {
        id: "call_1".into(),
        name: "glob".into(),
        arguments: "{}".into(),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

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

#[test]
fn consumed_system_items_preserve_newlines_and_concatenate_only_text_blocks() {
    let item = |role, content| AiItem {
        role,
        content,
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut request = AiRequest::new(
        "model",
        vec![
            item(Role::System, MessageContent::Text("系统".to_owned().into())),
            item(
                Role::Developer,
                MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "first".to_owned().into(),
                        cache_control: None,
                    },
                    ContentBlock::Thinking {
                        thinking: "not system text".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "second".to_owned().into(),
                        cache_control: None,
                    },
                ]),
            ),
            item(Role::System, MessageContent::Text(String::new().into())),
            item(Role::User, MessageContent::Text("hello".to_owned().into())),
        ],
    );
    request.instructions = Some("instructions".into());

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(body["system"], "instructions\n系统\nfirstsecond\n");
    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "hello"}]}
        ])
    );
}

#[test]
fn owned_normalization_merges_text_and_blocks_without_changing_shared_history() {
    let mut first = AiItem::output_text("first");
    first.role = Role::User;
    let mut second = AiItem::output_text("");
    second.role = Role::User;
    second.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: " \n ".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::Text {
            text: "second".to_owned().into(),
            cache_control: None,
        },
    ]);
    let request = AiRequest::new("model", vec![first, second]);
    let preserved = request.clone();

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role": "user", "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ]}
        ])
    );
    assert_eq!(preserved.items[0].content.to_text(), "first");
    assert_eq!(preserved.items[1].content.to_text(), " \n second");
}

#[test]
fn owned_raw_system_messages_and_tools_keep_precedence_and_cache_controls() {
    let mut request = AiRequest::new("model", vec![AiItem::output_text("ignored")]);
    request.instructions = Some("ignored instructions".into());
    let system = serde_json::json!([
        {"type": "text", "text": "native system", "cache_control": {"type": "ephemeral"}}
    ]);
    let messages = serde_json::json!([
        {"role": "user", "content": [
            {"type": "text", "text": "native message", "cache_control": {"type": "ephemeral"}}
        ]}
    ]);
    let tools = serde_json::json!([
        {"name": "native_tool", "input_schema": {"type": "object"},
         "cache_control": {"type": "ephemeral"}}
    ]);
    request
        .meta
        .vendor
        .ingress
        .insert("__anthropic_raw_system".into(), system.clone());
    request
        .meta
        .vendor
        .ingress
        .insert("__anthropic_raw_messages".into(), messages.clone());
    request
        .meta
        .vendor
        .ingress
        .insert("__anthropic_raw_tools".into(), tools.clone());

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(body["system"], system);
    assert_eq!(body["messages"], messages);
    assert_eq!(body["tools"], tools);
}

#[test]
fn consuming_instruction_blocks_still_reserves_their_supplied_tool_ids() {
    let mut instruction = AiItem::output_text("");
    instruction.role = Role::System;
    instruction.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "instruction".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::ToolUse {
            id: "tc_1".into(),
            name: "reserved".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    ]);
    let call = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
        id: "".into(),
        name: "actual".into(),
        arguments: "{}".into(),
    });
    let request = AiRequest::new("model", vec![instruction, call]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(body["system"], "instruction");
    assert_eq!(body["messages"][0]["content"][0]["id"], "tc_2");
}

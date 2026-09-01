use super::*;

#[test]
fn openai_to_anthropic_thinking_blocks() {
    let mut resp = IrAiResponse::new("msg_1", "minimax-m2.7");
    resp.push_reasoning("reasoning summary", None);
    resp.push_output_text("hello");
    resp.stop_reason = Some("stop".to_string());
    resp.usage = Usage {
        prompt_tokens: 10,
        completion_tokens: 20,
        ..Usage::default()
    };

    let out = AnthropicResponseFormatter.format_response(&resp);
    let content = out
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content should be array");
    assert_eq!(
        content[0].get("type").and_then(|v| v.as_str()),
        Some("thinking")
    );
    assert_eq!(
        content[0].get("thinking").and_then(|v| v.as_str()),
        Some("reasoning summary")
    );
}
#[test]
fn anthropic_encoder_replays_reasoning_extra_as_thinking_block() {
    let mut extra = std::collections::HashMap::new();
    extra.insert(
        "reasoning_content".to_string(),
        serde_json::Value::String("I should run a shell command.".to_string()),
    );

    let messages = vec![AiItem {
        role: IrRole::Assistant,
        content: IrMessageContent::Text("".to_string()),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".to_string(),
            name: "exec_command".to_string(),
            arguments: "{\"cmd\":\"echo hello\"}".to_string(),
        }]),
        tool_call_id: None,
        meta: Some(serde_json::Value::Object(extra.into_iter().collect())),
    }];
    let mut req = AiRequest::new("deepseek-v4-flash", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let blocks = body["messages"][0]["content"]
        .as_array()
        .expect("assistant content blocks");

    assert_eq!(blocks[0]["type"].as_str(), Some("thinking"));
    assert_eq!(
        blocks[0]["thinking"].as_str(),
        Some("I should run a shell command.")
    );
    assert_eq!(blocks[1]["type"].as_str(), Some("tool_use"));
}
#[test]
fn openai_formatter_sets_tool_calls_finish_reason_when_tool_calls_present() {
    let mut resp = IrAiResponse::new("gen_1", "gemini-2.5-flash");
    resp.extend_tool_calls(vec![ToolCall {
        id: "call_1".to_string(),
        name: "bash".to_string(),
        arguments: "{\"command\":\"ls\"}".to_string(),
    }]);
    resp.stop_reason = Some("stop".to_string());
    resp.usage = Usage {
        prompt_tokens: 44,
        completion_tokens: 13,
        ..Usage::default()
    };

    let out = crate::protocol::codec::openai::compatible::stream::OpenAIResponseFormatter
        .format_response(&resp);
    let finish_reason = out
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str());
    assert_eq!(finish_reason, Some("tool_calls"));
}
#[test]
fn openai_stream_formatter_sets_tool_calls_finish_reason_when_tool_calls_seen() {
    let mut fmt = OpenAIStreamFormatter::new();
    let ai_deltas = vec![
        IrStreamDelta::MessageStart {
            id: "gen_1".to_string(),
            model: "gemini-2.5-flash".to_string(),
        },
        IrStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".to_string(),
            name: "bash".to_string(),
        },
        IrStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"command\":\"ls\"}".to_string(),
        },
        IrStreamDelta::Done {
            stop_reason: "stop".to_string(),
        },
    ];
    let events = fmt.format_deltas(&ai_deltas);
    let last_json = events
        .iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .next_back()
        .expect("has final json");
    let finish_reason = last_json
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str());
    assert_eq!(finish_reason, Some("tool_calls"));
}
#[test]
fn anthropic_tool_result_decodes_to_tool_role() {
    let body = serde_json::json!({
        "model": "claude-sonnet",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "call_abc",
                        "name": "read_file",
                        "input": {"path": "Cargo.toml"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "call_abc",
                        "content": {"ok": true}
                    }
                ]
            }
        ]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("decode anthropic request");
    assert_eq!(req.items.len(), 2);
    assert_eq!(req.items[1].role, IrRole::Tool);
    assert_eq!(req.items[1].tool_call_id.as_deref(), Some("call_abc"));
}
#[test]
fn anthropic_multi_tool_result_decodes_to_multiple_tool_messages() {
    let body = serde_json::json!({
        "model": "claude-sonnet",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "call_a", "name": "read_file", "input": {"path":"a"} },
                    { "type": "tool_use", "id": "call_b", "name": "read_file", "input": {"path":"b"} }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "call_a", "content": {"ok": true} },
                    { "type": "tool_result", "tool_use_id": "call_b", "content": {"ok": true} }
                ]
            }
        ]
    });
    let req = AnthropicDecoder
        .decode_request(body)
        .expect("decode anthropic request");
    assert_eq!(req.items.len(), 3);
    assert_eq!(req.items[1].role, IrRole::Tool);
    assert_eq!(req.items[2].role, IrRole::Tool);
    assert_eq!(req.items[1].tool_call_id.as_deref(), Some("call_a"));
    assert_eq!(req.items[2].tool_call_id.as_deref(), Some("call_b"));
}
#[test]
fn anthropic_thinking_block_round_trips_with_signature() {
    let body = serde_json::json!({
        "model": "claude-sonnet",
        "max_tokens": 1024,
        "messages": [{
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "review prior tool output",
                    "signature": "sig_123"
                },
                {
                    "type": "text",
                    "text": "Ready."
                }
            ]
        }]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("decode anthropic request");
    let IrMessageContent::Blocks(blocks) = &req.items[0].content else {
        panic!("thinking must remain a structured block");
    };
    assert!(matches!(
        &blocks[0],
        IrContentBlock::Thinking { thinking, signature }
            if thinking == "review prior tool output" && signature.as_deref() == Some("sig_123")
    ));

    let (encoded, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic request");
    let block = encoded
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|messages| messages.first())
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
        .and_then(|content| content.first())
        .expect("first content block");
    assert_eq!(block.get("type").and_then(|v| v.as_str()), Some("thinking"));
    assert_eq!(
        block.get("thinking").and_then(|v| v.as_str()),
        Some("review prior tool output")
    );
    assert_eq!(
        block.get("signature").and_then(|v| v.as_str()),
        Some("sig_123")
    );
}
#[test]
fn anthropic_mixed_assistant_history_encodes_as_ordered_responses_items() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "gpt-5.6-luna",
            "max_tokens": 1024,
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "", "signature": "opaque"},
                        {"type": "text", "text": "I will inspect the repository."},
                        {
                            "type": "tool_use",
                            "id": "call_1",
                            "name": "todowrite",
                            "input": {"todos": []}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "call_1",
                        "content": "[]"
                    }]
                }
            ]
        }))
        .expect("Anthropic request");

    let encoded = pair
        .encode_request(&request)
        .expect("mixed assistant history must remain representable")
        .body;
    let input = encoded["input"].as_array().expect("Responses input");

    assert_eq!(
        input
            .iter()
            .map(|item| item["type"].as_str().expect("item type"))
            .collect::<Vec<_>>(),
        [
            "reasoning",
            "message",
            "function_call",
            "function_call_output"
        ]
    );
    assert_eq!(input[0]["encrypted_content"], "opaque");
    assert_eq!(input[0]["summary"], json!([]));
}
#[test]
fn openai_encoder_injects_synthetic_tool_call_before_orphan_tool_result() {
    let messages = vec![AiItem {
        role: IrRole::Tool,
        content: IrMessageContent::Text("{\"ok\":true}".to_string()),
        tool_calls: None,
        tool_call_id: Some("call_orphan_1".to_string()),
        meta: None,
    }];
    let mut req = AiRequest::new("minimax-m2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(
        messages[1].get("role").and_then(|v| v.as_str()),
        Some("tool")
    );
    assert_eq!(
        messages[1].get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_orphan_1")
    );
}
#[test]
fn openai_encoder_injects_adjacent_tool_call_for_non_adjacent_match() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("will call".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_x".to_string(),
                name: "ls".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::User,
            content: IrMessageContent::Text("intermediate".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_x".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("minimax-m2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[2].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(
        messages[3].get("role").and_then(|v| v.as_str()),
        Some("tool")
    );
    let tool_id = messages[3]
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(!tool_id.is_empty());
    let assistant_call_id = messages[2]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(assistant_call_id, tool_id);
}
#[test]
fn openai_encoder_drops_intermediate_assistant_text_before_tool_result() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("plan".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_keep".to_string(),
                name: "exec_command".to_string(),
                arguments: "{\"command\":\"ls -la\"}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("extra text".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"stdout\":\"...\"}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_keep".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    // intermediate assistant text should be dropped to keep tool_result adjacent
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(
        messages[1]
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|tc| tc.get("id"))
            .and_then(|v| v.as_str()),
        Some("call_keep")
    );
    assert_eq!(
        messages[2].get("role").and_then(|v| v.as_str()),
        Some("tool")
    );
    assert_eq!(
        messages[2].get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_keep")
    );
}
#[test]
fn openai_encoder_remaps_duplicate_tool_call_ids() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_dup".to_string(),
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_dup".to_string(),
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_dup".to_string()),
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_dup".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    let ids: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            m.get("tool_calls")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
        })
        .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);

    let tool_ids: Vec<String> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
        .filter_map(|m| {
            m.get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    assert_eq!(tool_ids.len(), 2);
    assert!(ids.contains(&tool_ids[0]));
    assert!(ids.contains(&tool_ids[1]));
}
#[test]
fn anthropic_encoder_maps_required_tool_choice_to_any() {
    let messages = vec![AiItem {
        role: IrRole::User,
        content: IrMessageContent::Text("hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let tools = Some(vec![ToolSpec {
        name: "exec_command".to_string(),
        description: Some("Execute command".to_string()),
        parameters: serde_json::json!({"type":"object","properties":{"command":{"type":"string"}}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.tools = tools;
    req.tool_choice = Some(crate::protocol::ir::ToolChoice::Raw(serde_json::json!(
        "required"
    )));
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    assert_eq!(
        body.get("tool_choice")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str()),
        Some("any")
    );
}
#[test]
fn anthropic_encoder_maps_function_tool_choice_to_tool_name() {
    let messages = vec![AiItem {
        role: IrRole::User,
        content: IrMessageContent::Text("hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let tools = Some(vec![ToolSpec {
        name: "exec_command".to_string(),
        description: Some("Execute command".to_string()),
        parameters: serde_json::json!({"type":"object","properties":{"command":{"type":"string"}}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.tools = tools;
    req.tool_choice = Some(crate::protocol::ir::ToolChoice::Raw(serde_json::json!({
        "type":"function",
        "function":{"name":"exec_command"}
    })));
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    assert_eq!(
        body.get("tool_choice")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str()),
        Some("tool")
    );
    assert_eq!(
        body.get("tool_choice")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str()),
        Some("exec_command")
    );
}
#[test]
fn anthropic_encoder_merges_consecutive_roles_and_drops_empty_text() {
    let messages = vec![
        AiItem {
            role: IrRole::User,
            content: IrMessageContent::Text("first".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::User,
            content: IrMessageContent::Text("second".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("tool".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("result".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            meta: None,
        },
    ];
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].get("role").and_then(|v| v.as_str()), Some("user"));
    assert_eq!(
        msgs[1].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[2].get("role").and_then(|v| v.as_str()), Some("user"));

    let first_blocks = msgs[0]
        .get("content")
        .and_then(|v| v.as_array())
        .expect("first content blocks");
    assert_eq!(first_blocks.len(), 2);
    assert_eq!(
        first_blocks[0].get("text").and_then(|v| v.as_str()),
        Some("first")
    );
    assert_eq!(
        first_blocks[1].get("text").and_then(|v| v.as_str()),
        Some("second")
    );
}
#[test]
fn anthropic_encoder_normalizes_tool_use_ids_for_tool_and_result() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_function_abc_1".to_string(),
                name: "glob".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "call_function_abc_1".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call_function_abc_1".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        name: "glob".to_string(),
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.generation.max_tokens = Some(256);
    req.tools = tools;
    req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    let (body, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    let tool_use_id = msgs[0]
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|b| b.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_result_id = msgs[1]
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|b| b.get("tool_use_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(tool_use_id.starts_with("toolu_"));
    assert_eq!(tool_use_id, tool_result_id);
}
#[test]
fn openai_encoder_remaps_reused_tool_result_id_with_synthetic_adjacent_call() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_same".to_string(),
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("ok1".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_same".to_string()),
            meta: None,
        },
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("intermediate".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("ok2".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_same".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        name: "exec_command".to_string(),
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("gpt-4o-mini", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let (body, _) = OpenAIEncoder.encode_request(&req).expect("encode");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    let mut tool_ids: Vec<String> = Vec::new();
    for msg in msgs {
        if msg.get("role").and_then(|v| v.as_str()) == Some("tool") {
            let id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            assert!(!id.is_empty());
            tool_ids.push(id);
        }
    }
    assert_eq!(tool_ids.len(), 2);
    assert_ne!(tool_ids[0], tool_ids[1]);
}
#[test]
fn openai_encoder_rewrites_multi_tool_call_history_to_adjacent_pairs() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("".to_string()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_a".to_string(),
                    name: "Glob".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    id: "call_b".to_string(),
                    name: "Bash".to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("r1".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_a".to_string()),
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("r2".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_b".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        name: "Glob".to_string(),
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(ANTHROPIC_MESSAGES_2023_06_01);

    let (body, _) = OpenAIEncoder.encode_request(&req).expect("encode");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    assert_eq!(msgs.len(), 4);
    assert_eq!(
        msgs[0].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[1].get("role").and_then(|v| v.as_str()), Some("tool"));
    assert_eq!(
        msgs[2].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[3].get("role").and_then(|v| v.as_str()), Some("tool"));
    let id1 = msgs[1]
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let id2 = msgs[3]
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let prev1 = msgs[0]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let prev2 = msgs[2]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(id1, prev1);
    assert_eq!(id2, prev2);
}
#[test]
fn openai_encoder_preserves_reasoning_content_across_parallel_tool_calls() {
    // Regression: when an assistant message has multiple parallel tool calls
    // AND extra fields (e.g. reasoning_content from DeepSeek thinking mode),
    // each synthetic assistant message created by normalize_messages_for_openai
    // must carry forward the extra fields. std::mem::take() only works for the
    // first extraction — subsequent extractions get HashMap::new(), dropping
    // reasoning_content and causing HTTP 400 from DeepSeek.
    use std::collections::HashMap;
    let mut extra = HashMap::new();
    extra.insert(
        "reasoning_content".to_string(),
        serde_json::Value::String("I need to check the time in Tokyo and Paris.".to_string()),
    );

    let messages = vec![
        AiItem {
            role: IrRole::User,
            content: IrMessageContent::Text("What time is it in Tokyo and Paris?".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        // Single assistant message with TWO parallel tool calls + reasoning_content
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text("".to_string()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_tokyo".to_string(),
                    name: "get_time".to_string(),
                    arguments: "{\"location\":\"Tokyo\"}".to_string(),
                },
                ToolCall {
                    id: "call_paris".to_string(),
                    name: "get_time".to_string(),
                    arguments: "{\"location\":\"Paris\"}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: Some(serde_json::Value::Object(extra.into_iter().collect())),
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("10:30 JST".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_tokyo".to_string()),
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("03:30 CEST".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_paris".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        name: "get_time".to_string(),
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{"location":{"type":"string"}}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("deepseek-v4-flash", messages);
    req.stream = StreamConfig {
        enabled: true,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let (body, _) = OpenAIEncoder
        .encode_request(&req)
        .expect("encode openai body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    // We expect: [user, assistant(call_tokyo, reasoning_content), tool(call_tokyo),
    //             assistant(call_paris, reasoning_content), tool(call_paris)]
    // The original assistant with both calls gets pruned (empty content, no calls left).
    assert_eq!(
        msgs.len(),
        5,
        "expected 5 messages: user + 2 assistant+tool pairs"
    );

    // Every assistant message must carry reasoning_content
    for (i, msg) in msgs.iter().enumerate() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "assistant" {
            let rc = msg.get("reasoning_content").and_then(|v| v.as_str());
            assert!(
                rc.is_some(),
                "assistant message at index {} is missing reasoning_content. \
                 Bug: std::mem::take() on source.extra drops it after first extraction. \
                 Full msg: {:?}",
                i,
                msg
            );
            assert_eq!(
                rc,
                Some("I need to check the time in Tokyo and Paris."),
                "assistant[{}] has wrong reasoning_content value",
                i
            );
        }
    }
}
#[test]
fn anthropic_to_openai_thinking_round_trip_carries_reasoning_content() {
    // Regression for cross-protocol Anthropic Messages → OpenAI chat/completions:
    // when the client (Claude Code) re-sends an assistant turn containing
    // `thinking` + parallel `tool_use` blocks followed by `tool_result`s,
    // upstreams in thinking mode (Xiaomi Mimo / DeepSeek / etc.) require the
    // assistant message that carries `tool_calls` to also carry the original
    // `reasoning_content`. Otherwise they return:
    //   400 "The reasoning_content in the thinking mode must be passed back."
    //
    // The thinking text must be bridged through `meta.reasoning_content` so the
    // OpenAI encoder emits it on every split assistant message produced by
    // `normalize_messages_for_openai`.
    let raw = serde_json::json!({
        "model": "mimo-v2.5-pro",
        "max_tokens": 1024,
        "stream": true,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "ls the project"}]},
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "thinking",
                        "thinking": "The user wants me to list the project.",
                        "signature": ""
                    },
                    {
                        "type": "tool_use",
                        "id": "call_a",
                        "name": "Bash",
                        "input": {"command": "ls -la"}
                    },
                    {
                        "type": "tool_use",
                        "id": "call_b",
                        "name": "Bash",
                        "input": {"command": "find . -maxdepth 2"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "call_a", "content": "out-a"},
                    {"type": "tool_result", "tool_use_id": "call_b", "content": "out-b"}
                ]
            }
        ]
    });

    let ir = AnthropicDecoder
        .decode_request(raw)
        .expect("decode anthropic request");

    let asst_idx = ir
        .items
        .iter()
        .position(|m| m.role == IrRole::Assistant)
        .expect("assistant message present");
    let asst_meta = ir.items[asst_idx]
        .meta
        .as_ref()
        .and_then(|v| v.get("reasoning_content"))
        .and_then(|v| v.as_str());
    assert_eq!(
        asst_meta,
        Some("The user wants me to list the project."),
        "anthropic decoder must surface thinking text as meta.reasoning_content; \
         got meta={:?}",
        ir.items[asst_idx].meta,
    );

    let (body, _) = OpenAIEncoder
        .encode_request(&ir)
        .expect("encode openai body");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    let assistant_msgs: Vec<&serde_json::Value> = msgs
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        .collect();
    assert!(
        !assistant_msgs.is_empty(),
        "expected at least one assistant message in encoded body, got: {:?}",
        msgs
    );

    for (i, m) in assistant_msgs.iter().enumerate() {
        let rc = m.get("reasoning_content").and_then(|v| v.as_str());
        assert_eq!(
            rc,
            Some("The user wants me to list the project."),
            "assistant[{}] missing or wrong reasoning_content: {:?}",
            i,
            m
        );

        // Thinking block must NOT also leak into content as plain text — that
        // would duplicate reasoning across two channels.
        if let Some(arr) = m.get("content").and_then(|v| v.as_array()) {
            for part in arr {
                let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                assert!(
                    !text.contains("The user wants me to list the project."),
                    "thinking text leaked into content array: {:?}",
                    m
                );
            }
        }
    }
}
#[test]
fn openai_encoder_drops_orphan_assistant_tool_calls_without_results() {
    let messages = vec![
        AiItem {
            role: IrRole::System,
            content: IrMessageContent::Text("sys".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_old_1".to_string(),
                    name: String::new(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    id: "call_old_2".to_string(),
                    name: "list_directory".to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_new".to_string(),
                name: "glob".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Text("{\"ok\":true}".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_new".to_string()),
            meta: None,
        },
    ];
    let tools = Some(vec![ToolSpec {
        name: "glob".to_string(),
        description: None,
        parameters: serde_json::json!({"type":"object","properties":{}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("MiniMax-M2.7", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    let (body, _) = OpenAIEncoder.encode_request(&req).expect("encode");
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].get("role").and_then(|v| v.as_str()), Some("system"));
    assert_eq!(
        msgs[1].get("role").and_then(|v| v.as_str()),
        Some("assistant")
    );
    assert_eq!(msgs[2].get("role").and_then(|v| v.as_str()), Some("tool"));
    let call_id = msgs[1]
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(call_id, "call_new");
}
#[test]
fn codex_parallel_calls_with_intermediate_text_anthropic_egress() {
    let body = serde_json::json!({
        "model": "deepseek-v4-flash",
        "input": [
            {"type": "message", "role": "user",
                "content": [{"type":"input_text","text":"do parallel work"}]},
            {"type": "function_call", "call_id": "call_00_A",
                "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call", "call_id": "call_00_B",
                "name": "exec_command", "arguments": "{\"cmd\":\"pwd\"}"},
            {"type": "message", "role": "assistant",
                "content": [{"type":"output_text","text":"running both"}]},
            {"type": "function_call_output", "call_id": "call_00_A",
                "output": "{\"stdout\":\"a\"}"},
            {"type": "function_call_output", "call_id": "call_00_B",
                "output": "{\"stdout\":\"b\"}"},
        ]
    });
    let mut req: AiRequest = ResponsesDecoder.decode_request(body).expect("decode");
    normalize_request_tool_results(&mut req);

    let (encoded, _) = AnthropicEncoder
        .encode_request(&req)
        .expect("encode anthropic body");
    let msgs = encoded
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    for (i, m) in msgs.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let blocks = m
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let tool_use_ids: Vec<String> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .filter_map(|b| b.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        if tool_use_ids.is_empty() {
            continue;
        }

        assert_eq!(
            blocks
                .last()
                .and_then(|b| b.get("type"))
                .and_then(|v| v.as_str()),
            Some("tool_use"),
            "assistant message {i} must end with tool_use; got blocks={blocks:?}",
        );

        let next = msgs.get(i + 1).expect("must have next user msg");
        assert_eq!(next.get("role").and_then(|v| v.as_str()), Some("user"));
        let next_blocks = next
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let result_ids: Vec<String> = next_blocks
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
            .filter_map(|b| {
                b.get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect();
        for id in &tool_use_ids {
            assert!(
                result_ids.contains(id),
                "tool_use {id} has no matching tool_result in next user message; got {next_blocks:?}",
            );
        }
    }
}
#[test]
fn anthropic_inline_system_role_decodes_without_error() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 32000,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "帮我查下当前目录结构"}
                ]
            },
            {
                "role": "system",
                "content": "SessionStart hook additional context: you have superpowers."
            },
            {
                "role": "user",
                "content": "follow up question"
            }
        ]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("should not fail on inline system role");

    // Inline system decoded to Role::System at original position (index 1).
    assert_eq!(req.items.len(), 3);
    assert_eq!(req.items[0].role, IrRole::User);
    assert_eq!(req.items[1].role, IrRole::System);
    assert_eq!(req.items[2].role, IrRole::User);

    // System content is preserved.
    let sys_text = req.items[1].content.to_text();
    assert!(
        sys_text.contains("superpowers"),
        "system content must be preserved, got: {sys_text}"
    );
}

/// Inline system with content blocks (cache_control present, mirroring the real log).
#[test]
fn anthropic_inline_system_role_with_blocks_decodes() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 32000,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "first question"},
                    {"type": "text", "text": "cached part", "cache_control": {"type": "ephemeral"}}
                ]
            },
            {
                "role": "system",
                "content": [
                    {"type": "text", "text": "injected system context from skill"}
                ]
            },
            {
                "role": "assistant",
                "content": "sure"
            }
        ]
    });

    let req = AnthropicDecoder
        .decode_request(body)
        .expect("should handle inline system with surrounding cache_control blocks");

    assert_eq!(req.items.len(), 3);
    assert_eq!(req.items[1].role, IrRole::System);
    let sys_text = req.items[1].content.to_text();
    assert!(sys_text.contains("injected system context"));
}

/// Anthropic encoder re-merges inline system into top-level system field,
/// keeping the messages array clean for strict downstream endpoints.
#[test]
fn anthropic_inline_system_role_encodes_into_top_level_system() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "system": "base system prompt",
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "system", "content": "mid-conversation system injection"},
            {"role": "assistant", "content": "hi there"},
            {"role": "user", "content": "next turn"}
        ]
    });

    let ir = AnthropicDecoder.decode_request(body).expect("decode");

    let (encoded, _) = AnthropicEncoder.encode_request(&ir).expect("encode");

    // Top-level system should contain both base and injected text.
    let system_val = encoded.get("system").expect("system field must exist");
    let system_str = system_val.as_str().expect("system must be string");
    assert!(
        system_str.contains("base system prompt"),
        "base system missing"
    );
    assert!(
        system_str.contains("mid-conversation system injection"),
        "injected system missing"
    );

    // messages must not contain any system role (strict endpoint safe).
    let msgs = encoded["messages"].as_array().expect("messages array");
    for m in msgs {
        assert_ne!(
            m["role"].as_str(),
            Some("system"),
            "re-encoded messages must not contain system role"
        );
    }
    // user and assistant turns are preserved.
    assert_eq!(msgs.len(), 3, "user + assistant + user");
}

/// Unknown roles (not system/user/assistant) still produce a hard error.
#[test]
fn anthropic_truly_unknown_role_still_errors() {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "garbage_role", "content": "unexpected"}
        ]
    });

    let result = AnthropicDecoder.decode_request(body);
    assert!(
        result.is_err(),
        "truly unknown role must still be rejected with an error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unknown Anthropic role: garbage_role"),
        "error message must identify the bad role, got: {err}"
    );
}
#[test]
fn anthropic_to_openai_strips_tool_use_from_content_array() {
    // Regression for Anthropic Messages → OpenAI Chat Completions cross-protocol
    // conversion: the Anthropic decoder carries an assistant `tool_use` BOTH in
    // `content` blocks AND in `tool_calls`. The OpenAI encoder must NOT emit the
    // ToolUse block into the `content` array (OpenAI only accepts text/image/...
    // part types there) — otherwise strict upstreams reject with:
    //   400 "messages[N]: unknown variant `function`, expected `text`".
    // The tool call must instead live solely in the `tool_calls` array.
    let raw = serde_json::json!({
        "model": "deepseek-v4-flash",
        "max_tokens": 1024,
        "tools": [{
            "name": "Bash",
            "description": "run a shell command",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}}
            }
        }],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "list the project files"}]},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Sure, listing files."},
                    {"type": "tool_use", "id": "call_a", "name": "Bash", "input": {"command": "ls -la"}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "call_a", "content": "file1\nfile2"}
                ]
            }
        ]
    });

    let ir = AnthropicDecoder
        .decode_request(raw)
        .expect("decode anthropic request");

    let (body, _) = OpenAIEncoder
        .encode_request(&ir)
        .expect("encode openai body");

    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    // (a) No assistant content part may carry type:"function" (the bug).
    for (i, m) in msgs.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(arr) = m.get("content").and_then(|v| v.as_array()) {
            for part in arr {
                let ty = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                assert_ne!(
                    ty, "function",
                    "assistant[{i}] content leaked a `function` part into content array: {part:?}"
                );
            }
        }
    }

    // (b) The tool call survives intact in tool_calls (id / name / arguments).
    let call = msgs
        .iter()
        .filter_map(|m| m.get("tool_calls").and_then(|v| v.as_array()))
        .flatten()
        .find(|tc| tc.get("id").and_then(|v| v.as_str()) == Some("call_a"))
        .expect("tool_call call_a must be preserved in tool_calls");
    assert_eq!(call.get("type").and_then(|v| v.as_str()), Some("function"));
    assert_eq!(
        call.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str()),
        Some("Bash")
    );
    let args = call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        args.contains("ls -la"),
        "tool call arguments must survive, got: {args}"
    );

    // (c) tool_result message is correlated back to the same id.
    let tool_msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
        .expect("tool message present");
    assert_eq!(
        tool_msg.get("tool_call_id").and_then(|v| v.as_str()),
        Some("call_a")
    );

    // (d) tool definitions survive.
    let tools = body
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("tools array");
    assert_eq!(
        tools[0].get("type").and_then(|v| v.as_str()),
        Some("function")
    );
    assert_eq!(
        tools[0]
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str()),
        Some("Bash")
    );
}

// The dated contract rejects malformed function calls instead of preserving
// undocumented vendor quirks in the canonical graph.

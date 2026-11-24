use super::*;

#[test]
fn cohere_explicit_disabled_reasoning_is_not_default_enabled() {
    use stravia_runtime_contract::thinking::ThinkingLevel;
    let request = CohereChatV2
        .decode_request(json!({
            "model": "command-a-reasoning", "messages": [],
            "thinking": {"type": "disabled"}
        }))
        .unwrap();
    assert!(!request.reasoning.is_enabled());
    assert_eq!(request.reasoning.level, Some(ThinkingLevel::Off));
    assert_eq!(
        CohereChatV2.encode_request(request).unwrap().0["thinking"],
        json!({"type": "disabled"})
    );
}

#[test]
fn cohere_native_enabled_maps_effort_to_static_egress_budget() {
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
    let mut request = CohereChatV2
        .decode_request(json!({
            "model": "command-a-reasoning", "messages": [],
            "thinking": {"type": "enabled"}
        }))
        .unwrap();
    assert_eq!(request.reasoning.level, Some(ThinkingLevel::High));
    assert_eq!(
        CohereChatV2.encode_request(request.clone()).unwrap().0["thinking"],
        json!({"type": "enabled", "token_budget": 16384})
    );
    request.reasoning.target_control = Some(TargetThinkingControl::Effort {
        value: "low".into(),
    });
    assert_eq!(
        CohereChatV2.encode_request(request.clone()).unwrap().0["thinking"],
        json!({"type": "enabled", "token_budget": 2048})
    );
    request.reasoning.target_control = Some(TargetThinkingControl::Effort {
        value: "none".into(),
    });
    assert_eq!(
        CohereChatV2.encode_request(request).unwrap().0["thinking"],
        json!({"type": "disabled"})
    );
}

#[test]
fn consumes_canonical_sampling_and_discards_unmapped_controls() {
    let mut request = CohereChatV2
        .decode_request(json!({
            "model": "command-a",
            "messages": [{"role": "user", "content": "hello"}],
            "k": 0, "p": 0.5, "logprobs": false,
            "priority": 0, "strict_tools": false,
            "documents": [{"id": "doc", "data": {"text": "reference", "custom": {"nested": true}}}],
            "citation_options": {"mode": "OFF", "unknown": true},
            "safety_mode": "STRICT",
            "future_parameter": true
        }))
        .unwrap();
    assert_eq!(request.parameters.top_k, Some(0));
    assert_eq!(request.parameters.logprobs, None);
    assert!(request.parameters.resets(ParameterReset::Logprobs));
    let MessageContent::Blocks(documents) = &request.items[1].content else {
        panic!("expected document blocks");
    };
    assert!(matches!(&documents[0], ContentBlock::Document {
        source: DocumentSource::PlainText { data }, ..
    } if data == "reference"));
    request.parameters.min_p = Some(0.0);
    request.parameters.candidate_count = Some(0);
    let (body, _) = CohereChatV2.encode_request(request.clone()).unwrap();
    assert_eq!(body["k"], 0);
    assert!(body.get("logprobs").is_none());
    assert!(body.get("priority").is_none());
    assert!(body.get("strict_tools").is_none());
    assert_eq!(body["documents"], json!([{"data": {"text": "reference"}}]));
    assert!(body.get("citation_options").is_none());
    assert!(body.get("safety_mode").is_none());
    assert!(body.get("min_p").is_none());
    assert!(body.get("n").is_none());
    assert!(body.get("future_parameter").is_none());
    let (google, _) = stravia_protocol_codec::codec::google::gemini::generate_content::GoogleGenerateContentV1Beta.encode_request(request).unwrap();
    assert_eq!(google["generationConfig"]["topK"], 0);
    assert!(google["generationConfig"].get("responseLogprobs").is_none());
    assert!(google.get("priority").is_none());
    assert!(google.get("documents").is_none());
}

#[test]
fn imports_and_derives_request_wide_tool_strictness() {
    for strict in [false, true] {
        let mut request = CohereChatV2
            .decode_request(json!({
                "model": "command-a", "messages": [],
                "strict_tools": strict,
                "tools": [
                    {"type": "function", "function": {"name": "first", "parameters": {}}},
                    {"type": "function", "function": {"name": "second", "parameters": {}}}
                ]
            }))
            .unwrap();
        assert!(
            request
                .tools
                .as_ref()
                .unwrap()
                .iter()
                .all(|tool| tool.strict == Some(strict))
        );
        assert_eq!(
            CohereChatV2.encode_request(request.clone()).unwrap().0["strict_tools"],
            strict
        );
        request.tools.as_mut().unwrap()[0].strict = Some(true);
        request.tools.as_mut().unwrap()[1].strict = None;
        let (body, _) = CohereChatV2.encode_request(request.clone()).unwrap();
        assert_eq!(body["strict_tools"], false);
        assert!(body["tools"][0]["function"].get("strict").is_none());
        assert!(body["tools"][0]["function"].get("description").is_none());
        request.tool_choice = Some(ToolChoice::Named {
            name: "first".into(),
        });
        let (body, _) = CohereChatV2.encode_request(request).unwrap();
        assert_eq!(body["strict_tools"], true);
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["tool_choice"], "REQUIRED");
    }
}

#[test]
fn canonical_logprobs_enable_boolean_wire_output() {
    let mut request = CohereChatV2
        .decode_request(json!({
            "model": "command-a", "messages": [], "logprobs": true
        }))
        .unwrap();
    assert_eq!(request.parameters.logprobs, Some(0));
    assert!(!request.parameters.resets(ParameterReset::Logprobs));
    for count in [0, 5] {
        request.parameters.logprobs = Some(count);
        assert_eq!(
            CohereChatV2.encode_request(request.clone()).unwrap().0["logprobs"],
            true
        );
    }
}

#[test]
fn thinking_budget_comes_from_canonical_effort_ladder_at_egress() {
    use stravia_runtime_contract::thinking::ThinkingLevel;
    let mut request = CohereChatV2
        .decode_request(json!({
            "model": "command-a-reasoning", "messages": [],
            "thinking": {"type": "enabled", "token_budget": 1024}
        }))
        .unwrap();
    assert_eq!(request.reasoning.level, Some(ThinkingLevel::High));
    request.reasoning.level = Some(ThinkingLevel::Medium);
    assert_eq!(
        CohereChatV2.encode_request(request).unwrap().0["thinking"],
        json!({"type": "enabled", "token_budget": 8192})
    );
}

#[test]
fn ignores_null_controls_and_unrepresentable_documents() {
    let request = CohereChatV2
        .decode_request(json!({
            "model": "command-a", "messages": [], "thinking": null, "logprobs": null,
            "response_format": null, "tools": null, "tool_choice": null,
            "documents": [
                {"data": {"custom": {"nested": true}}},
                {"data": {"text": 42}},
                {"data": "reference"},
                {"data": {"text": "body", "title": "Title", "extra": true}}
            ]
        }))
        .unwrap();
    assert!(request.tools.is_none());
    assert!(request.tool_choice.is_none());
    assert!(request.response_format.is_none());
    assert!(request.parameters.resets(ParameterReset::Logprobs));
    assert!(request.parameters.resets(ParameterReset::Reasoning));
    assert!(request.parameters.resets(ParameterReset::ResponseFormat));
    let (body, _) = CohereChatV2.encode_request(request).unwrap();
    assert_eq!(
        body["documents"],
        json!([
            {"data": {"text": "reference"}},
            {"data": {"text": "body", "title": "Title"}}
        ])
    );
    assert_eq!(body["messages"], json!([]));
    for key in ["thinking", "response_format", "tools", "tool_choice"] {
        assert!(body.get(key).is_none());
    }
}

#[test]
fn ignores_unsupported_canonical_tool_choices() {
    for choice in [
        ToolChoice::Allowed {
            names: vec!["weather".into()],
            required: true,
        },
        ToolChoice::Native(stravia_runtime_contract::protocol::ir::NativeToolKind::WebSearch),
    ] {
        let mut request = AiRequest::new("command-a", vec![]);
        request.tool_choice = Some(choice);
        assert!(
            CohereChatV2
                .encode_request(request)
                .unwrap()
                .0
                .get("tool_choice")
                .is_none()
        );
    }
}

#[test]
fn cohere_official_response_formats_round_trip_without_trimming_schema() {
    for format in [
        json!({"type": "text"}),
        json!({"type": "json_object"}),
        json!({"type": "json_object", "json_schema": {
            "type": "object", "properties": {"custom": {"type": "string"}}
        }}),
    ] {
        let request = CohereChatV2
            .decode_request(json!({
                "model": "command-a", "messages": [], "response_format": format
            }))
            .unwrap();
        assert_eq!(
            CohereChatV2.encode_request(request).unwrap().0["response_format"],
            format
        );
    }
}

#[test]
fn projects_nested_cohere_results_without_disguising_media() {
    use stravia_runtime_contract::protocol::ir::ToolResultContentKind;
    let mixed = json!([
        {"type": "text", "text": "keep"},
        {"type": "input_text", "text": " more"},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aW1n"}},
        {"type": "unknown", "text": "drop"}
    ]);
    for (kind, content, expected) in [
        (
            ToolResultContentKind::ContentBlocks,
            mixed.clone(),
            Some("keep more".to_owned()),
        ),
        (
            ToolResultContentKind::Json,
            mixed.clone(),
            Some(serde_json::to_string(&mixed).unwrap()),
        ),
        (
            ToolResultContentKind::Json,
            json!({"type": "image", "business": true}),
            Some("{\"type\":\"image\",\"business\":true}".to_owned()),
        ),
        (
            ToolResultContentKind::ContentBlocks,
            json!([{"type": "image"}, {"type": "unknown", "text": "drop"}]),
            None,
        ),
    ] {
        let mut item = AiItem::function_call_output("call_1", json!("unused"));
        item.content = MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content_kind: Some(kind),
            content,
            is_error: Some(true),
            cache_control: None,
        }]);
        let (body, _) = CohereChatV2
            .encode_request(AiRequest::new("command-a", vec![item]))
            .unwrap();
        if let Some(expected) = expected {
            assert_eq!(body["messages"][0]["tool_call_id"], "call_1");
            let actual = body["messages"][0]["content"].as_str().unwrap();
            if kind == ToolResultContentKind::Json {
                assert_eq!(
                    serde_json::from_str::<Value>(actual).unwrap(),
                    serde_json::from_str::<Value>(&expected).unwrap()
                );
            } else {
                assert_eq!(actual, expected);
            }
        } else {
            assert_eq!(body["messages"], json!([]));
        }
    }
}

#[test]
fn encodes_cohere_tool_schema_without_openai_shape() {
    let mut request = AiRequest::new("command-a", vec![AiItem::output_text("hello")]);
    request.items[0].role = Role::User;
    request.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "weather".into(),
        description: Some("Get weather".into()),
        parameters: json!({"type": "object"}),
        strict: None,
        defer_loading: None,
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

#[test]
fn omits_file_id_images_preserving_text() {
    let mut item = AiItem::output_text("keep");
    item.role = Role::User;
    item.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "keep".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::Image {
            source: MediaSource::FileId {
                file_id: "file_1".into(),
                detail: None,
            },
            detail: None,
            cache_control: None,
        },
    ]);
    let request = AiRequest::new("command-a", vec![item]);
    let (body, _) = CohereChatV2.encode_request(request).unwrap();
    assert!(body.get("tool_choice").is_none());
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type": "text", "text": "keep"}])
    );
}

#[test]
fn skips_role_specific_unsupported_blocks_preserving_text() {
    for role in [
        Role::System,
        Role::Developer,
        Role::User,
        Role::Assistant,
        Role::Tool,
    ] {
        let item = AiItem {
            role,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "keep".to_owned().into(),
                    cache_control: None,
                },
                ContentBlock::Image {
                    source: MediaSource::FileId {
                        file_id: "file_1".into(),
                        detail: None,
                    },
                    detail: None,
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        };
        let message = encode_message(&item).unwrap().unwrap();
        let expected = if role == Role::User {
            json!([{"type": "text", "text": "keep"}])
        } else {
            json!("keep")
        };
        assert_eq!(message["content"], expected);
    }
}

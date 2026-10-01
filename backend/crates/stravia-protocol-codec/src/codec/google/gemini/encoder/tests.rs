use super::*;

#[test]
fn chat_history_and_responses_tool_result_keep_gemini_call_pairing() {
    let response = crate::codec::openai::compatible::stream::OpenAIResponseParser
        .parse_response(serde_json::json!({
            "id": "chat_response", "model": "chat-model", "choices": [{
                "index": 0, "finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": "Inspecting.",
                    "reasoning_content": "Read the files first.",
                    "tool_calls": [{"id": "call_lookup", "type": "function", "function": {
                        "name": "lookup", "arguments": "{\"key\":\"value\"}"
                    }}]
                }
            }]
        }))
        .expect("decode Chat tool response");
    let history = crate::codec::openai::compatible::stream::client_history_output_item(&response);
    let mut dual = history.clone();
    let MessageContent::Blocks(blocks) = &mut dual.content else {
        panic!("readable Chat history blocks")
    };
    blocks.push(ContentBlock::ToolUse {
        id: "call_lookup".into(),
        name: "lookup".into(),
        input: serde_json::json!({"key": "value"}),
        cache_control: None,
    });
    let mut blocks_only = dual.clone();
    blocks_only.tool_calls = None;
    let continuation = crate::codec::open_responses::decoder::ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "model", "input": [{
                "type": "function_call_output", "call_id": "call_lookup", "output": "found"
            }]
        }))
        .expect("decode unadorned Responses function result");
    for history in [history, dual, blocks_only] {
        let mut request = continuation.clone();
        request.items.insert(0, history);
        let (body, _) = GoogleEncoder
            .encode_request(&request)
            .expect("encode continuation");
        assert_eq!(
            body["contents"][0]["parts"],
            serde_json::json!([
                {"text": "Read the files first.", "thought": true},
                {"text": "Inspecting."},
                {"functionCall": {"id": "call_lookup", "name": "lookup", "args": {"key": "value"}}}
            ])
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"],
            serde_json::json!({
                "id": "call_lookup", "name": "lookup", "response": {"result": "found"}
            })
        );
    }
}

#[test]
fn rejects_unrepresentable_named_tool_choice() {
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
    request.tool_choice = Some(ToolChoice::Named {
        name: "lookup".into(),
    });

    let error = GoogleEncoder
        .encode_request(&request)
        .expect_err("Gemini encoder cannot silently drop named tool choice");
    assert!(error.to_string().contains("tool_choice"));
}
#[test]
fn encodes_json_schema_response_format_in_generation_config() {
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
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        strict: Some(true),
        schema: serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"answer": {"type": "string"}}
        }),
    });

    let (body, _) = GoogleEncoder
        .encode_request(&request)
        .expect("Gemini supports structured JSON output");

    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        body["generationConfig"]["responseSchema"]["properties"]["answer"]["type"],
        "string"
    );
    assert!(
        body["generationConfig"]["responseSchema"]
            .get("$schema")
            .is_none()
    );
}

#[test]
fn target_controls_replace_raw_gemini_thinking_config() {
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
        "__google_generation_config".into(),
        serde_json::json!({"thinkingConfig": {"thinkingBudget": 12}}),
    );

    let (body, _) = GoogleEncoder
        .encode_request(&request)
        .expect("Gemini Thinking Level");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        serde_json::json!({"thinkingLevel": "HIGH"})
    );
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
fn signed_thinking_keeps_thought_part_with_signature() {
    let request = assistant_blocks_request(vec![ContentBlock::Thinking {
        thinking: "visible reasoning".into(),
        signature: Some("opaque-sig".into()),
    }]);

    let (body, _) = GoogleEncoder.encode_request(&request).expect("encode");

    assert_eq!(
        body["contents"][1]["parts"][0],
        serde_json::json!({
            "text": "visible reasoning",
            "thought": true,
            "thoughtSignature": "opaque-sig",
        })
    );
}

#[test]
fn unsigned_thinking_stays_a_thought_part_not_plain_text() {
    let request = assistant_blocks_request(vec![ContentBlock::Thinking {
        thinking: "unsigned reasoning".into(),
        signature: None,
    }]);

    let (body, _) = GoogleEncoder.encode_request(&request).expect("encode");

    // Gemini 接受无 thoughtSignature 的 thought part；明文推理不能降级为正文，
    // 否则模型会把推理当成已说出口的话。
    assert_eq!(
        body["contents"][1]["parts"][0],
        serde_json::json!({"text": "unsigned reasoning", "thought": true})
    );
}

#[test]
fn thinking_replay_responses_ciphertext_is_not_gemini_signature() {
    let request = assistant_blocks_request(vec![ContentBlock::Reasoning {
        summary: vec!["summary".into()],
        content: vec!["detail".into()],
        encrypted_content: Some("ciphertext".into()),
    }]);

    let (body, _) = GoogleEncoder.encode_request(&request).expect("encode");

    assert_eq!(
        body["contents"][1]["parts"],
        serde_json::json!([
            {"text": "summary", "thought": true},
            {"text": "detail", "thought": true}
        ])
    );
}

#[test]
fn redacted_thinking_is_dropped_without_leaking_data() {
    let request = assistant_blocks_request(vec![
        ContentBlock::RedactedThinking {
            data: "redacted-payload".into(),
        },
        ContentBlock::Text {
            text: "answer".into(),
            cache_control: None,
        },
    ]);

    let (body, _) = GoogleEncoder.encode_request(&request).expect("encode");

    let parts = body["contents"][1]["parts"].as_array().unwrap();
    assert_eq!(parts.as_slice(), [serde_json::json!({"text": "answer"})]);
    assert!(!body.to_string().contains("redacted-payload"));
}

#[test]
fn assistant_item_with_only_unmappable_protected_payload_is_skipped() {
    let request = assistant_blocks_request(vec![ContentBlock::RedactedThinking {
        data: "redacted-payload".into(),
    }]);

    let (body, _) = GoogleEncoder.encode_request(&request).expect("encode");

    // 整条 assistant 内容都无法承载时不能发出空 model content。
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    assert!(!body.to_string().contains("redacted-payload"));
}

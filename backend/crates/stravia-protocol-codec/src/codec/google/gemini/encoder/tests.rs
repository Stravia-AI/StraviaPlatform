use super::*;

#[test]
fn gemini_summary_intent_survives_independent_intensity_mapping() {
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
    for (config, control, include) in [
        (serde_json::json!({}), None, true),
        (
            serde_json::json!({"includeThoughts": false}),
            Some(TargetThinkingControl::Effort {
                value: "high".into(),
            }),
            false,
        ),
        (
            serde_json::json!({"includeThoughts": true}),
            Some(TargetThinkingControl::Budget { value: 2048 }),
            true,
        ),
        (
            serde_json::json!({"thinkingBudget": 0}),
            Some(TargetThinkingControl::Effort {
                value: "high".into(),
            }),
            false,
        ),
        (
            serde_json::json!({}),
            Some(TargetThinkingControl::Disabled),
            false,
        ),
    ] {
        let mut request = crate::codec::google::gemini::decoder::GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
                "generationConfig": {"thinkingConfig": config}
            }))
            .unwrap();
        request.reasoning.target_control = control.clone();
        let (body, _) = GoogleEncoder.encode_request(&request).unwrap();
        let thinking = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking["includeThoughts"], include);
        match control {
            Some(TargetThinkingControl::Effort { .. }) => {
                assert_eq!(thinking["thinkingLevel"], "HIGH")
            }
            Some(TargetThinkingControl::Budget { .. }) => {
                assert_eq!(thinking["thinkingBudget"], 2048)
            }
            Some(TargetThinkingControl::Disabled) => assert_eq!(thinking["thinkingBudget"], 0),
            None => {
                assert_eq!(request.reasoning.level, None::<ThinkingLevel>);
                assert!(thinking.get("thinkingBudget").is_none());
                assert!(thinking.get("thinkingLevel").is_none());
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn native_function_responses_round_trip_business_json_and_parallel_call_ids() {
    for responses in [
        serde_json::json!([{
            "functionResponse": {
                "id": "call_sum", "name": "sum",
                "response": {"sum": 42, "receipt": "synthetic-receipt-sum"}
            }
        }]),
        serde_json::json!([
            {"functionResponse": {
                "id": "call_lookup", "name": "lookup",
                "response": {"found": true, "receipt": "synthetic-receipt-lookup"}
            }},
            {"functionResponse": {
                "id": "call_sum", "name": "sum",
                "response": {"sum": 42, "receipt": "synthetic-receipt-sum"}
            }}
        ]),
    ] {
        let request = crate::codec::google::gemini::decoder::GoogleDecoder
            .decode_request(serde_json::json!({
                "model": "gemini-model",
                "contents": [
                    {"role": "model", "parts": [
                        {"functionCall": {
                            "id": "call_sum", "name": "sum", "args": {"a": 17, "b": 25}
                        }},
                        {"functionCall": {
                            "id": "call_lookup", "name": "lookup", "args": {"key": "value"}
                        }}
                    ]},
                    {"role": "user", "parts": responses.clone()}
                ]
            }))
            .expect("decode native Gemini tool results");

        let (body, _) = GoogleEncoder
            .encode_request(&request)
            .expect("encode native Gemini tool results");

        assert_eq!(body["contents"][1]["role"], "user");
        assert_eq!(body["contents"][1]["parts"], responses);
    }
}

#[test]
fn text_and_multimodal_tool_outputs_keep_existing_response_envelopes() {
    let call_names = HashMap::from([("call_sum", "sum")]);
    for (content, expected) in [
        (
            MessageContent::Text("{\"sum\":42}".into()),
            serde_json::json!({
                "id": "call_sum", "name": "sum", "response": {"result": "{\"sum\":42}"}
            }),
        ),
        (
            MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "image result".into(),
                    cache_control: None,
                },
                ContentBlock::Image {
                    source: MediaSource::Base64 {
                        media_type: "image/png".into(),
                        data: "synthetic-image".into(),
                    },
                    detail: None,
                    cache_control: None,
                },
            ]),
            serde_json::json!({
                "id": "call_sum", "name": "sum", "response": {"result": "image result"},
                "parts": [{"inlineData": {"mimeType": "image/png", "data": "synthetic-image"}}]
            }),
        ),
    ] {
        let message = AiItem {
            role: Role::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some("call_sum".into()),
            meta: None,
        };
        let encoded = encode_content(&message, &call_names).expect("encode tool result");
        assert_eq!(
            encoded,
            serde_json::json!({"role": "user", "parts": [{"functionResponse": expected}]})
        );
    }
}

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
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert!(
        body["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none()
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

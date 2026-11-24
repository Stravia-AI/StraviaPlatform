use super::*;

#[test]
fn nested_tool_results_project_supported_parts_without_reinterpreting_business_json() {
    use stravia_runtime_contract::protocol::ir::{ToolCall, ToolResultContentKind};

    let business = serde_json::json!([
        {"type": "text", "text": "business", "extra": [1, 2]},
        {"type": "image", "source": {"type": "url", "url": "https://example.invalid/business"}},
        {"type": "unknown", "raw": {"business": true}}
    ]);
    for (content, kind, expected) in [
        (
            serde_json::json!([
                {"type": "text", "text": "first"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "image-data"}},
                {"type": "audio", "source": {"type": "base64", "media_type": "audio/wav", "data": "audio-data"}},
                {"type": "file", "source": {"type": "file_id", "file_id": "file-123"}, "media_type": "application/pdf"},
                {"type": "video", "source": {"type": "url", "url": "https://example.invalid/video"}, "media_type": "video/mp4"},
                {"type": "unknown", "raw": {"private": "discard"}},
                {"type": "unrecognized", "payload": "discard"},
                {"type": "text", "text": "second"}
            ]),
            Some(ToolResultContentKind::ContentBlocks),
            Some(serde_json::json!({
                "id": "call_lookup", "name": "lookup", "response": {"result": "firstsecond"},
                "parts": [
                    {"inlineData": {"mimeType": "image/png", "data": "image-data"}},
                    {"inlineData": {"mimeType": "audio/wav", "data": "audio-data"}},
                    {"fileData": {"fileUri": "file-123", "mimeType": "application/pdf"}},
                    {"fileData": {"fileUri": "https://example.invalid/video", "mimeType": "video/mp4"}}
                ]
            })),
        ),
        (
            serde_json::json!([
                {"type": "unknown", "raw": {"discard": true}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "image-only"}}
            ]),
            Some(ToolResultContentKind::ContentBlocks),
            Some(serde_json::json!({
                "id": "call_lookup", "name": "lookup", "response": {},
                "parts": [{"inlineData": {"mimeType": "image/png", "data": "image-only"}}]
            })),
        ),
        (
            business.clone(),
            Some(ToolResultContentKind::Json),
            Some(serde_json::json!({
                "id": "call_lookup", "name": "lookup", "response": {"result": business}
            })),
        ),
        (
            serde_json::json!([
                {"type": "unknown", "raw": {"private": "discard"}},
                {"type": "thinking", "thinking": "discard"}
            ]),
            Some(ToolResultContentKind::ContentBlocks),
            None,
        ),
        (
            serde_json::json!([]),
            Some(ToolResultContentKind::ContentBlocks),
            None,
        ),
        (
            serde_json::json!("literal"),
            Some(ToolResultContentKind::ContentBlocks),
            Some(serde_json::json!({
                "id": "call_lookup", "name": "lookup", "response": {"result": "literal"}
            })),
        ),
    ] {
        let request = AiRequest::new(
            "model",
            vec![
                AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Text("".to_owned().into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_lookup".into(),
                        name: "lookup".into(),
                        arguments: "{}".into(),
                    }]),
                    tool_call_id: None,
                    meta: None,
                },
                AiItem {
                    role: Role::Tool,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "call_lookup".into(),
                        content,
                        content_kind: kind,
                        is_error: None,
                        cache_control: None,
                    }]),
                    tool_calls: None,
                    tool_call_id: Some("call_lookup".into()),
                    meta: None,
                },
            ],
        );
        let (body, _) = GoogleEncoder
            .encode_request(request)
            .expect("encode tool result");
        let results: Vec<_> = body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap())
            .filter_map(|part| part.get("functionResponse"))
            .collect();
        if let Some(expected) = expected {
            assert_eq!(results, vec![&expected]);
        } else {
            assert!(
                results.is_empty(),
                "unsupported blocks must not become a JSON response"
            );
        }
    }
}

#[test]
fn native_function_response_keeps_block_shaped_business_array() {
    let response = serde_json::json!({"result": [
        {"type": "text", "text": "business", "extra": [1, 2]},
        {"type": "image", "source": {"type": "url", "url": "https://example.invalid/business"}}
    ]});
    let request = crate::codec::google::gemini::decoder::GoogleDecoder
        .decode_request(serde_json::json!({"model": "model", "contents": [
            {"role": "model", "parts": [{"functionCall": {"id": "call_lookup", "name": "lookup", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "call_lookup", "name": "lookup", "response": response.clone()}}]}
        ]})).expect("decode native response");
    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("encode native response");
    assert_eq!(
        body["contents"][1]["parts"][0]["functionResponse"],
        serde_json::json!({
            "id": "call_lookup", "name": "lookup", "response": response
        })
    );
}

#[test]
fn request_encoder_drops_invalid_tool_arguments_in_all_call_carriers() {
    use stravia_runtime_contract::protocol::ir::request::ToolCall;
    for arguments in ["[]", "null", "42", "\"scalar\"", "not JSON", ""] {
        for as_blocks in [false, true] {
            let text = ContentBlock::Text {
                text: "kept".to_owned().into(),
                cache_control: None,
            };
            let content = if as_blocks {
                MessageContent::Blocks(vec![text])
            } else {
                MessageContent::Text("kept".to_owned().into())
            };
            let request = AiRequest::new(
                "model",
                vec![AiItem {
                    role: Role::Assistant,
                    content,
                    tool_calls: Some(vec![
                        ToolCall {
                            id: "bad".into(),
                            name: "lookup".into(),
                            arguments: arguments.into(),
                        },
                        ToolCall {
                            id: "good".into(),
                            name: "lookup".into(),
                            arguments: "{}".into(),
                        },
                    ]),
                    tool_call_id: None,
                    meta: None,
                }],
            );
            let (body, _) = GoogleEncoder
                .encode_request(request)
                .expect("unsupported arguments are omitted");
            assert_eq!(
                body["contents"][0]["parts"],
                serde_json::json!([
                    {"text": "kept"}, {"functionCall": {"id": "good", "name": "lookup", "args": {}}}
                ])
            );
        }
    }
    for input in [
        Value::Null,
        serde_json::json!([]),
        serde_json::json!(42),
        serde_json::json!("scalar"),
    ] {
        let block = ContentBlock::ToolUse {
            id: "bad".into(),
            name: "lookup".into(),
            input,
            cache_control: None,
        };
        assert!(encode_content_block_for_gemini(block, &HashMap::new()).is_none());
    }
}

#[test]
fn owned_large_text_preserves_wire_content_and_roles() {
    for role in [Role::User, Role::Assistant] {
        for as_block in [false, true] {
            let text = "varied text\n".repeat(480_000);
            let text = Arc::new(text);
            let content = if as_block {
                MessageContent::Blocks(vec![ContentBlock::Text {
                    text,
                    cache_control: None,
                }])
            } else {
                MessageContent::Text(text)
            };
            let request = AiRequest::new(
                "model",
                vec![AiItem {
                    role,
                    content,
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                }],
            );
            let (body, _) = GoogleEncoder
                .encode_request(request)
                .expect("encode owned text");
            let encoded = body["contents"][0]["parts"][0]["text"].as_str().unwrap();
            assert_eq!(encoded, "varied text\n".repeat(480_000));
            assert_eq!(
                body["contents"][0]["role"],
                if role == Role::Assistant {
                    "model"
                } else {
                    "user"
                }
            );
        }
    }
}

#[test]
fn shared_text_remains_available_after_owned_request_encoding() {
    let text = Arc::new("shared history\n".to_owned());
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text(Arc::clone(&text)),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("encode shared text");
    assert_eq!(body["contents"][0]["parts"][0]["text"], text.as_str());
    assert_eq!(text.as_str(), "shared history\n");
}

#[test]
fn system_text_preserves_concatenation_newlines_and_raw_priority() {
    let mut request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::System,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "first\n".to_owned().into(),
                        cache_control: None,
                    },
                    ContentBlock::Text {
                        text: "second".to_owned().into(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Developer,
                content: MessageContent::Text("developer\n".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );
    request.instructions = Some("instructions\n".into());
    let (body, _) = GoogleEncoder
        .encode_request(request.clone())
        .expect("encode system text");
    assert_eq!(
        body["systemInstruction"],
        serde_json::json!({"parts": [
            {"text": "instructions\n"}, {"text": "first\nsecond"}, {"text": "developer\n"}
        ]})
    );
    let raw = object([
        ("role", Value::String("system".into())),
        (
            "parts",
            Value::Array(vec![object([
                ("text", Value::String("raw system\n".into())),
                ("vendorFlag", Value::Bool(true)),
            ])]),
        ),
    ]);
    request
        .meta
        .vendor
        .ingress
        .insert("__google_raw_system_instruction".into(), raw.clone());
    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("encode raw system");
    assert_eq!(body["systemInstruction"], raw);
}

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
            Some(TargetThinkingControl::Effort {
                value: "low".into(),
            }),
            true,
        ),
        (
            serde_json::json!({"thinkingLevel": "NONE"}),
            Some(TargetThinkingControl::Effort {
                value: "high".into(),
            }),
            false,
        ),
        (
            serde_json::json!({}),
            Some(TargetThinkingControl::Effort {
                value: "none".into(),
            }),
            false,
        ),
    ] {
        let mut request = crate::codec::google::gemini::decoder::GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
                "generationConfig": {"thinkingConfig": config}
            }))
            .unwrap();
        request.model = "gemini-3-flash".into();
        request.reasoning.target_control = control.clone();
        let reasoning_level = request.reasoning.level;
        let (body, _) = GoogleEncoder.encode_request(request).unwrap();
        let thinking = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking["includeThoughts"], include);
        assert!(
            !(thinking.get("thinkingLevel").is_some() && thinking.get("thinkingBudget").is_some()),
            "Gemini must never emit thinkingLevel and thinkingBudget together: {thinking}"
        );
        match control {
            Some(TargetThinkingControl::Effort { ref value }) if value == "none" => {
                assert_eq!(thinking["thinkingBudget"], 0);
            }
            Some(TargetThinkingControl::Effort { ref value }) => {
                assert_eq!(thinking["thinkingLevel"], value.to_ascii_uppercase());
            }
            None => {
                assert_eq!(reasoning_level, None::<ThinkingLevel>);
                assert!(thinking.get("thinkingBudget").is_none());
                assert!(thinking.get("thinkingLevel").is_none());
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn gemini_egress_applies_generation_specific_level_and_budget_rules() {
    use stravia_runtime_contract::thinking::TargetThinkingControl;

    let cases = [
        ("gemini-3-flash", "high", Some("HIGH"), None, true),
        ("gemini-3-flash", "none", None, Some(0), false),
        ("gemini-3-pro", "high", Some("HIGH"), None, true),
        ("gemini-3-pro", "none", Some("LOW"), None, false),
        ("gemini-2.5-pro", "high", None, Some(16384), true),
        ("gemini-2.5-pro", "max", None, Some(32768), true),
        ("gemini-2.5-pro", "none", None, Some(128), false),
        ("gemini-2.5-flash", "high", None, Some(16384), true),
        ("gemini-2.5-flash", "max", None, Some(24576), true),
        ("gemini-2.5-flash", "none", None, Some(0), false),
    ];

    for (model, effort, expected_level, expected_budget, expected_thoughts) in cases {
        let mut request = AiRequest::new(
            model,
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.reasoning.target_control = Some(TargetThinkingControl::Effort {
            value: effort.into(),
        });
        let (body, _) = GoogleEncoder.encode_request(request).unwrap();
        let thinking = &body["generationConfig"]["thinkingConfig"];
        assert!(
            !(thinking.get("thinkingLevel").is_some() && thinking.get("thinkingBudget").is_some()),
            "{model}/{effort} emitted both thinkingLevel and thinkingBudget: {thinking}"
        );
        assert_eq!(
            thinking.get("thinkingLevel").and_then(Value::as_str),
            expected_level,
            "{model}/{effort}"
        );
        assert_eq!(
            thinking.get("thinkingBudget").and_then(Value::as_u64),
            expected_budget,
            "{model}/{effort}"
        );
        assert_eq!(
            thinking["includeThoughts"], expected_thoughts,
            "{model}/{effort}"
        );
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
            .encode_request(request)
            .expect("encode native Gemini tool results");

        assert_eq!(body["contents"][1]["role"], "user");
        assert_eq!(body["contents"][1]["parts"], responses);
    }
}

#[test]
fn text_and_multimodal_tool_outputs_keep_existing_response_envelopes() {
    let call_names = HashMap::from([("call_sum".to_owned(), "sum".to_owned())]);
    for (content, expected) in [
        (
            MessageContent::Text("{\"sum\":42}".to_owned().into()),
            serde_json::json!({
                "id": "call_sum", "name": "sum", "response": {"result": "{\"sum\":42}"}
            }),
        ),
        (
            MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "image result".to_owned().into(),
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
        let encoded = encode_content(message, &call_names).expect("encode tool result");
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
            .encode_request(request)
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
fn encodes_named_tool_choice() {
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
    request.tool_choice = Some(ToolChoice::Named {
        name: "lookup".into(),
    });

    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("Gemini supports named tool choice");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "hello");
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"],
        serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["lookup"]})
    );
}
#[test]
fn encodes_json_schema_response_format_in_generation_config() {
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
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        description: None,
        strict: Some(true),
        schema: serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"answer": {"type": "string"}}
        }),
    });

    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("Gemini supports structured JSON output");

    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"]["properties"]["answer"]["type"],
        "string"
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"]["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
}

#[test]
fn preserves_json_schema_constraints_and_local_definitions() {
    let mut request = AiRequest::new("gemini-2.5-flash", vec![]);
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        description: None,
        strict: Some(true),
        schema: serde_json::json!({
            "type": "object", "additionalProperties": false,
            "definitions": {"entry": {"type": "string", "minLength": 3}},
            "$defs": {"entry": {"type": "number"}},
            "properties": {
                "answer": {"$ref": "#/definitions/entry"},
                "additionalProperties": {"type": "string", "pattern": "x"},
                "numeric": {"$ref": "#/$defs/entry"}
            },
            "required": ["answer"], "unknownConstraint": true
        }),
    });
    request.meta.vendor.ingress.insert(
        "__google_generation_config".into(),
        serde_json::json!({"responseSchema": {"type": "OBJECT"}}),
    );
    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("structured schema");
    let schema = &body["generationConfig"]["responseJsonSchema"];
    assert!(body["generationConfig"].get("responseSchema").is_none());
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["answer"]["$ref"],
        "#/definitions/entry"
    );
    assert_eq!(schema["definitions"]["entry"]["type"], "string");
    assert_eq!(schema["$defs"]["entry"]["type"], "number");
    assert_eq!(
        schema["properties"]["additionalProperties"]["type"],
        "string"
    );
    assert_eq!(schema["properties"]["additionalProperties"]["pattern"], "x");
    assert_eq!(schema["definitions"]["entry"]["minLength"], 3);
    assert_eq!(schema["unknownConstraint"], true);
}

#[test]
fn preserves_recursive_refs_without_inlining() {
    let mut request = AiRequest::new("gemini-2.5-flash", vec![]);
    let schema = serde_json::json!({
        "type": "object", "properties": {"next": {"$ref": "#/$defs/node"}},
        "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}}
    });
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "node".into(),
        description: None,
        strict: None,
        schema: schema.clone(),
    });
    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("recursive schema");
    assert_eq!(body["generationConfig"]["responseJsonSchema"], schema);
}

#[test]
fn encodes_all_canonical_function_calling_modes() {
    for (choice, config) in [
        (ToolChoice::Auto, serde_json::json!({"mode": "AUTO"})),
        (ToolChoice::None, serde_json::json!({"mode": "NONE"})),
        (ToolChoice::Required, serde_json::json!({"mode": "ANY"})),
    ] {
        let mut request = AiRequest::new("gemini-2.5-flash", vec![]);
        request.tool_choice = Some(choice);
        let (body, _) = GoogleEncoder.encode_request(request).expect("tool choice");
        assert_eq!(body["toolConfig"]["functionCallingConfig"], config);
    }
}

#[test]
fn target_controls_replace_raw_gemini_thinking_config() {
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
        "__google_generation_config".into(),
        serde_json::json!({"thinkingConfig": {"thinkingBudget": 12}}),
    );

    let (body, _) = GoogleEncoder
        .encode_request(request)
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
fn signed_thinking_keeps_thought_part_with_signature() {
    let request = assistant_blocks_request(vec![ContentBlock::Thinking {
        thinking: "visible reasoning".into(),
        signature: Some("opaque-sig".into()),
    }]);

    let (body, _) = GoogleEncoder.encode_request(request).expect("encode");

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

    let (body, _) = GoogleEncoder.encode_request(request).expect("encode");

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

    let (body, _) = GoogleEncoder.encode_request(request).expect("encode");

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
            text: "answer".to_owned().into(),
            cache_control: None,
        },
    ]);

    let (body, _) = GoogleEncoder.encode_request(request).expect("encode");

    let parts = body["contents"][1]["parts"].as_array().unwrap();
    assert_eq!(parts.as_slice(), [serde_json::json!({"text": "answer"})]);
    assert!(!body.to_string().contains("redacted-payload"));
}

#[test]
fn assistant_item_with_only_unmappable_protected_payload_is_skipped() {
    let request = assistant_blocks_request(vec![ContentBlock::RedactedThinking {
        data: "redacted-payload".into(),
    }]);

    let (body, _) = GoogleEncoder.encode_request(request).expect("encode");

    // 整条 assistant 内容都无法承载时不能发出空 model content。
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    assert!(!body.to_string().contains("redacted-payload"));
}
#[test]
fn canonical_parameters_choice_schema_and_native_tools_are_projected() {
    use stravia_runtime_contract::protocol::ir::{
        ImageGenerationTool, NativeTool, ToolSpec, WebSearchTool,
    };
    let canonical_schema = serde_json::json!({
        "type": "object",
        "properties": {"answer": {"enum": [null, false, 0, ""]}},
        "additionalProperties": false
    });
    let mut request = AiRequest::new("gemini-2.5-flash", vec![]);
    request.parameters.top_k = Some(0);
    request.parameters.candidate_count = Some(0);
    request.parameters.logprobs = Some(0);
    request.parameters.modalities = Some(vec![Modality::Text, Modality::Audio]);
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        description: None,
        strict: None,
        schema: canonical_schema.clone(),
    });
    request.tools = Some(vec![ToolSpec {
        name: "lookup".into(),
        description: Some(String::new()),
        parameters: canonical_schema.clone(),
        strict: None,
        defer_loading: None,
    }]);
    request.tool_choice = Some(ToolChoice::None);
    request.meta.vendor.ingress.insert("__google_raw_tools".into(), serde_json::json!([
        {"functionDeclarations": [{"name": "stale", "parametersJsonSchema": {"type": "string"}}]}
    ]));
    request.parameters.native_tools = vec![
        NativeTool::WebSearch(WebSearchTool {
            blocked_domains: vec!["blocked.example".into()],
            ..Default::default()
        }),
        NativeTool::ImageGeneration(ImageGenerationTool::default()),
    ];
    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("canonical controls win");
    assert_eq!(body["generationConfig"]["topK"], 0);
    assert_eq!(body["generationConfig"]["candidateCount"], 0);
    assert_eq!(body["generationConfig"]["responseLogprobs"], true);
    assert_eq!(body["generationConfig"]["logprobs"], 0);
    assert_eq!(
        body["generationConfig"]["responseModalities"],
        serde_json::json!(["TEXT", "AUDIO"])
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"],
        canonical_schema
    );
    assert!(body["generationConfig"].get("unknownOption").is_none());
    assert!(body.get("cachedContent").is_none());
    assert_eq!(
        body["tools"],
        serde_json::json!([
            {"functionDeclarations": [{"name": "lookup", "description": "", "parametersJsonSchema": canonical_schema}]},
            {"googleSearch": {"excludeDomains": ["blocked.example"]}}
        ])
    );
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"],
        serde_json::json!({
            "mode": "NONE"
        })
    );
}

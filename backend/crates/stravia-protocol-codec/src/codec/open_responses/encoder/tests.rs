use super::*;

use crate::codec::anthropic::messages::decoder::AnthropicDecoder;
use crate::codec::open_responses::decoder::ResponsesDecoder;
use crate::codec::openai::compatible::decoder::OpenAIDecoder;
use stravia_runtime_contract::protocol::ir::AiItem;

#[test]
fn target_effort_maps_to_responses_shape() {
    let mut request = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "messages": [{"role": "user", "content": "hello"}],
            "reasoning_effort": "max"
        }))
        .unwrap();
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "xhigh".into(),
        },
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn chat_reasoning_content_degrades_to_output_text_before_its_tool_call() {
    let request = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "messages": [
                {
                    "role": "assistant",
                    "content": "",
                    "reasoning_content": "inspect repository",
                    "tool_calls": [{
                        "id": "call_glob",
                        "type": "function",
                        "function": {
                            "name": "glob",
                            "arguments": "{}"
                        }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_glob",
                    "content": "result"
                }
            ]
        }))
        .expect("decode Chat reasoning tool history");

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode Responses reasoning tool history");

    // Chat 的 `reasoning_content` 没有签名，上游不会
    // 接受裸 reasoning item，明文降级为 assistant message 的 output_text。
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "assistant");
    assert_eq!(
        body["input"][0]["content"],
        serde_json::json!([{"type": "output_text", "text": "inspect repository"}])
    );
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][1]["call_id"], "call_glob");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["call_id"], "call_glob");
}

#[test]
fn thinking_replay_signature_is_not_responses_ciphertext() {
    let request = AiRequest::new(
        "gpt",
        vec![AiItem::thinking(
            "visible thinking",
            Some("anthropic-signature".into()),
        )],
    );
    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();
    assert_eq!(
        body["input"],
        serde_json::json!([{
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "visible thinking"}]
        }])
    );
}

#[test]
fn native_responses_preserves_omitted_reasoning_summary() {
    let mut request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "input": "hello",
            "reasoning": {"effort": "medium"}
        }))
        .unwrap();
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "medium".into(),
        },
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert_eq!(body["reasoning"]["effort"], "medium");
    assert!(body["reasoning"].get("summary").is_none());
}

#[test]
fn anthropic_thinking_defaults_to_responses_auto_summary() {
    let mut request = AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 512}
        }))
        .unwrap();
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "medium".into(),
        },
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert_eq!(body["reasoning"]["summary"], "auto");
}

#[test]
fn anthropic_adaptive_thinking_maps_display_to_responses_summary() {
    for (display, expected_summary) in [("summarized", Some("auto")), ("omitted", None)] {
        let mut request = AnthropicDecoder
            .decode_request(serde_json::json!({
                "model": "gpt",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hello"}],
                "thinking": {"type": "adaptive", "display": display},
                "output_config": {"effort": "medium"}
            }))
            .unwrap();
        request.reasoning.target_control = Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: "medium".into(),
            },
        );

        let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

        assert_eq!(
            body["reasoning"].get("summary").and_then(Value::as_str),
            expected_summary
        );
    }
}

#[test]
fn anthropic_without_thinking_omits_responses_reasoning() {
    let request = AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert!(body.get("reasoning").is_none());
}

#[test]
fn writes_max_target_effort_without_a_local_allow_list() {
    let mut request = AiRequest::new("gpt", vec![AiItem::output_text("hello")]);
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "max".into(),
        },
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert_eq!(body["reasoning"]["effort"], "max");
}

#[test]
fn preserves_request_instructions_and_developer_role_with_dated_defaults() {
    let mut request = AiRequest::new(
        "logical-model",
        vec![
            AiItem {
                role: Role::Developer,
                content: MessageContent::Text("Use repository conventions.".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::User,
                content: MessageContent::Text("Implement it.".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );
    request.instructions = Some("Follow the accepted design.".into());

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert_eq!(body["instructions"], "Follow the accepted design.");
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["store"], true);
    assert_eq!(body["stream"], false);
}

#[test]
fn forwards_provider_persistence_only_when_explicitly_requested() {
    let mut request = AiRequest::new(
        "logical-model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("Persist upstream.".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                store: Some(true),
                ..Default::default()
            },
        ),
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();
    assert_eq!(body["store"], true);
}

#[test]
fn keeps_dated_metadata_and_safety_identifier_out_of_provider_requests() {
    let mut request = AiRequest::new(
        "logical-model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                metadata: Some(serde_json::json!({"tenant": "acme"})),
                safety_identifier: Some("safe-user-1".into()),
                ..Default::default()
            },
        ),
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert!(body.get("metadata").is_none());
    assert!(body.get("safety_identifier").is_none());
}

#[test]
fn function_call_output_content_array_round_trips_without_stringification() {
    let output = serde_json::json!([
        {"type": "input_text", "text": "tool text"},
        {"type": "input_image", "image_url": "https://example.test/image.png"}
    ]);
    let request = crate::codec::open_responses::decoder::ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": output
            }]
        }))
        .expect("decode function output");
    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode function output");

    assert_eq!(body["input"][0]["output"], output);
    assert!(body["input"][0]["output"].is_array());
}

#[test]
fn encodes_responses_supported_media_without_text_coercion() {
    let request = AiRequest::new(
        "gpt",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Image {
                    source: MediaSource::Base64 {
                        media_type: "image/png".into(),
                        data: "aGk=".into(),
                    },
                    detail: None,
                    cache_control: None,
                },
                ContentBlock::File {
                    source: MediaSource::Url("https://example.test/doc.pdf".into()),
                    media_type: Some("application/pdf".into()),
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();
    assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
    assert_eq!(
        body["input"][0]["content"][0]["image_url"],
        "data:image/png;base64,aGk="
    );
    assert_eq!(body["input"][0]["content"][1]["type"], "input_file");
    assert_eq!(
        body["input"][0]["content"][1]["file_url"],
        "https://example.test/doc.pdf"
    );
}

#[test]
fn continuation_without_new_input_omits_input() {
    let mut request = AiRequest::new("logical-model", Vec::new());
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                previous_response_id: Some("resp_parent".into()),
                ..Default::default()
            },
        ),
    );

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode continuation");

    assert_eq!(body["previous_response_id"], "resp_parent");
    assert!(body.get("input").is_none());
}

#[test]
fn rejects_responses_unsupported_media() {
    let request = AiRequest::new(
        "gpt",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Audio {
                source: MediaSource::Base64 {
                    media_type: "audio/wav".into(),
                    data: "aGk=".into(),
                },
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let error = ResponsesEncoder.encode_request(&request).unwrap_err();
    assert!(error.to_string().contains("audio"));
}

#[test]
fn encodes_canonical_json_schema_for_dated_targets() {
    let mut input = AiItem::output_text("return structured data");
    input.role = Role::User;
    let mut request = AiRequest::new("gpt", vec![input]);
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        }),
        strict: Some(true),
    });

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode canonical response format");
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["name"], "answer");
    assert_eq!(body["text"]["format"]["strict"], true);
    assert_eq!(
        body["text"]["format"]["schema"]["required"],
        serde_json::json!(["answer"])
    );
}

#[test]
fn rejects_canonical_json_object_for_dated_targets() {
    let mut input = AiItem::output_text("return JSON");
    input.role = Role::User;
    let mut request = AiRequest::new("gpt", vec![input]);
    request.response_format = Some(ResponseFormat::JsonObject);

    let error = ResponsesEncoder
        .encode_request(&request)
        .expect_err("json_object has no dated representation");
    assert!(error.to_string().contains("json_object"));
}

#[test]
fn encodes_non_media_tool_payload_as_json_text() {
    for payload in [
        serde_json::json!({"temperature": 21}),
        serde_json::json!([
            {"type": "tool_result", "content": {"temperature": 21}}
        ]),
    ] {
        let request = AiRequest::new(
            "gpt",
            vec![AiItem {
                role: Role::Tool,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: payload.clone(),
                    content_kind: Some(
                        stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                    ),
                    is_error: None,
                    cache_control: None,
                }]),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                meta: None,
            }],
        );

        let (body, _) = ResponsesEncoder
            .encode_request(&request)
            .expect("encode structured tool result");
        let output = body["input"][0]["output"]
            .as_str()
            .expect("non-media tool output must be JSON text, not a native content array");
        assert_eq!(serde_json::from_str::<Value>(output).unwrap(), payload);
    }
}

#[test]

fn tool_output_parts_follow_dated_request_and_response_media_shapes() {
    assert!(request_tool_output_part(&serde_json::json!({
        "type": "input_image",
        "image_url": "https://example.test/image.png",
        "detail": "high"
    })));
    assert!(!request_tool_output_part(&serde_json::json!({
        "type": "input_image",
        "file_id": "file_1",
        "detail": "high"
    })));
    assert!(response_tool_output_part(&serde_json::json!({
        "type": "input_image",
        "image_url": null,
        "detail": "auto"
    })));
    assert!(response_tool_output_part(
        &serde_json::json!({"type": "input_file"})
    ));
    for invalid in [
        serde_json::json!({
            "type": "input_image",
            "image_url": "https://example.test/image.png"
        }),
        serde_json::json!({
            "type": "input_image",
            "image_url": "https://example.test/image.png",
            "detail": "future"
        }),
        serde_json::json!({
            "type": "input_file",
            "file_data": "data:text/plain;base64,aGk="
        }),
    ] {
        assert!(!response_tool_output_part(&invalid));
    }
}

#[test]
fn unsigned_thinking_degrades_to_output_text_message() {
    let request = AiRequest::new("gpt", vec![AiItem::thinking("why", None)]);

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode unsigned thinking");

    // 无签名：上游不接受裸 reasoning item，明文进 output_text。
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "assistant");
    assert_eq!(
        body["input"][0]["content"],
        serde_json::json!([{"type": "output_text", "text": "why"}])
    );
}

#[test]
fn reasoning_without_id_or_encrypted_content_degrades_to_output_text() {
    let request = AiRequest::new(
        "gpt",
        vec![AiItem::reasoning(
            vec!["summary".into()],
            vec!["detail".into()],
            None,
        )],
    );

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode unprotected reasoning");

    // summary 各段在前、content 各段在后，每段一个 output_text 部件。
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "assistant");
    assert_eq!(
        body["input"][0]["content"],
        serde_json::json!([
            {"type": "output_text", "text": "summary"},
            {"type": "output_text", "text": "detail"},
        ])
    );
}

#[test]
fn mixed_assistant_item_splits_native_reasoning_in_order() {
    let mut item = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "before".into(),
                cache_control: None,
            },
            ContentBlock::Reasoning {
                summary: vec!["summary".into()],
                content: vec!["detail".into()],
                encrypted_content: Some("opaque".into()),
            },
            ContentBlock::Text {
                text: "after".into(),
                cache_control: None,
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    item.tool_calls = Some(vec![stravia_runtime_contract::protocol::ir::ToolCall {
        id: "call_1".into(),
        name: "lookup".into(),
        arguments: "{}".into(),
    }]);
    let request = AiRequest::new("gpt", vec![item]);

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode mixed assistant item");

    let input = body["input"].as_array().unwrap();
    // 前面的普通块 flush 成 message，Reasoning 独立成 item，剩余文本与
    // tool_calls 依次跟上。
    assert_eq!(input[0]["type"], "message");
    assert_eq!(
        input[0]["content"],
        serde_json::json!([{"type": "output_text", "text": "before"}])
    );
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["summary"][0]["text"], "summary");
    assert_eq!(input[1]["content"], serde_json::json!([]));
    assert_eq!(input[1]["encrypted_content"], "opaque");
    assert_eq!(input[2]["type"], "message");
    assert_eq!(
        input[2]["content"],
        serde_json::json!([
            {"type": "output_text", "text": "detail"},
            {"type": "output_text", "text": "after"}
        ])
    );
    assert_eq!(input[3]["type"], "function_call");
    assert_eq!(input[3]["call_id"], "call_1");
}

#[test]
fn mixed_assistant_item_merges_degraded_reasoning_into_message() {
    let item = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "before".into(),
                cache_control: None,
            },
            ContentBlock::Reasoning {
                summary: vec!["summary".into()],
                content: vec!["detail".into()],
                encrypted_content: None,
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new("gpt", vec![item]);

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode mixed assistant item");

    // 无 id/密文的 Reasoning 降级为 output_text，与前后文本合并在同一 message。
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(
        input[0]["content"],
        serde_json::json!([
            {"type": "output_text", "text": "before"},
            {"type": "output_text", "text": "summary"},
            {"type": "output_text", "text": "detail"},
        ])
    );
}

#[test]
fn redacted_thinking_is_silently_dropped() {
    let request = AiRequest::new(
        "gpt",
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
                content: MessageContent::Blocks(vec![
                    ContentBlock::RedactedThinking {
                        data: "redacted-payload".into(),
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).expect("encode");

    assert_eq!(body["input"][1]["type"], "message");
    assert_eq!(
        body["input"][1]["content"],
        serde_json::json!([{"type": "output_text", "text": "answer"}])
    );
    assert!(!body.to_string().contains("redacted-payload"));
}

#[test]
fn assistant_item_with_only_redacted_thinking_emits_nothing() {
    let request = AiRequest::new(
        "gpt",
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
                content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
                    data: "redacted-payload".into(),
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );

    let (body, _) = ResponsesEncoder.encode_request(&request).expect("encode");

    // 只剩承载不了的受保护载荷的 assistant 条目整条跳过，不发出空 message。
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
    assert!(!body.to_string().contains("redacted-payload"));
}

#[test]
fn degraded_reasoning_message_does_not_reuse_reasoning_item_id() {
    use stravia_runtime_contract::protocol::ir::{AiItemAudience, AiItemProvenance, AiItemStatus};

    // 跨来源回放：密文缺失或为空的 Reasoning、无/有签名的 Thinking 都没有
    // 原生载体，明文降级为 message。拆出的新载体不能借用推理条目的 `rs_`
    // 图 id（上游要求 message id 以 `msg` 开头）、status、phase 或来源侧
    // 原生扩展字段。
    for item in [
        AiItem::reasoning(vec!["summary".into()], Vec::new(), None),
        AiItem::reasoning(vec!["summary".into()], Vec::new(), Some(String::new())),
        AiItem::thinking("summary", None),
        AiItem::thinking("summary", Some("anthropic-signature".into())),
    ] {
        let mut item = item.with_graph_metadata(
            Some("rs_gateway_0".into()),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        );
        let meta = item.meta.as_mut().expect("graph metadata creates meta");
        meta.insert_extension("phase", serde_json::json!("commentary"))
            .expect("phase is not reserved");
        meta.insert_extension(
            "__open_responses_item_fields",
            serde_json::json!({
                "internal_chat_message_metadata_passthrough": {"trace": "opaque"}
            }),
        )
        .expect("item fields is not reserved");

        let (body, _) = ResponsesEncoder
            .encode_request(&AiRequest::new("gpt", vec![item]))
            .expect("encode degraded reasoning");

        let input = body["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        let message = &input[0];
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(
            message["content"],
            serde_json::json!([{"type": "output_text", "text": "summary"}])
        );
        for field in [
            "id",
            "status",
            "phase",
            "internal_chat_message_metadata_passthrough",
        ] {
            assert!(
                message.get(field).is_none(),
                "degraded message must not borrow source item `{field}`: {message}"
            );
        }
    }
}

#[test]
fn mixed_reasoning_tool_use_split_does_not_reuse_item_identity() {
    use stravia_runtime_contract::protocol::ir::{
        AiItemAudience, AiItemProvenance, AiItemStatus, ToolCall,
    };

    // 双表示边界：ToolUse 块与 canonical tool_calls 指向同一 call（len=1）。
    // 拆出的降级 message 与派生 function_call 都是新载体，都不能借用父推理
    // 条目的 `rs_` 身份或原生扩展字段。
    let mut item = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::Thinking {
                thinking: "chain".into(),
                signature: Some("anthropic-signature".into()),
            },
            ContentBlock::Reasoning {
                summary: vec!["sum".into()],
                content: vec!["detail".into()],
                encrypted_content: None,
            },
            ContentBlock::Text {
                text: "answer".into(),
                cache_control: None,
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "lookup".into(),
                input: serde_json::json!({"q": "x"}),
                cache_control: None,
            },
        ]),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"q\":\"x\"}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    item.set_graph_metadata(
        Some("rs_gateway_0".into()),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    );
    let meta = item.meta.as_mut().expect("graph metadata creates meta");
    meta.insert_extension("phase", serde_json::json!("commentary"))
        .expect("phase is not reserved");
    meta.insert_extension(
        "__open_responses_item_fields",
        serde_json::json!({
            "internal_chat_message_metadata_passthrough": {"trace": "opaque"}
        }),
    )
    .expect("item fields is not reserved");

    let request = AiRequest::new(
        "gpt",
        vec![
            item,
            AiItem::function_call_output("call_1", serde_json::json!({"rows": 2}))
                .with_graph_metadata(
                    Some("fco_gateway_1".into()),
                    Some(AiItemStatus::Completed),
                    AiItemProvenance::Client,
                    AiItemAudience::Provider,
                ),
        ],
    );

    let (body, _) = ResponsesEncoder
        .encode_request(&request)
        .expect("encode mixed reasoning tool history");

    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 3);

    // 明文推理段与正文按原顺序合并为同一个降级 message。
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "assistant");
    assert_eq!(
        input[0]["content"],
        serde_json::json!([
            {"type": "output_text", "text": "chain"},
            {"type": "output_text", "text": "sum"},
            {"type": "output_text", "text": "detail"},
            {"type": "output_text", "text": "answer"},
        ])
    );
    for field in [
        "id",
        "status",
        "phase",
        "internal_chat_message_metadata_passthrough",
    ] {
        assert!(
            input[0].get(field).is_none(),
            "degraded message must not borrow source item `{field}`: {}",
            input[0]
        );
    }

    // ToolUse 块派生的 function_call 完整保留 call_id/name/arguments，
    // 同样不得继承父条目身份。
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_1");
    assert_eq!(input[1]["name"], "lookup");
    assert_eq!(
        serde_json::from_str::<Value>(input[1]["arguments"].as_str().expect("arguments string"))
            .expect("arguments are JSON"),
        serde_json::json!({"q": "x"})
    );
    for field in ["id", "status", "internal_chat_message_metadata_passthrough"] {
        assert!(
            input[1].get(field).is_none(),
            "derived function_call must not borrow source item `{field}`: {}",
            input[1]
        );
    }

    // 结果条目是独立条目，保留自身合法图身份与完整输出。
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["id"], "fco_gateway_1");
    assert_eq!(input[2]["status"], "completed");
    assert_eq!(
        serde_json::from_str::<Value>(input[2]["output"].as_str().expect("output string"))
            .expect("tool output is JSON text"),
        serde_json::json!({"rows": 2})
    );
}

#[test]
fn plain_assistant_message_keeps_provider_item_identity() {
    use stravia_runtime_contract::protocol::ir::{AiItemAudience, AiItemProvenance, AiItemStatus};

    // 对照组：未拆分的普通 assistant message 原样保留合法 `msg_` 图 id、
    // status 与 phase，守卫不能误删真实身份。
    let mut item = AiItem::output_text("answer").with_graph_metadata(
        Some("msg_1".into()),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    );
    item.meta
        .as_mut()
        .expect("graph metadata creates meta")
        .insert_extension("phase", serde_json::json!("final_answer"))
        .expect("phase is not reserved");

    let (body, _) = ResponsesEncoder
        .encode_request(&AiRequest::new("gpt", vec![item]))
        .expect("encode plain assistant message");

    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "assistant");
    assert_eq!(body["input"][0]["id"], "msg_1");
    assert_eq!(body["input"][0]["status"], "completed");
    assert_eq!(body["input"][0]["phase"], "final_answer");
}

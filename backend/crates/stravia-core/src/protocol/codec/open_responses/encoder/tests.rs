use super::*;

use crate::protocol::codec::anthropic::messages::decoder::AnthropicDecoder;
use crate::protocol::codec::open_responses::decoder::ResponsesDecoder;
use crate::protocol::codec::openai::compatible::decoder::OpenAIDecoder;
use crate::protocol::ir::AiItem;

#[test]
fn target_effort_maps_to_responses_shape() {
    let mut request = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "messages": [{"role": "user", "content": "hello"}],
            "reasoning_effort": "max"
        }))
        .unwrap();
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "xhigh".into(),
    });

    let (body, _) = ResponsesEncoder.encode_request(&request).unwrap();

    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn chat_reasoning_content_preserves_tool_call_before_its_output() {
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

    assert_eq!(body["input"][0]["type"], "reasoning");
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][1]["call_id"], "call_glob");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["call_id"], "call_glob");
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
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "medium".into(),
    });

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
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "medium".into(),
    });

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
        request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
            value: "medium".into(),
        });

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
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "max".into(),
    });

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
    request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            store: Some(true),
            ..Default::default()
        },
    ));

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
    request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            metadata: Some(serde_json::json!({"tenant": "acme"})),
            safety_identifier: Some("safe-user-1".into()),
            ..Default::default()
        },
    ));

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
    let request = crate::protocol::codec::open_responses::decoder::ResponsesDecoder
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
    request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            previous_response_id: Some("resp_parent".into()),
            ..Default::default()
        },
    ));

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
fn stringifies_structured_single_tool_results() {
    let request = AiRequest::new(
        "gpt",
        vec![AiItem {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: serde_json::json!({"temperature": 21}),
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
    assert_eq!(
        body["input"][0]["output"],
        serde_json::Value::String(r#"{"temperature":21}"#.into())
    );
}

#[test]
fn stringifies_invalid_tool_result_content_arrays() {
    let output = encode_tool_output(&MessageContent::Blocks(vec![ContentBlock::ToolResult {
        tool_use_id: "call_1".into(),
        content: serde_json::json!([
            {"type": "tool_result", "content": {"temperature": 21}}
        ]),
        is_error: None,
        cache_control: None,
    }]))
    .expect("normalize invalid content array");

    assert_eq!(
        output,
        Value::String(r#"[{"content":{"temperature":21},"type":"tool_result"}]"#.into())
    );
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

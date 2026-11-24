use super::*;

#[test]
fn gateway_native_reasoning_projects_effort_and_off_levels() {
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

    for (value, level, expected) in [
        ("none", Some(ThinkingLevel::Off), Some("none")),
        ("provider-default", None, None),
        ("high", Some(ThinkingLevel::High), Some("high")),
    ] {
        let request = GatewayLanguageModelV4
            .decode_request(json!({"prompt": [], "reasoning": value}))
            .unwrap();
        assert_eq!(request.reasoning.level, level);
        let (encoded, _) = GatewayLanguageModelV4.encode_request(request).unwrap();
        assert_eq!(encoded.get("reasoning").and_then(Value::as_str), expected);
    }

    let mut controlled = AiRequest::new("model", vec![]);
    controlled.reasoning.target_control = Some(TargetThinkingControl::Effort {
        value: "off".into(),
    });
    assert_eq!(
        GatewayLanguageModelV4
            .encode_request(controlled.clone())
            .unwrap()
            .0["reasoning"],
        "none"
    );
    controlled.reasoning.target_control = Some(TargetThinkingControl::Effort {
        value: "xhigh".into(),
    });
    assert_eq!(
        GatewayLanguageModelV4.encode_request(controlled).unwrap().0["reasoning"],
        "xhigh"
    );
}

#[test]
fn gateway_top_k_uses_platform_control_without_raw_parameter_echo() {
    let mut request = AiRequest::new("model", vec![]);
    request.parameters.top_k = Some(0);
    request.parameters.min_p = Some(0.0);
    request.parameters.logprobs = Some(0);

    let (body, _) = GatewayLanguageModelV4.encode_request(request).unwrap();
    assert_eq!(body["topK"], 0);
    assert!(body.get("min_p").is_none());
    assert!(body.get("logprobs").is_none());
    let decoded = GatewayLanguageModelV4.decode_request(body).unwrap();
    assert_eq!(decoded.parameters.top_k, Some(0));
}

#[test]
fn gateway_skips_unrepresentable_choices_and_null_controls() {
    let decoded = GatewayLanguageModelV4
        .decode_request(json!({
            "prompt": [], "tools": null, "toolChoice": null, "responseFormat": null,
            "providerOptions": {"untrusted": {"control": true}}
        }))
        .unwrap();
    assert!(
        decoded
            .parameters
            .resets(stravia_runtime_contract::protocol::ir::ParameterReset::ResponseFormat)
    );
    let body = GatewayLanguageModelV4.encode_request(decoded).unwrap().0;
    for field in ["tools", "toolChoice", "responseFormat", "providerOptions"] {
        assert!(body.get(field).is_none(), "{field}");
    }
    for choice in [
        ToolChoice::Allowed {
            names: vec!["lookup".into()],
            required: true,
        },
        ToolChoice::Native(stravia_runtime_contract::protocol::ir::NativeToolKind::WebSearch),
    ] {
        let mut request = AiRequest::new("model", vec![]);
        request.tool_choice = Some(choice);
        let body = GatewayLanguageModelV4.encode_request(request).unwrap().0;
        assert!(body.get("toolChoice").is_none());
    }
}

#[test]
fn projects_nested_gateway_results_preserving_error_and_json_semantics() {
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
            Some(json!({"type": "error-text", "value": "keep more"})),
        ),
        (
            ToolResultContentKind::Json,
            mixed.clone(),
            Some(json!({"type": "error-json", "value": mixed})),
        ),
        (
            ToolResultContentKind::Json,
            json!({"type": "image", "business": true}),
            Some(json!({"type": "error-json", "value": {"type": "image", "business": true}})),
        ),
        (
            ToolResultContentKind::ContentBlocks,
            json!([{"type": "image"}, {"type": "unknown", "text": "drop"}]),
            None,
        ),
    ] {
        let mut call = AiItem::output_text("calling");
        call.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            name: "weather".into(),
            arguments: "{}".into(),
        }]);
        let mut result = AiItem::function_call_output("call_1", json!("unused"));
        result.content = MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content_kind: Some(kind),
            content,
            is_error: Some(true),
            cache_control: None,
        }]);
        let (body, _) = GatewayLanguageModelV4
            .encode_request(AiRequest::new("model", vec![call, result]))
            .unwrap();
        if let Some(expected) = expected {
            let output = &body["prompt"][1]["content"][0];
            assert_eq!(output["toolCallId"], "call_1");
            assert_eq!(output["toolName"], "weather");
            assert_eq!(output["output"], expected);
        } else {
            assert_eq!(body["prompt"].as_array().unwrap().len(), 1);
        }
    }
}
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::MessageContent;

#[test]
fn encodes_ai_sdk_v4_language_model_wire() {
    let request = AiRequest::new(
        "ignored-by-gateway",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let (body, headers) = GatewayLanguageModelV4
        .encode_request(request.clone())
        .unwrap();

    assert_eq!(
        GatewayLanguageModelV4.request_path("anything", false),
        "/language-model"
    );
    assert_eq!(
        body["prompt"][0],
        json!({"role": "user", "content": [{"type": "text", "text": "hello"}]})
    );
    assert!(body.get("model").is_none());
    assert!(body.get("messages").is_none());
    assert_eq!(
        headers
            .get("ai-language-model-specification-version")
            .unwrap(),
        "4"
    );
    assert_eq!(
        headers.get("ai-language-model-id").unwrap(),
        "ignored-by-gateway"
    );
    assert_eq!(headers.get("ai-language-model-streaming").unwrap(), "false");

    let mut streaming_request = request;
    streaming_request.stream.enabled = true;
    let (_, streaming_headers) = GatewayLanguageModelV4
        .encode_request(streaming_request)
        .unwrap();
    assert_eq!(
        streaming_headers
            .get("ai-language-model-streaming")
            .unwrap(),
        "true"
    );
}

#[test]
fn decodes_ai_sdk_v4_response() {
    let response = GatewayLanguageModelV4
            .decode_response(json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "tool-call", "toolCallId": "call_1", "toolName": "weather", "input": {"city": "Paris"}}
                ],
                "finishReason": {"unified": "tool-calls"},
                "usage": {
                    "inputTokens": {"total": 3},
                    "outputTokens": {"total": 5}
                },
                "response": {"id": "resp_1", "modelId": "openai/gpt-5"}
            }))
            .unwrap();

    assert_eq!(response.id, "resp_1");
    assert_eq!(response.model, "openai/gpt-5");
    assert_eq!(response.output_text(), "hello");
    assert_eq!(response.tool_calls().count(), 1);
    assert_eq!(response.stop_reason.as_deref(), Some("tool-calls"));
    assert_eq!(response.usage.total_tokens, 8);
}

#[test]
fn parses_ai_sdk_v4_stream_events() {
    let mut parser = GatewayStreamParser::new();
    let deltas = parser
            .parse_chunk(
                "data: {\"type\":\"response-metadata\",\"id\":\"resp_1\",\"modelId\":\"openai/gpt-5\"}\n\n\
                 data: {\"type\":\"text-delta\",\"id\":\"text_1\",\"delta\":\"hello\"}\n\n\
                 data: {\"type\":\"finish\",\"finishReason\":{\"unified\":\"stop\"},\"usage\":{\"inputTokens\":{\"total\":3},\"outputTokens\":{\"total\":5}}}\n\n",
            )
            .unwrap();

    assert!(matches!(deltas[0], AiStreamDelta::MessageStart { .. }));
    assert!(matches!(&deltas[1], AiStreamDelta::TextDelta(text) if text == "hello"));
    assert!(matches!(
        deltas[2],
        AiStreamDelta::Usage(Usage {
            total_tokens: 8,
            ..
        })
    ));
    assert!(matches!(&deltas[3], AiStreamDelta::Done { stop_reason } if stop_reason == "stop"));
}

#[test]
fn replays_reasoning_natively_and_drops_protected_only_assistant() {
    let block = |block: ContentBlock| AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![block]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new(
        "ignored-by-gateway",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            block(ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: Some("sig_protected".into()),
            }),
            block(ContentBlock::Reasoning {
                summary: vec!["summary".into()],
                content: vec![String::new(), "detail".into()],
                encrypted_content: Some("enc_protected".into()),
            }),
            // 只含密文载荷、编码为空且无 tool call：整条跳过。
            block(ContentBlock::RedactedThinking {
                data: "redacted".into(),
            }),
        ],
    );

    let (body, _) = GatewayLanguageModelV4.encode_request(request).unwrap();
    let prompt = body["prompt"].as_array().unwrap();
    assert_eq!(prompt.len(), 3);
    assert_eq!(
        prompt[1]["content"],
        json!([{"type": "reasoning", "text": "let me think"}])
    );
    assert_eq!(
        prompt[2]["content"],
        json!([
            {"type": "reasoning", "text": "summary"},
            {"type": "reasoning", "text": "detail"},
        ])
    );
    // 受保护载荷无原生载体，绝不能出现在线上 body 里。
    let body_text = serde_json::to_string(&body).unwrap();
    assert!(!body_text.contains("sig_protected"));
    assert!(!body_text.contains("enc_protected"));
    assert!(!body_text.contains("redacted"));
}

#[test]
fn omits_role_specific_unsupported_blocks_without_stringifying_results() {
    let names = BTreeMap::from([("call_1".into(), "weather".into())]);
    for role in [
        Role::System,
        Role::Developer,
        Role::User,
        Role::Assistant,
        Role::Tool,
    ] {
        let mut item = AiItem {
            role,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "keep".to_owned().into(),
                    cache_control: None,
                },
                ContentBlock::Audio {
                    source: MediaSource::Url("https://example.com/audio.wav".into()),
                },
            ]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        };
        let message = encode_message(&item, &names).unwrap().unwrap();
        match role {
            Role::System | Role::Developer => assert_eq!(message["content"], json!("keep")),
            Role::User | Role::Assistant => assert_eq!(
                message["content"],
                json!([{"type": "text", "text": "keep"}])
            ),
            Role::Tool => assert_eq!(
                message["content"][0]["output"],
                json!({"type": "text", "value": "keep"})
            ),
        }
        item.content = MessageContent::Blocks(vec![ContentBlock::Audio {
            source: MediaSource::Url("https://example.com/audio.wav".into()),
        }]);
        assert!(encode_message(&item, &names).unwrap().is_none());
    }
    let output = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "keep".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: json!([{"type":"input_audio","data":"aGk="}]),
            content_kind: Some(
                stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
            ),
            is_error: None,
            cache_control: None,
        },
    ]);
    assert_eq!(tool_result_text(&output).unwrap(), "keep");
    let output = MessageContent::Blocks(vec![ContentBlock::ToolResult {
        tool_use_id: "call_1".into(),
        content: json!("plain result"),
        content_kind: None,
        is_error: None,
        cache_control: None,
    }]);
    assert_eq!(tool_result_text(&output).unwrap(), "plain result");
    let image = MessageContent::Blocks(vec![ContentBlock::Image {
        source: MediaSource::Url("https://example.com/image.png".into()),
        detail: None,
        cache_control: None,
    }]);
    assert!(encode_content_parts(&image, true).unwrap().is_empty());
    assert_eq!(encode_content_parts(&image, false).unwrap().len(), 1);
}

#[test]
fn preserves_json_tool_results_in_native_output_carrier() {
    let names = BTreeMap::from([("call_1".into(), "weather".into())]);
    let item = AiItem::function_call_output("call_1", json!({"temperature": 23}));
    let message = encode_message(&item, &names).unwrap().unwrap();
    assert_eq!(
        message["content"][0]["output"],
        json!({
            "type": "json", "value": {"temperature": 23},
        })
    );
}

#[test]
fn roundtrips_native_tool_output_values_and_errors_without_stringification() {
    let names = BTreeMap::from([("call_1".into(), "weather".into())]);
    for output in [
        json!({"type": "text", "value": "plain"}),
        json!({"type": "text", "value": ""}),
        json!({"type": "error-text", "value": "failed"}),
        json!({"type": "json", "value": "plain"}),
        json!({"type": "json", "value": {"temperature": 23}}),
        json!({"type": "error-json", "value": [1,2]}),
    ] {
        let item = decode_message(&json!({
            "role": "tool", "content": [{
                "type": "tool-result", "toolCallId": "call_1", "toolName": "weather", "output": output,
            }],
        })).unwrap();
        assert_eq!(
            encode_message(&item, &names).unwrap().unwrap()["content"][0]["output"],
            output
        );
    }
    for output in [
        json!({"type": "text", "value": 1}),
        json!({"type": "json"}),
        json!({"value": "missing type"}),
    ] {
        assert!(decode_message(&json!({
            "role": "tool", "content": [{"type": "tool-result", "toolCallId": "call_1", "output": output}],
        })).is_err());
    }
}

#[test]
fn response_omits_malformed_calls_but_preserves_native_json_inputs() {
    let mut response = AiResponse::new("response", "model");
    let mut item = AiItem::output_text("keep");
    item.tool_calls = Some(vec![
        ToolCall {
            id: "bad".into(),
            name: "weather".into(),
            arguments: "not-json".into(),
        },
        ToolCall {
            id: "valid".into(),
            name: "weather".into(),
            arguments: "[1,2]".into(),
        },
    ]);
    response.items.push(item);
    assert_eq!(
        GatewayLanguageModelV4.encode_response(&response)["content"],
        json!([
            {"type": "text", "text": "keep"},
            {"type": "tool-call", "toolCallId": "valid", "toolName": "weather", "input": [1,2]},
        ])
    );
}

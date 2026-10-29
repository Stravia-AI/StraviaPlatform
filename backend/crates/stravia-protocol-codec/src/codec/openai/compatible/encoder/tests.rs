use super::*;

#[test]
fn canonical_response_groups_preserve_full_steps_and_parallel_results() {
    use stravia_runtime_contract::protocol::ir::{AiResponse, response::mark_response_history};
    let call = |id: &str, arguments: &str| AiItem {
        role: Role::Assistant,
        content: MessageContent::Text(String::new().into()),
        tool_calls: Some(vec![ToolCall {
            id: id.into(),
            name: "lookup".into(),
            arguments: arguments.into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    let result = |id: &str, text: &str| AiItem {
        role: Role::Tool,
        content: MessageContent::Text(text.to_owned().into()),
        tool_calls: None,
        tool_call_id: Some(id.into()),
        meta: None,
    };
    let mut first = AiResponse::new("first", "model");
    first.items = vec![
        AiItem::output_text("Checking both keys."),
        AiItem::thinking(
            "Look up both keys.",
            Some("native-signature-not-plaintext".into()),
        ),
        AiItem::reasoning(
            Vec::new(),
            Vec::new(),
            Some("native-ciphertext-not-plaintext".into()),
        ),
        call("a", "{\"key\": \"A\"}"),
        AiItem::output_text(" Please wait."),
        call("b", "{\"key\": \"B\"}"),
    ];
    mark_response_history(&mut first.items, "first-response");
    let mut items = first.items;
    items.extend([result("b", "2"), result("a", "1")]);
    let mut second = AiResponse::new("second", "model");
    second.items = vec![
        AiItem::thinking("Add the returned values.", None),
        AiItem::output_text("3"),
    ];
    mark_response_history(&mut second.items, "second-response");
    items.extend(second.items);
    // Distinct completed responses remain distinct even without a role change.
    let mut third = AiResponse::new("third", "model");
    third.items = vec![
        AiItem::thinking("Check the answer.", None),
        AiItem::output_text("Confirmed."),
    ];
    mark_response_history(&mut third.items, "third-response");
    items.extend(third.items);
    let (body, _) = OpenAIEncoder
        .encode_request(AiRequest::new("model", items))
        .unwrap();
    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role":"assistant","content":"Checking both keys. Please wait.","reasoning_content":"Look up both keys.","tool_calls":[
                {"id":"a","type":"function","function":{"name":"lookup","arguments":"{\"key\": \"A\"}"}},
                {"id":"b","type":"function","function":{"name":"lookup","arguments":"{\"key\": \"B\"}"}}
            ]},
            {"role":"tool","tool_call_id":"b","content":"2"},
            {"role":"tool","tool_call_id":"a","content":"1"},
            {"role":"assistant","content":"3","reasoning_content":"Add the returned values."},
            {"role":"assistant","content":"Confirmed.","reasoning_content":"Check the answer."}
        ])
    );
}

#[test]
fn ungrouped_chat_messages_preserve_boundaries_and_parallel_calls() {
    use crate::transform::ProtocolTransform;
    use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    let messages = serde_json::json!([
        {"role":"user","content":"Look up both keys."},
        {"role":"assistant","content":"First.","reasoning_content":"First reasoning."},
        {"role":"assistant","content":"","reasoning_content":"Separate reasoning."},
        {"role":"assistant","content":" \n"},
        {"role":"assistant","content":"Checking.","reasoning_content":"Call both tools.","tool_calls":[
            {"id":"a","type":"function","function":{"name":"lookup","arguments":"{\"key\": \"A\"}"}},
            {"id":"b","type":"function","function":{"name":"lookup","arguments":"{\"key\": \"B\"}"}}
        ]},
        {"role":"assistant","content":"Do not delete this message."},
        {"role":"tool","tool_call_id":"b","content":"2"},
        {"role":"tool","tool_call_id":"a","content":"1"}
    ]);
    let mut input = serde_json::json!({"model":"model","messages":messages});
    for item in input["messages"].as_array_mut().unwrap() {
        item[RESPONSE_HISTORY_GROUP_META] = "untrusted-group".into();
    }
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let request = pair.decode_request(input).unwrap();
    let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
    assert_eq!(body["messages"], messages);
}

#[test]
fn chat_logprobs_merge_and_identifier_precedence_use_unified_intent() {
    use super::super::decoder::OpenAIDecoder;
    for (controls, expected) in [
        (serde_json::json!({"logprobs": true}), Some(0)),
        (
            serde_json::json!({"logprobs": true, "top_logprobs": 3}),
            Some(3),
        ),
        (serde_json::json!({"top_logprobs": 2}), Some(2)),
        (
            serde_json::json!({"logprobs": false, "top_logprobs": 3}),
            None,
        ),
    ] {
        let mut input = serde_json::json!({
            "model": "model", "messages": [{"role": "user", "content": "hello"}],
            "user": "legacy-user", "safety_identifier": "preferred-user", "n": 2
        });
        input
            .as_object_mut()
            .unwrap()
            .extend(controls.as_object().unwrap().clone());
        let request = OpenAIDecoder.decode_request(input).unwrap();
        assert_eq!(request.parameters.logprobs, expected);
        assert_eq!(
            request.parameters.end_user_id.as_deref(),
            Some("preferred-user")
        );
        assert_eq!(request.parameters.candidate_count, Some(2));
        let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
        assert_eq!(body["safety_identifier"], "preferred-user");
        assert!(body.get("user").is_none());
        assert_eq!(body["n"], 2);
        if let Some(count) = expected {
            assert_eq!(body["logprobs"], true);
            assert_eq!(body["top_logprobs"], count);
        } else {
            assert!(body.get("logprobs").is_none());
            assert!(body.get("top_logprobs").is_none());
        }
    }
}

#[test]
fn chat_null_controls_reset_inheritance_without_emitting_nulls() {
    use super::super::decoder::OpenAIDecoder;
    use stravia_runtime_contract::protocol::ir::ParameterReset;
    let request = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "model", "messages": [{"role": "user", "content": "hello"}],
            "temperature": null, "top_p": null, "max_completion_tokens": null,
            "presence_penalty": null, "frequency_penalty": null, "parallel_tool_calls": null,
            "store": null, "prompt_cache_key": null, "prompt_cache_retention": null,
            "verbosity": null, "service_tier": null, "reasoning_effort": null,
            "response_format": null, "logprobs": null, "user": null,
            "web_search_options": null
        }))
        .unwrap();
    for reset in [
        ParameterReset::Temperature,
        ParameterReset::TopP,
        ParameterReset::MaxTokens,
        ParameterReset::PresencePenalty,
        ParameterReset::FrequencyPenalty,
        ParameterReset::ParallelToolCalls,
        ParameterReset::Store,
        ParameterReset::CacheKey,
        ParameterReset::CacheRetention,
        ParameterReset::Verbosity,
        ParameterReset::ServiceTier,
        ParameterReset::ReasoningLevel,
        ParameterReset::ResponseFormat,
        ParameterReset::Logprobs,
        ParameterReset::EndUser,
        ParameterReset::NativeTools,
    ] {
        assert!(request.parameters.resets(reset), "{reset:?}");
    }
    let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
    assert!(
        body.as_object()
            .unwrap()
            .values()
            .all(|value| !value.is_null())
    );
    assert!(body.get("resets").is_none());
}

#[test]
fn chat_default_web_search_is_enabled_and_empty_tools_reset_inheritance() {
    use super::super::decoder::OpenAIDecoder;
    use stravia_runtime_contract::protocol::ir::{NativeTool, ParameterReset};
    let request = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "model", "messages": [{"role": "user", "content": "hello"}],
            "web_search_options": {}, "tools": []
        }))
        .unwrap();
    assert!(request.parameters.resets(ParameterReset::NativeTools));
    assert!(matches!(
        request.parameters.native_tools.as_slice(),
        [NativeTool::WebSearch(_)]
    ));
    let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
    assert_eq!(body["web_search_options"], serde_json::json!({}));
}

#[test]
fn chat_only_projects_supported_tool_choices() {
    use stravia_runtime_contract::protocol::ir::NativeToolKind;
    for (choice, expected) in [
        (ToolChoice::Auto, Some(serde_json::json!("auto"))),
        (ToolChoice::None, Some(serde_json::json!("none"))),
        (ToolChoice::Required, Some(serde_json::json!("required"))),
        (
            ToolChoice::Named {
                name: "lookup".into(),
            },
            Some(serde_json::json!({
                "type": "function", "function": {"name": "lookup"}
            })),
        ),
        (
            ToolChoice::Allowed {
                names: vec!["lookup".into()],
                required: true,
            },
            None,
        ),
        (ToolChoice::Native(NativeToolKind::WebSearch), None),
    ] {
        let mut request = AiRequest::new(
            "model",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text(std::sync::Arc::new("hello".into())),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.tool_choice = Some(choice);
        let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
        assert_eq!(body.get("tool_choice"), expected.as_ref());
    }
}

#[test]
fn chat_drops_unsupported_official_controls_and_unknown_enum_values() {
    use super::super::decoder::OpenAIDecoder;
    let request = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "model", "messages": [{"role": "user", "content": "hello"}],
            "prediction": {"type": "future", "private": true}, "service_tier": "scale",
            "verbosity": "unsupported", "prompt_cache_retention": "forever",
            "modalities": ["text", "private"], "tool_choice": {"type": "unsupported"},
            "tools": [{"type": "file_search", "private": true}],
            "audio": {"voice": "alloy", "format": null, "private": true},
            "stream": true, "stream_options": {"include_obfuscation": true}
        }))
        .unwrap();
    assert!(request.tool_choice.is_none());
    let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
    for key in [
        "prediction",
        "service_tier",
        "verbosity",
        "prompt_cache_retention",
        "tool_choice",
    ] {
        assert!(body.get(key).is_none(), "{key}");
    }
    assert_eq!(body["modalities"], serde_json::json!(["text"]));
    assert_eq!(body["tools"], serde_json::json!([]));
    assert_eq!(body["audio"], serde_json::json!({"voice": "alloy"}));
    assert_eq!(
        body["stream_options"],
        serde_json::json!({"include_usage": false})
    );
}

#[test]
fn named_search_controls_strip_unknown_keys_without_changing_business_payloads() {
    use crate::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
    use crate::transform::ProtocolAdapter;
    let schema = serde_json::json!({"type": "object", "properties": {"private": {"type": "boolean"}}, "business_constraint": true});
    let request = OpenAIChatCompletionsV1.decode_request(serde_json::json!({
        "model": "model", "messages": [{"role": "user", "content": "hello"}],
        "web_search_options": {"search_context_size": "low", "private": true, "user_location": {"type": "approximate", "private": true, "approximate": {"city": "Tokyo", "private": true}}},
        "metadata": {"private": {"business": true}, "label": "business"},
        "tools": [{"type": "function", "function": {"name": "business", "parameters": schema}}],
        "unknown_body": true
    })).unwrap();
    assert!(matches!(
        request.parameters.native_tools.as_slice(),
        [stravia_runtime_contract::protocol::ir::NativeTool::WebSearch(_)]
    ));
    let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
    assert_eq!(
        body["web_search_options"],
        serde_json::json!({"search_context_size": "low", "user_location": {"type": "approximate", "approximate": {"city": "Tokyo"}}})
    );
    assert_eq!(body["metadata"], serde_json::json!({"label": "business"}));
    assert_eq!(body["tools"][0]["function"]["parameters"], schema);
    assert!(body.get("unknown_body").is_none());
}

#[test]
fn chat_parameters_roundtrip_preserves_explicit_values_without_legacy_copies() {
    use crate::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
    use crate::transform::ProtocolAdapter;

    let request = OpenAIChatCompletionsV1.decode_request(serde_json::json!({
        "model": "model", "messages": [{"role": "user", "content": "hello"}],
        "stream": true, "stream_options": {"include_usage": false, "include_obfuscation": false},
        "n": 0, "logprobs": false, "top_logprobs": 0, "modalities": [], "tools": [],
        "logit_bias": {}, "store": false, "metadata": {},
        "prompt_cache_key": "", "prompt_cache_retention": "24h",
        "audio": {"voice": "alloy", "format": "wav"},
        "prediction": {"type": "content", "content": ""},
        "verbosity": "low", "safety_identifier": "", "user": "",
        "seed": 0, "frequency_penalty": 0, "presence_penalty": 0,
        "reasoning_effort": "none", "unknown_future_option": {"private": true}
    })).expect("decode canonical controls");
    assert!(request.meta.vendor.ingress.is_empty());
    assert_eq!(request.parameters.candidate_count, Some(0));
    assert_eq!(request.parameters.logprobs, None);
    assert!(
        request
            .parameters
            .resets(stravia_runtime_contract::protocol::ir::ParameterReset::Logprobs)
    );
    assert_eq!(
        request.parameters.modalities.as_deref(),
        Some([].as_slice())
    );
    let (body, _) = OpenAIEncoder
        .encode_request(request)
        .expect("encode canonical controls");
    for (name, expected) in [
        ("n", serde_json::json!(0)),
        ("modalities", serde_json::json!([])),
        ("logit_bias", serde_json::json!({})),
        ("tools", serde_json::json!([])),
        ("store", serde_json::json!(false)),
        ("metadata", serde_json::json!({})),
        ("prompt_cache_key", serde_json::json!("")),
        ("prompt_cache_retention", serde_json::json!("24h")),
        (
            "audio",
            serde_json::json!({"voice": "alloy", "format": "wav"}),
        ),
        ("reasoning_effort", serde_json::json!("none")),
        (
            "stream_options",
            serde_json::json!({"include_usage": false}),
        ),
    ] {
        assert_eq!(body[name], expected, "{name}");
    }
    assert!(body.get("unknown_future_option").is_none());
    assert!(body.get("prediction").is_none());
    assert!(body.get("logprobs").is_none());
    assert!(body.get("top_logprobs").is_none());
    assert!(body.get("user").is_none());
    assert_eq!(body["safety_identifier"], "");
}

#[test]
fn chat_projection_uses_canonical_controls_and_unified_service_tier() {
    use stravia_runtime_contract::protocol::ir::ServiceTier;

    let mut request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text(std::sync::Arc::new("hello".into())),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.parameters.candidate_count = Some(0);
    request.parameters.modalities = Some(Vec::new());
    request.parameters.service_tier = Some(ServiceTier::Auto);
    request.parallel_tool_calls = Some(false);
    let mut legacy = serde_json::to_value(&request).unwrap();
    legacy["ext"] = serde_json::json!({
        "OpenAiChat": {"n": 9, "modalities": ["audio"]}
    });
    let mut request: AiRequest = serde_json::from_value(legacy).unwrap();
    request
        .meta
        .vendor
        .ingress
        .insert("parallel_tool_calls".into(), serde_json::json!(true));
    request
        .meta
        .vendor
        .ingress
        .insert("unknown_future_option".into(), serde_json::json!("private"));

    let (body, _) = OpenAIEncoder
        .encode_request(request)
        .expect("encode canonical priority");
    assert_eq!(body["n"], 0);
    assert_eq!(body["modalities"], serde_json::json!([]));
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["service_tier"], "auto");
    assert!(body.get("unknown_future_option").is_none());
    assert!(body.get("web_search_options").is_none());
}

#[test]
fn malformed_chat_tool_is_a_protocol_error() {
    use crate::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
    use crate::transform::ProtocolAdapter;

    assert!(
        OpenAIChatCompletionsV1
            .decode_request(serde_json::json!({
                "model": "model", "messages": [{"role": "user", "content": "hello"}],
                "tools": [{"type": "function", "function": {"parameters": {}}}]
            }))
            .is_err()
    );
}

#[test]
fn chat_tool_schema_null_branches_are_preserved_exactly() {
    use crate::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
    use crate::transform::ProtocolAdapter;

    let schema = serde_json::json!({
        "type": "object", "additionalProperties": false,
        "properties": {"value": {"anyOf": [{"type": "string"}, {"type": "null"}]}},
        "required": ["value"], "default": null
    });
    let request = OpenAIChatCompletionsV1
        .decode_request(serde_json::json!({
            "model": "model", "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"type": "function", "function": {
                "name": "lookup", "parameters": schema, "strict": true
            }}]
        }))
        .expect("decode nullable schema");
    let (body, _) = OpenAIEncoder
        .encode_request(request)
        .expect("encode nullable schema");
    assert_eq!(body["tools"][0]["function"]["parameters"], schema);
    assert_eq!(body["tools"][0]["function"]["strict"], true);
}

#[test]
fn json_schema_description_survives_without_overriding_canonical_controls() {
    use crate::transform::ProtocolTransform;
    use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    use stravia_runtime_contract::protocol::ir::ResponseFormat;

    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("bind Chat schema round trip");
    let mut request = pair
        .decode_request(serde_json::json!({
            "model": "model",
            "messages": [{"role": "user", "content": "hello"}],
            "response_format": {"type": "json_schema", "json_schema": {
                "name": "original", "description": "Describe the answer",
                "schema": {"type": "object"}, "strict": false
            }}
        }))
        .expect("decode described schema");
    let schema =
        serde_json::json!({"type": "object", "properties": {"answer": {"type": "string"}}});
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "changed".into(),
        description: Some("Canonical description".into()),
        schema: schema.clone(),
        strict: Some(true),
    });
    let body = pair
        .encode_request(request)
        .expect("encode canonical schema changes")
        .body;
    assert_eq!(
        body["response_format"]["json_schema"],
        serde_json::json!({
            "name": "changed", "description": "Canonical description", "schema": schema, "strict": true
        })
    );
}

#[test]
fn anthropic_controls_convert_to_chat_with_exact_tool_strict_values() {
    use crate::transform::ProtocolTransform;
    use stravia_runtime_contract::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    };

    let schema = serde_json::json!({
        "type": "object", "properties": {"answer": {"type": "string"}},
        "required": ["answer"], "additionalProperties": false
    });
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("bind reverse controls");
    for disabled in [false, true] {
        let request = pair
            .decode_request(serde_json::json!({
                "model": "model", "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hello"}],
                "tool_choice": {"type": "auto", "disable_parallel_tool_use": disabled},
                "stop_sequences": ["END", "DONE"],
                "output_config": {"format": {"type": "json_schema", "schema": schema}},
                "tools": [
                    {"name": "strict", "input_schema": schema, "strict": true},
                    {"name": "non_strict", "input_schema": schema, "strict": false},
                    {"name": "unspecified", "input_schema": schema}
                ]
            }))
            .expect("decode Anthropic controls");
        let body = pair
            .encode_request(request)
            .expect("encode Chat controls")
            .body;
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], !disabled);
        assert_eq!(body["stop"], serde_json::json!(["END", "DONE"]));
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["tools"][0]["function"]["strict"], true);
        assert_eq!(body["tools"][1]["function"]["strict"], false);
        assert!(body["tools"][2]["function"].get("strict").is_none());
    }
}

#[test]
fn tool_result_payload_kind_controls_chat_text_projection() {
    use stravia_runtime_contract::protocol::ir::ToolResultContentKind;

    let business_array = serde_json::json!([
        {"type": "text", "text": "business", "extra": [1, 2]},
        {"type": "image", "source": {"type": "url", "url": "https://example.invalid/business"}},
        {"type": "unrecognized", "payload": true}
    ]);
    for (content, kind, expected) in [
        (
            serde_json::json!([
                {"type": "text", "text": "first"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "image-data"}},
                {"type": "unknown", "raw": {"private": "discard"}},
                {"type": "unrecognized", "payload": "discard"},
                {"type": "text", "text": "second"}
            ]),
            Some(ToolResultContentKind::ContentBlocks),
            Some("firstsecond".to_owned()),
        ),
        (
            business_array.clone(),
            Some(ToolResultContentKind::Json),
            Some(business_array.to_string()),
        ),
        (
            serde_json::json!([
                {"type": "image", "source": {"type": "url", "url": "https://example.invalid/image"}},
                {"type": "unknown", "raw": {"private": "discard"}}
            ]),
            Some(ToolResultContentKind::ContentBlocks),
            None,
        ),
        (
            serde_json::json!("literal result"),
            Some(ToolResultContentKind::ContentBlocks),
            Some("literal result".into()),
        ),
        (
            serde_json::json!([]),
            Some(ToolResultContentKind::ContentBlocks),
            None,
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
        let (body, _) = OpenAIEncoder
            .encode_request(request)
            .expect("encode tool result");
        let results: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        if let Some(expected) = expected {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0]["tool_call_id"], "call_lookup");
            assert_eq!(results[0]["content"], expected);
        } else {
            assert!(
                results.is_empty(),
                "unsupported tool results must not become JSON text"
            );
        }
    }
}

#[test]
fn request_roundtrip_preserves_escaped_text_and_multimodal_parts() {
    use crate::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
    use crate::transform::{ProtocolAdapter, ProtocolTransform};

    for content in [
        serde_json::json!(""),
        serde_json::json!("原文 🐟\n\"quote\"\\path\tend"),
        serde_json::json!([
            {"type": "text", "text": "原文 🐟\n"},
            {"type": "image_url", "image_url": {"url": "https://example.invalid/image", "detail": "auto"}},
            {"type": "input_audio", "input_audio": {"data": "YWJj", "format": "wav"}}
        ]),
    ] {
        let request = OpenAIChatCompletionsV1
            .decode_request(serde_json::json!({
                "model": "model",
                "messages": [{"role": "user", "content": content}]
            }))
            .expect("decode content");
        let encoded = ProtocolTransform::encode_request_with(&OpenAIChatCompletionsV1, request)
            .expect("encode content");
        assert_eq!(encoded.body["messages"][0]["role"], "user");
        assert_eq!(encoded.body["messages"][0]["content"], content);
    }
}

#[test]
fn request_decode_rejects_invalid_text_or_parts() {
    use crate::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
    use crate::transform::ProtocolAdapter;

    for content in [
        serde_json::json!(false),
        serde_json::json!(42),
        serde_json::json!({"type": "text", "text": "not an array"}),
        serde_json::json!(["not a part"]),
        serde_json::json!([{"type": "text", "text": 42}]),
        serde_json::json!([{"type": "unknown_part"}]),
    ] {
        assert!(
            OpenAIChatCompletionsV1
                .decode_request(serde_json::json!({
                    "model": "model",
                    "messages": [{"role": "user", "content": content}]
                }))
                .is_err()
        );
    }
}

#[test]
fn owned_encoding_preserves_malformed_legacy_metadata_extensions() {
    use crate::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
    use crate::transform::{ProtocolAdapter, ProtocolTransform};

    let message = serde_json::json!({
        "role": "user",
        "content": "hello",
        "id": null,
        "status": "legacy-status",
        "provenance": 42,
        "audience": ["legacy-audience"],
        "__open_responses_item_reference": null,
        "vendor_extra": {"value": "preserved"}
    });
    let request = OpenAIChatCompletionsV1
        .decode_request(serde_json::json!({
            "model": "model",
            "messages": [message]
        }))
        .expect("decode legacy metadata");
    let encoded = ProtocolTransform::encode_request_with(&OpenAIChatCompletionsV1, request)
        .expect("encode legacy metadata");
    assert_eq!(
        encoded.body["messages"],
        serde_json::json!([{
            "role": "user",
            "content": "hello",
            "id": null,
            "status": "legacy-status",
            "provenance": 42,
            "audience": ["legacy-audience"],
            "__open_responses_item_reference": null,
            "vendor_extra": {"value": "preserved"}
        }])
    );
}

#[test]
fn owned_encoding_preserves_shared_text_and_visible_block_shapes() {
    let shared = Arc::new(" shared payload \n".to_string());
    let item = |role, content| AiItem {
        role,
        content,
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new(
        "model",
        vec![
            item(Role::User, MessageContent::Text(Arc::clone(&shared))),
            item(Role::Assistant, MessageContent::Text(Arc::clone(&shared))),
            item(
                Role::User,
                MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: Arc::clone(&shared),
                        cache_control: None,
                    },
                    ContentBlock::Text {
                        text: "tail".to_string().into(),
                        cache_control: None,
                    },
                ]),
            ),
            item(
                Role::Assistant,
                MessageContent::Blocks(vec![ContentBlock::Text {
                    text: Arc::clone(&shared),
                    cache_control: None,
                }]),
            ),
        ],
    );
    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");
    assert_eq!(shared.as_str(), " shared payload \n");
    assert_eq!(body["messages"][0]["content"], shared.as_str());
    assert_eq!(body["messages"][1]["content"], shared.as_str());
    assert_eq!(
        body["messages"][2]["content"],
        serde_json::json!([
            {"type": "text", "text": shared.as_str()},
            {"type": "text", "text": "tail"},
        ])
    );
    assert_eq!(body["messages"][3]["content"], shared.as_str());
}

#[test]
fn tool_result_uses_first_block_hint_and_keeps_result_order() {
    let shared = Arc::new("first result".to_string());
    let request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new().into()),
                tool_calls: Some(vec![
                    ToolCall {
                        id: "first".into(),
                        name: "first_tool".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "second".into(),
                        name: "second_tool".into(),
                        arguments: "{}".into(),
                    },
                ]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: String::new().into(),
                        content: Value::String("first result".into()),
                        content_kind: None,
                        is_error: None,
                        cache_control: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "second".into(),
                        content: Value::String("ignored second block".into()),
                        content_kind: None,
                        is_error: None,
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text(Arc::clone(&shared)),
                tool_calls: None,
                tool_call_id: Some("second".into()),
                meta: None,
            },
        ],
    );
    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");
    let results: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["tool_call_id"], "first");
    assert_eq!(results[0]["content"], "first result");
    assert_eq!(results[1]["tool_call_id"], "second");
    assert_eq!(results[1]["content"], shared.as_str());
    assert_eq!(shared.as_str(), "first result");
}

/// 未知响应归属的独立推理项也必须使用有效 Chat 载体，不猜测它与相邻调用
/// 属于同一个模型响应，更不能把签名或密文当作明文。
#[test]
fn standalone_reasoning_item_does_not_become_contentless_assistant_message() {
    let assistant_call = |id: &str, name: &str| AiItem {
        role: Role::Assistant,
        content: MessageContent::Text(String::new().into()),
        tool_calls: Some(vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    let tool_output = |id: &str, text: &str| AiItem {
        role: Role::Tool,
        content: MessageContent::Text(text.to_owned().into()),
        tool_calls: None,
        tool_call_id: Some(id.into()),
        meta: None,
    };
    let request = AiRequest::new(
        "deepseek-flash",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("修复这个问题".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem::reasoning(Vec::new(), vec!["inspect the repo first".into()], None),
            assistant_call("call_00", "bash"),
            assistant_call("call_01", "glob"),
            tool_output("call_01", "scratch listing"),
            tool_output("call_00", "git status output"),
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");
    assert_replayed_reasoning_is_valid(body);

    // Release shape: replay keeps preserved native reasoning as Thinking
    // blocks (see `transform/replay.rs`), which `encode_message` strips from
    // content — the exact message DeepSeek rejected in production.
    let request = AiRequest::new(
        "deepseek-flash",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("修复这个问题".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem::thinking("inspect the repo first", None),
            assistant_call("call_00", "bash"),
            assistant_call("call_01", "glob"),
            tool_output("call_01", "scratch listing"),
            tool_output("call_00", "git status output"),
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");
    assert_replayed_reasoning_is_valid(body);
}

fn assert_replayed_reasoning_is_valid(body: serde_json::Value) {
    let messages = body["messages"].as_array().expect("messages array");

    let mut reasoning_merged = false;
    for message in messages {
        if message["role"] != "assistant" {
            continue;
        }
        let has_content = message.get("content").is_some_and(|c| !c.is_null());
        let has_calls = message
            .get("tool_calls")
            .is_some_and(|c| c.as_array().is_some_and(|a| !a.is_empty()));
        assert!(
            has_content || has_calls,
            "assistant message without content or tool_calls: {message}"
        );
        if message.get("reasoning_content").and_then(|c| c.as_str())
            == Some("inspect the repo first")
        {
            reasoning_merged = true;
        }
    }
    assert!(
        reasoning_merged,
        "standalone reasoning text must survive without inventing a response boundary: {messages:?}"
    );
}

#[test]
fn canonical_system_is_encoded_as_a_system_message() {
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
    request.instructions = Some("hook-system".into());

    let (body, _) = OpenAIEncoder.encode_request(request).unwrap();

    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "hook-system");
    assert_eq!(body["messages"][1]["role"], "user");
}

#[test]
fn internal_artifact_identity_is_not_sent_upstream() {
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: Some(
                stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                    "__stravia_artifact_references": [{
                        "block_index": 0,
                        "artifact_id": "artifact_secret"
                    }],
                    "reasoning_content": "visible reasoning"
                })),
            ),
        }],
    );

    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");
    assert_eq!(
        body["messages"][0]["reasoning_content"],
        "visible reasoning"
    );
    assert!(
        body["messages"][0]
            .get("__stravia_artifact_references")
            .is_none()
    );
    assert!(!body.to_string().contains("artifact_secret"));
}

#[test]
fn gateway_request_state_is_not_passed_through_upstream() {
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
    let ingress = &mut request.meta.vendor.ingress;
    ingress.insert(
        "__stravia_generation_session_id".into(),
        serde_json::json!("session-secret"),
    );
    ingress.insert(
        stravia_runtime_contract::protocol::ir::request::VERIFIED_HISTORY_REPLAY_META.into(),
        serde_json::Value::Bool(true),
    );
    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");
    assert!(!body.to_string().contains("__stravia_"), "{body}");
}

#[test]
fn synthetic_tool_ids_are_distinct_correlated_and_skip_supplied_ids() {
    let request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new().into()),
                tool_calls: Some(vec![
                    ToolCall {
                        id: (String::new()).into(),
                        name: "first".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "tc_1".into(),
                        name: "external".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: (String::new()).into(),
                        name: "second".into(),
                        arguments: "{}".into(),
                    },
                ]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("first result".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("external result".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("second result".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );

    let (first, _) = OpenAIEncoder
        .encode_request(request.clone())
        .expect("encode");
    let (repeated, _) = OpenAIEncoder.encode_request(request).expect("encode");
    let result_ids: Vec<&str> = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["tool_call_id"].as_str().unwrap())
        .collect();

    assert_eq!(result_ids, ["tc_2", "tc_1", "tc_3"]);
    assert_eq!(repeated["messages"], first["messages"]);
}

#[test]
fn duplicate_external_tool_calls_remain_distinct_and_correlated() {
    let request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new().into()),
                tool_calls: Some(vec![
                    ToolCall {
                        id: "external".into(),
                        name: "first".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "external".into(),
                        name: "second".into(),
                        arguments: "{}".into(),
                    },
                ]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("second result".to_owned().into()),
                tool_calls: None,
                tool_call_id: Some("external".into()),
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("first result".to_owned().into()),
                tool_calls: None,
                tool_call_id: Some("external".into()),
                meta: None,
            },
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");
    let result_ids: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["tool_call_id"].as_str().unwrap())
        .collect();

    assert_eq!(result_ids, ["tc_1", "external"]);
}

#[test]
fn responses_reasoning_effort_maps_to_chat_without_loss() {
    use crate::codec::open_responses::decoder::ResponsesDecoder;

    let mut request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "input": "hello",
            "reasoning": { "effort": "xhigh", "summary": "auto" }
        }))
        .unwrap();
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "xhigh".into(),
        },
    );

    let (body, _) = OpenAIEncoder.encode_request(request).unwrap();

    assert_eq!(body["reasoning_effort"], "xhigh");
    assert!(body.get("reasoning").is_none());
}

#[test]
fn generic_toggle_without_provider_adapter_is_omitted() {
    let mut request = AiRequest::new(
        "custom-model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.reasoning.target_control =
        Some(stravia_runtime_contract::thinking::TargetThinkingControl::Enabled);
    request.meta.source_protocol =
        Some(stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let encoded = crate::transform::ProtocolTransform::global()
        .bind(
            stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap()
        .encode_request(request)
        .unwrap();
    assert!(encoded.body.get("reasoning").is_none());
    assert!(encoded.body.get("thinking").is_none());
    assert!(encoded.body.get("enable_thinking").is_none());
    assert_eq!(encoded.body["messages"][0]["content"], "hello");
}

#[test]
fn unknown_reasoning_effort_is_silently_dropped() {
    use crate::codec::openai::compatible::decoder::OpenAIDecoder;

    for value in ["future_7", "not safe"] {
        let request = OpenAIDecoder
            .decode_request(serde_json::json!({
                "model": "gpt",
                "messages": [{"role": "user", "content": "hello"}],
                "reasoning_effort": value
            }))
            .unwrap();
        assert!(request.reasoning.level.is_none());
        assert!(request.reasoning.target_control.is_none());
        let (body, _) = OpenAIEncoder.encode_request(request).unwrap();
        assert!(body.get("reasoning_effort").is_none());
    }
}

#[test]
fn thinking_and_reasoning_plaintext_joins_into_reasoning_content() {
    let request = AiRequest::new(
        "deepseek-flash",
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
                content: MessageContent::Blocks(vec![
                    ContentBlock::Thinking {
                        thinking: "first thought".into(),
                        signature: Some("opaque-sig".into()),
                    },
                    ContentBlock::Reasoning {
                        summary: vec!["summary".into()],
                        content: vec!["detail".into()],
                        encrypted_content: Some("cipher-text".into()),
                    },
                    ContentBlock::RedactedThinking {
                        data: "redacted-payload".into(),
                    },
                    ContentBlock::Text {
                        text: "answer".to_owned().into(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");

    let assistant = &body["messages"][1];
    // 明文推理按块顺序进入 reasoning_content；受保护载荷没有任何载体。
    assert_eq!(
        assistant["reasoning_content"],
        "first thought\nsummary\ndetail"
    );
    let wire = body.to_string();
    assert!(!wire.contains("opaque-sig"), "{wire}");
    assert!(!wire.contains("cipher-text"), "{wire}");
    assert!(!wire.contains("redacted-payload"), "{wire}");
    assert!(!wire.contains("reasoning\""), "{wire}");
}

#[test]
fn protected_only_assistant_item_is_dropped_entirely() {
    for blocks in [
        vec![ContentBlock::RedactedThinking {
            data: "redacted-payload".into(),
        }],
        vec![ContentBlock::Reasoning {
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: Some("cipher-text".into()),
        }],
        vec![ContentBlock::Thinking {
            thinking: String::new(),
            signature: Some("opaque-sig".into()),
        }],
    ] {
        let request = AiRequest::new(
            "deepseek-flash",
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
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("continue".to_owned().into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );

        let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");

        // 条目只剩承载不了的受保护载荷：整条跳过，不能发出空 assistant 消息。
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages.iter().all(|m| m["role"] == "user"));
        let wire = body.to_string();
        assert!(!wire.contains("redacted-payload"), "{wire}");
        assert!(!wire.contains("cipher-text"), "{wire}");
        assert!(!wire.contains("opaque-sig"), "{wire}");
    }
}

#[test]
fn signed_thinking_with_tool_calls_keeps_reasoning_content_and_drops_signature() {
    let request = AiRequest::new(
        "deepseek-flash",
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
                content: MessageContent::Blocks(vec![ContentBlock::Thinking {
                    thinking: "inspect the repo".into(),
                    signature: Some("opaque-sig".into()),
                }]),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "glob".into(),
                    arguments: "{}".into(),
                }]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("listing".to_owned().into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                meta: None,
            },
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(request).expect("encode");

    let assistant = &body["messages"][1];
    assert_eq!(assistant["reasoning_content"], "inspect the repo");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    // 有 tool_calls 时 content 可缺省，但绝不能把签名写进任何字段。
    assert!(!body.to_string().contains("opaque-sig"));
}

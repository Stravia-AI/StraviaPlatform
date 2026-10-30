use super::*;

#[test]
fn unified_controls_project_and_unsupported_vendor_controls_are_dropped() {
    let request = ResponsesDecoder.decode_request(serde_json::json!({
        "model": "gpt", "input": "hello", "generate": false,
        "access_programs": {"cyber": "standard", "private": true},
        "reasoning": {"context": "current_turn", "private": true},
        "stream_options": {"reasoning_summary_delivery": "delta", "private": true},
        "prompt_cache_options": {"mode": "explicit", "ttl": "30m", "prewarm": false, "private": true},
        "context_management": [{"type": "compaction", "compact_threshold": 10000, "private": true}],
        "metadata": {"private": {"business": true}},
        "unknown_body": {"private": true}
    })).unwrap();
    assert!(crate::codec::compaction::native_compaction_requested(
        &request
    ));
    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert_eq!(body["generate"], false);
    for field in ["access_programs", "stream_options", "prompt_cache_options"] {
        assert!(body.get(field).is_none(), "{field} leaked");
    }
    assert!(body["reasoning"].get("context").is_none());
    assert_eq!(
        body["context_management"],
        serde_json::json!([{"type": "compaction", "compact_threshold": 10000}])
    );
    assert_eq!(body["metadata"], serde_json::json!({}));
    assert!(body.get("unknown_body").is_none());
}

#[test]
fn compaction_requires_an_official_control_or_explicit_trigger() {
    for control in [
        Value::Null,
        serde_json::json!([]),
        serde_json::json!([{"type": "future", "private": true}]),
    ] {
        let request = ResponsesDecoder
            .decode_request(serde_json::json!({
                "model": "gpt", "input": "hello", "context_management": control
            }))
            .unwrap();
        assert!(!crate::codec::compaction::native_compaction_requested(
            &request
        ));
    }
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt", "input": [{"type": "compaction_trigger"}], "context_management": null
        }))
        .unwrap();
    assert!(crate::codec::compaction::native_compaction_requested(
        &request
    ));
}

use crate::codec::anthropic::messages::decoder::AnthropicDecoder;
use crate::codec::open_responses::decoder::ResponsesDecoder;
use crate::codec::openai::compatible::decoder::OpenAIDecoder;
use stravia_runtime_contract::protocol::ir::AiItem;

#[test]
fn responses_include_is_derived_only_from_unified_intent() {
    for (store, effort, logprobs, expected) in [
        (
            false,
            "high",
            Some(0),
            serde_json::json!([
                "reasoning.encrypted_content",
                "message.output_text.logprobs"
            ]),
        ),
        (
            true,
            "high",
            Some(2),
            serde_json::json!(["message.output_text.logprobs"]),
        ),
        (false, "none", None, Value::Null),
    ] {
        let request = ResponsesDecoder
            .decode_request(serde_json::json!({
                "model": "gpt", "input": "hello", "store": store,
                "reasoning": {"effort": effort}, "top_logprobs": logprobs,
                "include": ["file_search_call.results", "reasoning.encrypted_content"],
            }))
            .unwrap();
        let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
        assert_eq!(body["include"], expected);
        if expected.is_null() {
            assert!(body.get("include").is_none());
        }
    }
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt", "input": "hello", "store": false,
            "include": ["reasoning.encrypted_content"],
        }))
        .unwrap();
    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert!(body.get("include").is_none());
}

#[test]
fn native_tools_and_choices_use_current_official_types() {
    use stravia_runtime_contract::protocol::ir::{NativeToolKind, ToolChoice};
    for (wire_type, kind, official_type) in [
        (
            "web_search_preview",
            NativeToolKind::WebSearch,
            "web_search",
        ),
        (
            "code_interpreter",
            NativeToolKind::CodeExecution,
            "code_interpreter",
        ),
        (
            "image_generation",
            NativeToolKind::ImageGeneration,
            "image_generation",
        ),
        ("tool_search", NativeToolKind::ToolSearch, "tool_search"),
    ] {
        let request = ResponsesDecoder
            .decode_request(serde_json::json!({
                "model": "gpt", "input": "hello",
                "tools": [{"type": wire_type}, {"type": "file_search"}, {"type": "mcp"}],
                "tool_choice": {"type": wire_type},
            }))
            .unwrap();
        assert!(matches!(request.tool_choice, Some(ToolChoice::Native(actual)) if actual == kind));
        assert_eq!(request.parameters.native_tools.len(), 1);
        let profile = response_profile_from_request(&request);
        let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
        for output in [&profile, &body] {
            assert_eq!(output["tools"].as_array().unwrap().len(), 1);
            assert_eq!(output["tools"][0]["type"], official_type);
            assert_eq!(output["tool_choice"]["type"], official_type);
        }
    }
}

#[test]
fn custom_and_allowed_choices_remain_typed_without_tool_definition_passthrough() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt", "input": "hello",
            "tools": [{"type": "custom", "name": "patch", "format": {
                "type": "grammar", "syntax": "lark", "definition": "start: \"ok\""
            }, "private": true}],
            "tool_choice": {"type": "allowed_tools", "mode": "required",
                "tools": [{"type": "custom", "name": "patch"}]},
        }))
        .unwrap();
    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert_eq!(body["tools"][0]["type"], "custom");
    assert_eq!(body["tools"][0]["format"]["definition"], "start: \"ok\"");
    assert!(body["tools"][0].get("private").is_none());
    assert_eq!(
        body["tool_choice"],
        serde_json::json!({
            "type": "allowed_tools", "mode": "required", "tools": [{"type": "custom", "name": "patch"}]
        })
    );
}

#[test]
fn effective_profile_round_trip_retains_unified_response_controls() {
    use stravia_runtime_contract::protocol::ir::{
        CompactionControl, ReasoningSummary, ServiceTier,
    };
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt", "input": "hello", "store": false,
            "user": "legacy-user", "safety_identifier": "safe-user",
            "metadata": {"tenant": "acme", "discard": 42},
            "reasoning": {"effort": "high", "summary": "concise"},
            "service_tier": "priority",
            "context_management": [{"type": "compaction", "compact_threshold": 12000}],
        }))
        .unwrap();
    assert_eq!(request.parameters.end_user_id.as_deref(), Some("safe-user"));
    assert_eq!(
        request.parameters.compaction,
        Some(CompactionControl {
            threshold_tokens: Some(12000)
        })
    );
    let profile = effective_response_profile_from_request(&request);
    let decoded = super::super::decoder::decode_effective_response_profile(
        "gpt",
        profile.as_object().unwrap(),
    )
    .unwrap();
    assert_eq!(decoded.reasoning.summary, Some(ReasoningSummary::Concise));
    assert_eq!(decoded.parameters.service_tier, Some(ServiceTier::Priority));
    assert_eq!(decoded.parameters.compaction, request.parameters.compaction);
    let (body, _) = ResponsesEncoder.encode_request(decoded).unwrap();
    assert_eq!(body["safety_identifier"], "safe-user");
    assert!(body.get("user").is_none());
    assert_eq!(body["metadata"], serde_json::json!({"tenant": "acme"}));
    assert_eq!(
        body["context_management"],
        serde_json::json!([{"type": "compaction", "compact_threshold": 12000}])
    );
}

#[test]
fn canonical_controls_win_in_encoder_and_borrowed_profile() {
    use stravia_runtime_contract::protocol::ir::{
        CacheRetention, Continuation, NativeTool, ServiceTier, Verbosity, WebSearchTool,
    };
    let mut request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt", "input": "hello"
        }))
        .unwrap();
    request.parameters.store = Some(false);
    request.parameters.background = Some(false);
    request.parameters.max_tool_calls = Some(0);
    request.parameters.logprobs = Some(0);
    request.parameters.metadata = Some(Default::default());
    request.parameters.verbosity = Some(Verbosity::Low);
    request.parameters.cache.key = Some(String::new());
    request.parameters.cache.retention = Some(CacheRetention::Extended);
    request.parameters.continuation = Some(Continuation::Conversation("conv_canonical".into()));
    request.parameters.service_tier = Some(ServiceTier::Priority);
    request
        .parameters
        .native_tools
        .push(NativeTool::WebSearch(WebSearchTool::default()));
    let schema =
        serde_json::json!({"type": "object", "properties": {"optional": {"type": "string"}}});
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "canonical".into(),
        description: Some(String::new()),
        schema: schema.clone(),
        strict: Some(false),
    });
    let profile = response_profile_from_request(&request);
    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    for output in [&profile, &body] {
        assert_eq!(output["store"], false);
        assert_eq!(output["background"], false);
        assert_eq!(output["max_tool_calls"], 0);
        assert_eq!(output["top_logprobs"], 0);
        assert_eq!(output["metadata"], serde_json::json!({}));
        assert_eq!(output["prompt_cache_key"], "");
        assert_eq!(output["prompt_cache_retention"], "24h");
        assert!(output.get("prompt_cache_options").is_none());
        assert_eq!(output["conversation"], "conv_canonical");
        assert_eq!(output["service_tier"], "priority");
        assert_eq!(output["text"]["verbosity"], "low");
        assert_eq!(output["text"]["format"]["description"], "");
        assert_eq!(output["text"]["format"]["schema"], schema);
        assert_eq!(output["text"]["format"]["strict"], false);
        assert_eq!(output["tools"][0]["type"], "web_search");
    }
    assert_eq!(
        body["include"],
        serde_json::json!(["message.output_text.logprobs"])
    );
    assert!(body.get("stream_options").is_none());
    assert!(profile.get("include").is_none());
    assert!(profile.get("stream_options").is_none());
}

#[test]
fn cross_projection_uses_typed_controls_without_foreign_options() {
    let request = OpenAIDecoder
        .decode_request(serde_json::json!({
            "model": "gpt", "messages": [{"role": "user", "content": "hello"}],
            "store": false, "logprobs": true, "top_logprobs": 0, "verbosity": "low",
            "prompt_cache_key": "", "stream_options": {"include_obfuscation": false},
            "service_tier": "priority", "parallel_tool_calls": false,
            "reasoning_effort": "high", "web_search_options": {}
        }))
        .unwrap();
    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["top_logprobs"], 0);
    assert_eq!(body["text"]["verbosity"], "low");
    assert_eq!(body["prompt_cache_key"], "");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(body.get("stream_options").is_none());
    assert!(body.get("context_management").is_none());
    assert_eq!(body["service_tier"], "priority");
    assert_eq!(body["tools"][0]["type"], "web_search");
    for field in [
        "unknown",
        "temperature",
        "web_search_options",
        "client_metadata",
    ] {
        assert!(body.get(field).is_none(), "{field} leaked");
    }
}

#[test]
fn explicit_null_resets_never_leave_as_null_controls() {
    let mut request = ResponsesDecoder.decode_request(serde_json::json!({
        "model": "gpt", "input": "hello", "store": null, "top_logprobs": null,
        "background": null, "stream_options": null, "safety_identifier": null,
        "text": {"verbosity": null, "format": {
            "type": "json_schema", "name": "response", "schema": {"type": "object"}, "strict": null
        }},
        "reasoning": {"effort": null, "summary": null}
    })).unwrap();
    let (body, _) = ResponsesEncoder.encode_request(request.clone()).unwrap();
    for field in [
        "top_logprobs",
        "background",
        "stream_options",
        "safety_identifier",
    ] {
        assert!(body.get(field).is_none(), "{field}");
    }
    assert_eq!(body["store"], true);
    assert!(body["text"]["format"].get("strict").is_none());
    assert!(body["text"].get("verbosity").is_none());
    assert!(body.get("reasoning").is_none());
    request.parameters.store = Some(false);
    request.parameters.logprobs = Some(0);
    request.parameters.verbosity = Some(stravia_runtime_contract::protocol::ir::Verbosity::Low);
    request.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::Low);
    let profile = response_profile_from_request(&request);
    assert_eq!(profile["store"], false);
    assert_eq!(profile["top_logprobs"], 0);
    assert_eq!(profile["text"]["verbosity"], "low");
    assert_eq!(profile["reasoning"]["effort"], "low");
}

#[test]
fn canonical_presence_and_reasoning_mode_project_without_native_maps() {
    let mut request = AiRequest::new("gpt", vec![AiItem::output_text("hello")]);
    request
        .parameters
        .reset(stravia_runtime_contract::protocol::ir::ParameterReset::Instructions);
    request
        .parameters
        .reset(stravia_runtime_contract::protocol::ir::ParameterReset::NativeTools);
    request.reasoning.mode = Some("disabled".into());
    let profile = effective_response_profile_from_request(&request);
    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    for output in [&profile, &body] {
        assert!(output.get("instructions").is_none());
        assert_eq!(output["reasoning"], serde_json::json!({"effort": "none"}));
    }
    assert_eq!(profile["tools"], serde_json::json!([]));
    assert!(body.get("tools").is_none());
}

#[test]
fn consumed_text_requests_preserve_shared_snapshots_and_wire_roles() {
    let text = Arc::new("Unicode 正文\n\"quoted\"\\tail".to_owned());
    for (role, text_type) in [(Role::User, "input_text"), (Role::Assistant, "output_text")] {
        for content in [
            MessageContent::Text(text.clone()),
            MessageContent::Blocks(vec![ContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            }]),
        ] {
            let request = AiRequest::new(
                "gpt",
                vec![AiItem {
                    role,
                    content,
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                }],
            );
            let snapshot = request.clone();
            let (mut body, _) = ResponsesEncoder.encode_request(request).unwrap();
            assert_eq!(body["input"][0]["content"][0]["type"], text_type);
            assert_eq!(body["input"][0]["content"][0]["text"], text.as_str());
            body["input"][0]["content"][0]["text"] = Value::String("changed".into());
            match &snapshot.items[0].content {
                MessageContent::Text(value) => assert_eq!(value.as_str(), text.as_str()),
                MessageContent::Blocks(blocks) => {
                    let ContentBlock::Text { text: value, .. } = &blocks[0] else {
                        panic!("snapshot must retain its text block");
                    };
                    assert_eq!(value.as_str(), text.as_str());
                }
            }
        }
    }
}

#[test]
fn responses_summary_respects_explicit_intent_and_disabled_target() {
    use stravia_runtime_contract::protocol::ir::ReasoningSummary;
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
    for (summary, level, control, expected) in [
        (Some(ReasoningSummary::None), None, None, None),
        (Some(ReasoningSummary::Concise), None, None, Some("concise")),
        (
            Some(ReasoningSummary::Detailed),
            None,
            None,
            Some("detailed"),
        ),
        (
            None,
            Some(ThinkingLevel::Off),
            Some(TargetThinkingControl::Effort {
                value: "high".into(),
            }),
            None,
        ),
        (None, None, Some(TargetThinkingControl::Disabled), None),
    ] {
        let mut request = ResponsesDecoder
            .decode_request(serde_json::json!({"model": "gpt", "input": "hello"}))
            .unwrap();
        request.reasoning.summary = summary;
        request.reasoning.level = level;
        request.reasoning.target_control = control;
        let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
        assert_eq!(
            body["reasoning"].get("summary").and_then(Value::as_str),
            expected
        );
        if level == Some(ThinkingLevel::Off) {
            assert_eq!(body["reasoning"]["effort"], "high");
        }
    }
}

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

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn chat_text_precedes_tool_calls_and_their_results() {
    for content in [
        serde_json::json!("inspect repository"),
        serde_json::json!([{"type": "text", "text": "inspect repository"}]),
    ] {
        let request = OpenAIDecoder
            .decode_request(serde_json::json!({
                "model": "gpt",
                "messages": [
                    {
                        "role": "assistant",
                        "content": content,
                        "tool_calls": [
                            {"id": "call_read", "type": "function", "function": {
                                "name": "read", "arguments": "{\"path\":\"src/main.rs\"}"
                            }},
                            {"id": "call_glob", "type": "function", "function": {
                                "name": "glob", "arguments": "{}"
                            }}
                        ]
                    },
                    {"role": "tool", "tool_call_id": "call_read", "content": "source"},
                    {"role": "tool", "tool_call_id": "call_glob", "content": "paths"}
                ]
            }))
            .expect("decode Chat text and tool history");

        let (body, _) = ResponsesEncoder
            .encode_request(request)
            .expect("encode ordered Responses history");

        assert_eq!(
            body["input"],
            serde_json::json!([
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "inspect repository"}
                ]},
                {"type": "function_call", "call_id": "call_read", "name": "read",
                    "arguments": "{\"path\":\"src/main.rs\"}"},
                {"type": "function_call", "call_id": "call_glob", "name": "glob",
                    "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_read", "output": "source"},
                {"type": "function_call_output", "call_id": "call_glob", "output": "paths"}
            ])
        );
    }
}

#[test]
fn assistant_tool_blocks_keep_text_between_calls() {
    let request = AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "claude", "max_tokens": 128,
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_read", "name": "read", "input": {}},
                    {"type": "text", "text": "inspect the remaining paths"},
                    {"type": "tool_use", "id": "call_glob", "name": "glob", "input": {}}
                ]
            }]
        }))
        .expect("decode ordered assistant blocks");

    let (body, _) = ResponsesEncoder
        .encode_request(request)
        .expect("encode ordered assistant blocks");

    assert_eq!(
        body["input"],
        serde_json::json!([
            {"type": "function_call", "call_id": "call_read", "name": "read", "arguments": "{}"},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "inspect the remaining paths"}
            ]},
            {"type": "function_call", "call_id": "call_glob", "name": "glob", "arguments": "{}"}
        ])
    );
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
        .encode_request(request)
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
    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert_eq!(
        body["input"],
        serde_json::json!([{
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "visible thinking"}]
        }])
    );
}

#[test]
fn native_responses_requests_summary_without_explicit_effort() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "input": "hello"
        }))
        .unwrap();

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

    assert_eq!(body["reasoning"]["summary"], "auto");
    assert!(body["reasoning"].get("effort").is_none());
}

#[test]
fn native_responses_defaults_omitted_reasoning_summary() {
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

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["reasoning"]["summary"], "auto");
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

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

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

        let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

        assert_eq!(
            body["reasoning"].get("summary").and_then(Value::as_str),
            expected_summary
        );
    }
}

#[test]
fn anthropic_without_thinking_requests_summary_without_selecting_effort() {
    let request = AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

    assert_eq!(body["reasoning"]["summary"], "auto");
    assert!(body["reasoning"].get("effort").is_none());
}

#[test]
fn writes_max_target_effort_without_a_local_allow_list() {
    let mut request = AiRequest::new("gpt", vec![AiItem::output_text("hello")]);
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "max".into(),
        },
    );

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

    assert_eq!(body["reasoning"]["effort"], "max");
}

#[test]
fn preserves_request_instructions_and_developer_role_with_dated_defaults() {
    let mut request = AiRequest::new(
        "logical-model",
        vec![
            AiItem {
                role: Role::Developer,
                content: MessageContent::Text("Use repository conventions.".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::User,
                content: MessageContent::Text("Implement it.".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );
    request.instructions = Some("Follow the accepted design.".into());

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

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
            content: MessageContent::Text("Persist upstream.".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.parameters.store = Some(true);

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert_eq!(body["store"], true);
}

#[test]
fn forwards_canonical_metadata_and_safety_identifier() {
    let mut request = AiRequest::new(
        "logical-model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.parameters.metadata = Some([("tenant".into(), "acme".into())].into());
    request.parameters.end_user_id = Some("safe-user-1".into());

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();

    assert_eq!(body["metadata"], serde_json::json!({"tenant": "acme"}));
    assert_eq!(body["safety_identifier"], "safe-user-1");
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
        .encode_request(request)
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

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
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
    request.parameters.continuation = Some(
        stravia_runtime_contract::protocol::ir::Continuation::PreviousResponse(
            "resp_parent".into(),
        ),
    );

    let (body, _) = ResponsesEncoder
        .encode_request(request)
        .expect("encode continuation");

    assert_eq!(body["previous_response_id"], "resp_parent");
    assert!(body.get("input").is_none());
}

#[test]
fn omits_responses_unsupported_media() {
    let request = AiRequest::new(
        "gpt",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: Arc::new("keep".into()),
                    cache_control: None,
                },
                ContentBlock::Audio {
                    source: MediaSource::Base64 {
                        media_type: "audio/wav".into(),
                        data: "aGk=".into(),
                    },
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert_eq!(
        body["input"][0]["content"],
        serde_json::json!([
            {"type": "input_text", "text": "keep"}
        ])
    );
}

#[test]
fn encodes_canonical_json_schema_for_dated_targets() {
    let mut input = AiItem::output_text("return structured data");
    input.role = Role::User;
    let mut request = AiRequest::new("gpt", vec![input]);
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        description: None,
        schema: serde_json::json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        }),
        strict: Some(true),
    });

    let (body, _) = ResponsesEncoder
        .encode_request(request)
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
fn omits_canonical_json_object_for_dated_targets() {
    let mut input = AiItem::output_text("return JSON");
    input.role = Role::User;
    let mut request = AiRequest::new("gpt", vec![input]);
    request.response_format = Some(ResponseFormat::JsonObject);

    let (body, _) = ResponsesEncoder.encode_request(request).unwrap();
    assert!(body.get("text").is_none());
}

#[test]
fn business_json_tool_payloads_are_preserved_as_json_text() {
    for payload in [
        serde_json::json!({"temperature": 21}),
        serde_json::json!([
            {"type": "tool_result", "content": {"temperature": 21}}
        ]),
    ] {
        let request = AiRequest::new(
            "gpt",
            vec![
                AiItem::output_text("keep"),
                AiItem {
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
                },
            ],
        );

        let (body, _) = ResponsesEncoder
            .encode_request(request)
            .expect("encode structured tool result");
        let output = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["call_id"], "call_1");
        assert_eq!(
            serde_json::from_str::<Value>(output["output"].as_str().unwrap()).unwrap(),
            payload
        );
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
        .encode_request(request)
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
        .encode_request(request)
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
                text: "before".to_owned().into(),
                cache_control: None,
            },
            ContentBlock::Reasoning {
                summary: vec!["summary".into()],
                content: vec!["detail".into()],
                encrypted_content: Some("opaque".into()),
            },
            ContentBlock::Text {
                text: "after".to_owned().into(),
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
        .encode_request(request)
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
                text: "before".to_owned().into(),
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
        .encode_request(request)
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
                content: MessageContent::Text("hello".to_owned().into()),
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

    let (body, _) = ResponsesEncoder.encode_request(request).expect("encode");

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
                content: MessageContent::Text("hello".to_owned().into()),
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

    let (body, _) = ResponsesEncoder.encode_request(request).expect("encode");

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
            .encode_request(AiRequest::new("gpt", vec![item]))
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
                text: "answer".to_owned().into(),
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
            AiItem::function_call_output("call_1", serde_json::json!("rows: 2"))
                .with_graph_metadata(
                    Some("fco_gateway_1".into()),
                    Some(AiItemStatus::Completed),
                    AiItemProvenance::Client,
                    AiItemAudience::Provider,
                ),
        ],
    );

    let (body, _) = ResponsesEncoder
        .encode_request(request)
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
    assert_eq!(input[2]["output"], "rows: 2");
}

#[test]
fn mixed_assistant_media_omission_preserves_text_and_calls() {
    for with_thinking in [false, true] {
        let mut blocks = vec![
            ContentBlock::Text {
                text: Arc::new("answer".into()),
                cache_control: None,
            },
            ContentBlock::Image {
                source: MediaSource::Url("https://example.test/image.png".into()),
                detail: None,
                cache_control: None,
            },
            ContentBlock::File {
                source: MediaSource::Url("https://example.test/file.pdf".into()),
                media_type: None,
            },
        ];
        if with_thinking {
            blocks.insert(
                0,
                ContentBlock::Thinking {
                    thinking: "consider".into(),
                    signature: None,
                },
            );
        }
        let mut item = AiItem::output_text("unused");
        item.content = MessageContent::Blocks(blocks);
        item.tool_calls = Some(vec![stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        }]);
        let (body, _) = ResponsesEncoder
            .encode_request(AiRequest::new("gpt", vec![item]))
            .unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        let message = &input[0];
        assert_eq!(message["type"], "message");
        let parts = message["content"].as_array().unwrap();
        assert_eq!(parts.len(), if with_thinking { 2 } else { 1 });
        assert!(parts.iter().all(|part| part["type"] == "output_text"));
        assert_eq!(parts.last().unwrap()["text"], "answer");
    }
}

#[test]
fn nested_tool_outputs_filter_mixed_parts_and_omit_unsupported_only_payloads() {
    let supported = serde_json::json!([
        {"type": "input_text", "text": "keep"},
        {"type": "input_image", "image_url": "https://example.test/image.png", "detail": "auto"},
        {"type": "input_file", "file_url": "https://example.test/file.pdf"}
    ]);
    let mut mixed = supported.as_array().unwrap().clone();
    mixed.insert(
        1,
        serde_json::json!({"type": "input_audio", "data": "aGk="}),
    );
    mixed.push(serde_json::json!({"temperature": 21}));
    let result = |payload| {
        MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: payload,
            content_kind: Some(
                stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
            ),
            is_error: None,
            cache_control: None,
        }])
    };
    let content = result(Value::Array(mixed));
    assert!(tool_output_representable(&content));
    assert_eq!(encode_tool_output(content.clone()), Some(supported.clone()));
    assert_eq!(encode_response_tool_output(&content), Some(supported));
    for payload in [
        serde_json::json!({"temperature": 21}),
        serde_json::json!([
            {"type": "input_audio", "data": "aGk="}
        ]),
        serde_json::json!([]),
        Value::Null,
    ] {
        let content = result(payload);
        assert!(!tool_output_representable(&content));
        assert_eq!(encode_tool_output(content.clone()), None);
        assert_eq!(encode_response_tool_output(&content), None);
    }
    let mixed_blocks = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: Arc::new("first".into()),
            cache_control: None,
        },
        ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: serde_json::json!([{"type":"input_audio","data":"aGk="}]),
            content_kind: Some(
                stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
            ),
            is_error: None,
            cache_control: None,
        },
        ContentBlock::Audio {
            source: MediaSource::Url("https://example.test/audio.wav".into()),
        },
        ContentBlock::ServerToolResult {
            tool_use_id: "call_1".into(),
            content: Value::String("last".into()),
            content_kind: None,
            server_type: None,
            cache_control: None,
        },
    ]);
    let expected = serde_json::json!([
        {"type": "input_text", "text": "first"},
        {"type": "input_text", "text": "last"}
    ]);
    assert_eq!(
        encode_tool_output(mixed_blocks.clone()),
        Some(expected.clone())
    );
    assert_eq!(encode_response_tool_output(&mixed_blocks), Some(expected));
    let content = result(Value::String("".into()));
    assert_eq!(
        encode_tool_output(content.clone()),
        Some(Value::String("".into()))
    );
    assert_eq!(
        encode_response_tool_output(&content),
        Some(Value::String("".into()))
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
        .encode_request(AiRequest::new("gpt", vec![item]))
        .expect("encode plain assistant message");

    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["role"], "assistant");
    assert_eq!(body["input"][0]["id"], "msg_1");
    assert_eq!(body["input"][0]["status"], "completed");
    assert_eq!(body["input"][0]["phase"], "final_answer");
}

use super::*;

#[test]
fn unsupported_official_controls_are_dropped_and_cache_is_unified() {
    let request = AnthropicDecoder.decode_request(request_with(serde_json::json!({
        "speed": "fast",
        "output_config": {"task_budget": {"type": "tokens", "total": 0, "remaining": 0, "private": true}},
        "cache_control": {"type": "ephemeral", "ttl": "1h", "evict_on_complete": false, "private": true},
        "thread": {"type": "continue", "previous_message_id": "msg", "private": true},
        "diagnostics": {"previous_message_id": null, "private": true},
        "safeguards": [{"type": "policy", "classifier_context": "context", "private": true}],
        "context_hint": {"enabled": false, "target_tokens_saved": 0, "private": true},
        "fallbacks": [{"model": "claude", "private": true}],
        "thinking": {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "error", "private": true}},
        "unknown_body": true
    }))).unwrap();
    let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .unwrap();
    assert_eq!(
        body["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"})
    );
    for key in [
        "speed",
        "thread",
        "diagnostics",
        "safeguards",
        "context_hint",
        "fallbacks",
    ] {
        assert!(body.get(key).is_none(), "{key}");
    }
    assert!(body["output_config"].get("task_budget").is_none());
    assert!(body["thinking"].get("block_binding").is_none());
    assert!(body.get("unknown_body").is_none());
}

fn request_with(extra: Value) -> Value {
    let mut request = serde_json::json!({
        "model": "model",
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 1024
    });
    request
        .as_object_mut()
        .expect("request object")
        .extend(extra.as_object().expect("extra object").clone());
    request
}

#[test]
fn malformed_structured_output_and_controls_remain_decode_errors() {
    for extra in [
        serde_json::json!({"output_config": {"format": {"type": "json_schema", "schema": null}}}),
        serde_json::json!({"output_config": {"format": {"type": "json_schema", "schema": []}}}),
        serde_json::json!({"output_config": {"format": {"type": "json_schema"}}}),
        serde_json::json!({"output_config": {"format": {"type": "unknown", "schema": {}}}}),
        serde_json::json!({"tool_choice": {"type": "auto", "disable_parallel_tool_use": "false"}}),
        serde_json::json!({"stop_sequences": [1]}),
        serde_json::json!({"tools": [{"name": "read", "input_schema": {}, "strict": "true"}]}),
    ] {
        assert!(
            AnthropicDecoder
                .decode_request(request_with(extra.clone()))
                .is_err(),
            "malformed source controls must fail: {extra}"
        );
    }
}

#[test]
fn adaptive_thinking_uses_output_effort() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "medium"}
        })))
        .expect("adaptive thinking");

    assert!(request.reasoning.is_enabled());
    assert_eq!(
        request.reasoning.level,
        Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium)
    );
}

#[test]
fn output_effort_without_thinking_is_preserved() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "output_config": {"effort": "high"}
        })))
        .expect("output effort");

    assert!(request.reasoning.is_enabled());
    assert_eq!(request.reasoning.mode, None);
    assert_eq!(
        request.reasoning.level,
        Some(stravia_runtime_contract::thinking::ThinkingLevel::High)
    );
}

#[test]
fn explicit_effort_keeps_source_mode_and_enabled_budget() {
    for thinking in [
        serde_json::json!({"type": "enabled", "budget_tokens": 1024, "display": "omitted"}),
        serde_json::json!({"type": "disabled", "display": "omitted"}),
    ] {
        let request = AnthropicDecoder
            .decode_request(request_with(serde_json::json!({
                "thinking": thinking.clone(),
                "output_config": {"effort": "max"}
            })))
            .unwrap();
        assert_eq!(
            request.reasoning.level,
            Some(stravia_runtime_contract::thinking::ThinkingLevel::Max)
        );
        assert_eq!(request.reasoning.mode.as_deref(), thinking["type"].as_str());
        assert_eq!(
            request.reasoning.is_enabled(),
            thinking["type"] != "disabled"
        );
        assert_eq!(
            request.reasoning.budget_tokens,
            thinking["budget_tokens"].as_u64().map(|v| v as u32)
        );
        assert_eq!(request.reasoning.summary, Some(ReasoningSummary::None));
    }
}

#[test]
fn explicit_effort_does_not_accept_unknown_thinking_type() {
    assert!(
        AnthropicDecoder
            .decode_request(request_with(serde_json::json!({
                "thinking": {"type": "unknown"},
                "output_config": {"effort": "high"}
            })))
            .is_err()
    );
}

#[test]
fn same_protocol_round_trip_keeps_tool_strict_and_tool_result_is_error() {
    let body = serde_json::json!({
        "model": "model",
        "max_tokens": 1024,
        "tools": [
            {"name": "read", "description": "Read", "input_schema": {"type": "object"}, "strict": true},
            {"name": "grep", "description": "Grep", "input_schema": {"type": "object"}, "strict": false},
            {"name": "list", "input_schema": {"type": "object"}}
        ],
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok", "is_error": false}
            ]}
        ]
    });

    let request = AnthropicDecoder.decode_request(body).expect("decode");
    let tools = request.tools.as_ref().expect("tools");
    assert_eq!(tools[0].strict, Some(true));
    assert_eq!(tools[1].strict, Some(false));
    assert_eq!(tools[2].strict, None);
    let (encoded, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .expect("encode");

    assert_eq!(encoded["tools"][0]["strict"], true);
    assert_eq!(encoded["tools"][1]["strict"], false);
    assert!(encoded["tools"][2].get("strict").is_none());
    assert_eq!(encoded["messages"][2]["content"][0]["is_error"], false);
}

#[test]
fn native_controls_decode_directly_into_canonical_parameters() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "top_k": 0,
            "context_management": null,
            "container": {"id": "", "skills": []},
            "inference_geo": "",
            "metadata": {"user_id": "", "unknown": "not-forwarded"},
            "service_tier": "auto",
            "stop_sequences": [],
            "system": "",
            "tools": [],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": false}
        })))
        .unwrap();
    assert_eq!(request.parameters.top_k, Some(0));
    assert!(request.parameters.compaction.is_none());
    assert!(request.parameters.native_tools.is_empty());
    assert!(request.parameters.end_user_id.is_none());
    assert_eq!(request.parameters.service_tier, Some(ServiceTier::Auto));
    assert_eq!(request.generation.stop, Some(vec![]));
    assert_eq!(request.parallel_tool_calls, Some(true));
    assert!(request.parameters.resets(ParameterReset::Instructions));
    assert!(request.parameters.resets(ParameterReset::NativeTools));
    assert!(request.parameters.resets(ParameterReset::EndUser));

    assert!(request.meta.vendor.ingress.is_empty());
    let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .unwrap();
    assert!(body.get("context_management").is_none());
    assert!(body.get("container").is_none());
    assert!(body.get("inference_geo").is_none());
    assert!(body.get("system").is_none());
    assert!(body.get("tools").is_none());
    assert!(body.get("metadata").is_none());
}

#[test]
fn native_search_is_typed_and_function_vendor_controls_are_dropped() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "tools": [
                {"name": "read", "input_schema": {"type": "object"},
                 "strict": false, "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"name": "web_search", "type": "web_search_20250305", "max_uses": 0,
                 "allowed_domains": [], "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]
        })))
        .unwrap();
    assert_eq!(request.tools.as_ref().unwrap().len(), 1);
    assert_eq!(request.parameters.native_tools.len(), 1);
    let NativeTool::WebSearch(search) = &request.parameters.native_tools[0] else {
        panic!("expected unified web search");
    };
    assert_eq!(search.max_uses, Some(0));
    assert!(
        !request
            .meta
            .vendor
            .ingress
            .contains_key("__anthropic_raw_tools")
    );
    let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .unwrap();
    assert!(body["tools"][0].get("cache_control").is_none());
    assert_eq!(body["tools"][0]["strict"], false);
    assert_eq!(body["tools"][1]["type"], "web_search_20250305");
    assert_eq!(body["tools"][1]["max_uses"], 0);
    assert!(body["tools"][1].get("cache_control").is_none());
}

#[test]
fn unified_reasoning_cache_container_and_compaction_roundtrip() {
    for (display, summary) in [
        ("summarized", ReasoningSummary::Auto),
        ("omitted", ReasoningSummary::None),
    ] {
        let request = AnthropicDecoder.decode_request(request_with(serde_json::json!({
            "thinking": {"type": "adaptive", "display": display},
            "cache_control": {"type": "ephemeral", "ttl": "5m"},
            "tools": [{"type": "code_execution_20250825", "name": "code_execution"}],
            "container": {"id": "container_1", "skills": [{"private": true}]},
            "context_management": {"edits": [
                {"type": "clear_tool_uses_20250919", "trigger": {"type": "input_tokens", "value": 100}},
                {"type": "compact_20260112", "trigger": {"type": "input_tokens", "value": 120000}, "instructions": "private"}
            ]},
            "service_tier": "standard_only",
            "metadata": {"user_id": "user_1", "private": "ignored"}
        }))).unwrap();
        assert_eq!(request.reasoning.summary, Some(summary));
        assert_eq!(
            request.parameters.cache.retention,
            Some(CacheRetention::Default)
        );
        assert_eq!(
            request
                .parameters
                .compaction
                .as_ref()
                .unwrap()
                .threshold_tokens,
            Some(120000)
        );
        assert_eq!(request.parameters.service_tier, Some(ServiceTier::Default));
        let NativeTool::CodeExecution(tool) = &request.parameters.native_tools[0] else {
            panic!("expected code execution");
        };
        assert_eq!(tool.container.as_deref(), Some("container_1"));
        let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
            .encode_request(request)
            .unwrap();
        assert_eq!(body["thinking"]["display"], display);
        assert_eq!(body["container"], "container_1");
        assert_eq!(body["tools"][0]["type"], "code_execution_20260120");
        assert!(body["tools"][0].get("container").is_none());
        assert_eq!(
            body["context_management"],
            serde_json::json!({"edits": [{
                "type": "compact_20260112", "trigger": {"type": "input_tokens", "value": 120000}
            }]})
        );
        assert_eq!(body["metadata"], serde_json::json!({"user_id": "user_1"}));
    }
}

#[test]
fn explicit_nulls_only_reset_inheritance_and_are_not_emitted() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "system": null, "tools": null, "temperature": null, "top_p": null,
            "max_tokens": null, "thinking": null, "cache_control": null,
            "service_tier": null, "metadata": {"user_id": null}, "output_config": null
        })))
        .unwrap();
    for reset in [
        ParameterReset::Instructions,
        ParameterReset::NativeTools,
        ParameterReset::Temperature,
        ParameterReset::TopP,
        ParameterReset::MaxTokens,
        ParameterReset::Reasoning,
        ParameterReset::CacheRetention,
        ParameterReset::ServiceTier,
        ParameterReset::EndUser,
        ParameterReset::ResponseFormat,
    ] {
        assert!(request.parameters.resets(reset), "{reset:?}");
    }
    let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .unwrap();
    assert!(
        body.as_object()
            .unwrap()
            .values()
            .all(|value| !value.is_null())
    );
}

#[test]
fn unsupported_tools_and_vendor_function_controls_are_silently_dropped() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "tools": [
                {"type": "computer_20250124", "name": "computer", "display_width_px": 1024},
                {"type": "web_fetch_20250910", "name": "web_fetch"},
                {"type": "bash_20250124", "name": "bash"},
                {"name": "read", "input_schema": {"type": "object"},
                 "eager_input_streaming": {"unsupported": true}, "allowed_callers": 123,
                 "citations": "unsupported", "input_examples": false, "cache_control": false}
            ],
            "speed": {"unsupported": true},
            "inference_geo": 123
        })))
        .unwrap();
    assert!(request.parameters.native_tools.is_empty());
    let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .unwrap();
    assert_eq!(
        body["tools"],
        serde_json::json!([{
            "name": "read", "input_schema": {"type": "object"}
        }])
    );
    assert!(body.get("speed").is_none());
    assert!(body.get("inference_geo").is_none());
}

#[test]
fn native_tool_choice_has_unified_ownership() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "tool_choice": {"type": "tool", "name": "web_search"}
        })))
        .unwrap();
    assert!(matches!(
        request.tool_choice,
        Some(ToolChoice::Native(NativeToolKind::WebSearch))
    ));
    let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .unwrap();
    assert_eq!(
        body["tool_choice"],
        serde_json::json!({"type": "tool", "name": "web_search"})
    );
}

#[test]
fn nested_null_controls_reset_without_emitting_nulls() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "thinking": {"type": "adaptive", "display": null},
            "output_config": {"effort": null, "format": null},
            "cache_control": {"type": "ephemeral", "ttl": null},
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": null}
        })))
        .unwrap();
    for reset in [
        ParameterReset::ReasoningSummary,
        ParameterReset::ReasoningLevel,
        ParameterReset::ResponseFormat,
        ParameterReset::CacheRetention,
        ParameterReset::ParallelToolCalls,
    ] {
        assert!(request.parameters.resets(reset), "{reset:?}");
    }
    assert!(request.reasoning.summary.is_none());
    assert!(request.parameters.cache.retention.is_none());
    let (body, _) = crate::codec::anthropic::messages::encoder::AnthropicEncoder
        .encode_request(request)
        .unwrap();
    assert!(body["thinking"].get("display").is_none());
    assert!(body.get("cache_control").is_none());
    assert!(
        body["tool_choice"]
            .get("disable_parallel_tool_use")
            .is_none()
    );
    assert!(body["output_config"].get("format").is_none());
}

#[test]
fn compaction_control_enters_ir_and_null_does_not_request_compaction() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "compaction": {"type": "summarize", "instructions": "Be brief"}
        })))
        .unwrap();
    assert_eq!(
        serde_json::to_value(&request.parameters.compaction).unwrap(),
        serde_json::json!({"instructions": "Be brief"})
    );
    assert!(request.items.last().unwrap().is_compaction_trigger());
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({"compaction": null})))
        .unwrap();
    assert!(request.parameters.compaction.is_none());
    assert!(
        !request
            .items
            .iter()
            .any(|item| item.is_compaction_trigger())
    );
}

#[test]
fn native_compaction_block_decodes_to_the_same_state_the_upstream_registered() {
    use stravia_runtime_contract::protocol::ir::canonical::native_compaction_item;
    let upstream = serde_json::json!({
        "type": "message",
        "content": [{"type": "compaction", "content": "Summary", "signature": "sig-1"}],
        "stop_reason": "compaction",
        "usage": {"input_tokens": 0, "output_tokens": 0}
    });
    let registered =
        crate::codec::compaction::parse_anthropic_compaction(upstream.clone()).unwrap();
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "messages": [
                {"role": "assistant", "content": upstream["content"].clone()},
                {"role": "user", "content": "go on"}
            ]
        })))
        .unwrap();
    let replayed = request
        .items
        .iter()
        .find(|item| item.is_compaction())
        .unwrap();
    assert_eq!(
        native_compaction_item(replayed),
        native_compaction_item(&registered.items[0])
    );
}

#[test]
fn compaction_block_without_a_signature_is_rejected() {
    let error = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [{"type": "compaction", "content": "Summary"}]},
                {"role": "user", "content": "go on"}
            ]
        })))
        .unwrap_err();
    assert!(error.to_string().contains("signature"), "{error}");
}

#[test]
fn responses_window_sealed_in_a_block_expands_to_its_items() {
    let output = vec![
        serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "earlier question"}]}),
        serde_json::json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "enc"}),
    ];
    let signature = crate::codec::compaction::seal_responses(&output);
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [{
                    "type": "compaction",
                    "content": crate::codec::compaction::ENVELOPE_PLACEHOLDER_CONTENT,
                    "signature": signature
                }]},
                {"role": "user", "content": "go on"}
            ]
        })))
        .unwrap();
    assert_eq!(request.items.len(), 3);
    assert!(request.items[1].is_compaction());
    let replayed = stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(
        &request.items[1],
    )
    .unwrap();
    assert_eq!(replayed["encrypted_content"], "enc");
}

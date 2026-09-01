use super::*;

#[test]
fn accepts_codex_rolling_request_extensions() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }],
            "tools": [
                {
                    "type": "function",
                    "name": "shell_command",
                    "description": "Run a command",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "namespace",
                    "name": "multi_agent_v1",
                    "tools": [{
                        "type": "function",
                        "name": "spawn_agent",
                        "parameters": {"type": "object"}
                    }]
                },
                {"type": "web_search"}
            ],
            "tool_choice": "auto",
            "reasoning": {"effort": "high", "summary": "auto"},
            "include": ["reasoning.encrypted_content"],
            "client_metadata": {
                "session_id": "session",
                "turn_id": "turn"
            },
            "stream": true
        }))
        .expect("Codex rolling Responses extensions should be accepted");

    assert_eq!(
        request.reasoning.level,
        Some(crate::thinking::ThinkingLevel::High)
    );
    assert_eq!(
        request
            .tools
            .as_ref()
            .expect("function tools")
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["shell_command"]
    );
}

#[test]
fn registered_web_search_extension_decodes_without_vendor_aliases() {
    let ParsedTools {
        tools,
        native_web_search: native,
        passthrough_tools,
    } = parse_tools(Some(&serde_json::json!([{
        "type": "stravia:web_search",
        "filters": { "allowed_domains": ["docs.rs"] },
        "search_context_size": "high"
    }])))
    .expect("registered web search");

    assert_eq!(tools, Some(Vec::new()));
    assert!(passthrough_tools.is_empty());
    let native = native.expect("web search extension");
    assert_eq!(native["type"], "stravia:web_search");
    assert_eq!(native["filters"]["allowed_domains"][0], "docs.rs");
    assert_eq!(native["search_context_size"], "high");
}

#[test]
fn rolling_tools_are_preserved_and_client_function_names_remain_independent() {
    for tool_type in ["web_search", "web_search_2025_08_26", "web_search_preview"] {
        let parsed = parse_tools(Some(&serde_json::json!([{"type": tool_type}])))
            .expect("rolling hosted tool");
        assert_eq!(parsed.passthrough_tools[0]["type"], tool_type);
    }
    parse_tools(Some(&serde_json::json!([{"type": "vendor:unknown"}])))
        .expect_err("unregistered namespaced extension must fail");

    let ParsedTools {
        tools,
        native_web_search: native,
        passthrough_tools,
    } = parse_tools(Some(&serde_json::json!([
        { "type": "stravia:web_search" },
        {
            "type": "function",
            "name": "web_search",
            "parameters": { "type": "object" }
        }
    ])))
    .expect("client function and registered extension");
    assert_eq!(tools.expect("function tools")[0].name, "web_search");
    assert!(native.is_some());
    assert!(passthrough_tools.is_empty());
}

#[test]
fn response_input_message_preserves_inline_images_in_stable_order() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "parent",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "inspect"},
                    {
                        "type": "input_image",
                        "image_url": "data:image/png;base64,aW1hZ2U=",
                        "detail": "high"
                    }
                ]
            }]
        }))
        .expect("Responses request");

    assert!(matches!(
        request.items.as_slice(),
        [AiItem {
            content: MessageContent::Blocks(blocks),
            ..
        }] if matches!(
            blocks.as_slice(),
            [
                crate::protocol::ir::ContentBlock::Text { text, .. },
                crate::protocol::ir::ContentBlock::Image {
                    source: crate::protocol::ir::MediaSource::Base64 { media_type, data },
                    detail: Some(detail),
                    ..
                }
            ] if text == "inspect" && media_type == "image/png" && data == "aW1hZ2U=" && detail == "high"
        )
    ));
}

#[test]
fn top_level_instructions_are_not_inserted_into_message_history() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "instructions": "Follow the request-level contract.",
            "input": "hello"
        }))
        .expect("Open Responses request");

    assert_eq!(
        request.instructions.as_deref(),
        Some("Follow the request-level contract.")
    );
    assert_eq!(request.items.len(), 1);
    assert_eq!(request.items[0].role, Role::User);
}

#[test]
fn developer_role_remains_distinct_in_canonical_messages() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [{
                "type": "message",
                "role": "developer",
                "content": "Prefer repository conventions."
            }]
        }))
        .expect("Open Responses request");

    assert_eq!(request.items[0].role, Role::Developer);
}

#[test]
fn continuation_may_omit_model_and_input() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "previous_response_id": "resp_parent",
            "model": null,
            "input": null
        }))
        .expect("continuation request");

    assert!(request.model.is_empty());
    assert!(request.items.is_empty());
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext else {
        panic!("Open Responses extension");
    };
    assert_eq!(
        extension.previous_response_id.as_deref(),
        Some("resp_parent")
    );
}

#[test]
fn resolves_item_references_from_the_current_request_graph() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [
                {
                    "type": "message",
                    "id": "msg_local",
                    "role": "user",
                    "content": "repeat me"
                },
                {"type": "item_reference", "id": "msg_local"}
            ]
        }))
        .expect("current item reference");

    assert_eq!(request.items.len(), 2);
    assert_eq!(request.items[0].content.to_text(), "repeat me");
    assert_eq!(request.items[1].content.to_text(), "repeat me");
    assert_eq!(request.items[1].id_ref(), Some("msg_local"));
}
#[test]
fn preserves_fields_added_after_the_dated_protocol() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": "hello",
            "conversation": "conv_123"
        }))
        .expect("rolling provider field");

    let Some(ProtocolExt::OpenResponses(extension)) = request.ext else {
        panic!("Open Responses extension");
    };
    assert_eq!(extension.passthrough_body["conversation"], "conv_123");
}

#[test]
fn rejects_mistyped_known_request_fields() {
    for (field, value) in [
        ("stream", serde_json::json!("false")),
        ("store", serde_json::json!("false")),
        ("temperature", serde_json::json!("cold")),
        ("max_output_tokens", serde_json::json!(-1)),
        ("instructions", serde_json::json!(["not", "text"])),
        ("metadata", serde_json::json!("not-an-object")),
        ("include", serde_json::json!("not-an-array")),
    ] {
        let mut body = serde_json::json!({
            "model": "logical-model",
            "input": "hello"
        });
        body[field] = value;
        let error = ResponsesDecoder
            .decode_request(body)
            .expect_err("mistyped known field must fail");
        assert!(
            error.to_string().contains(field),
            "error did not identify {field}: {error}"
        );
    }
}
#[test]
fn accepts_nullable_dated_integer_controls() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": "hello",
            "max_output_tokens": null,
            "max_tool_calls": null,
            "top_logprobs": null
        }))
        .expect("nullable integer controls");

    assert_eq!(request.generation.max_tokens, None);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext else {
        unreachable!();
    };
    assert_eq!(extension.max_tool_calls, None);
    assert_eq!(extension.top_logprobs, None);
}

#[test]
fn rejects_integer_fields_that_exceed_canonical_width() {
    for field in ["max_output_tokens", "max_tool_calls", "top_logprobs"] {
        let mut body = serde_json::json!({
            "model": "logical-model",
            "input": "hello",
        });
        body[field] = serde_json::json!(u64::from(u32::MAX) + 1);
        let error = ResponsesDecoder
            .decode_request(body)
            .expect_err("oversized integer must fail");
        assert!(
            error.to_string().contains(field),
            "error did not identify {field}: {error}"
        );
    }
}

#[test]
fn explicit_empty_tools_remains_an_override() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": "hello",
            "tools": [],
        }))
        .expect("request with explicit empty tools");

    assert_eq!(request.tools, Some(Vec::new()));
}

#[test]
fn decodes_dated_named_and_allowed_tool_choices() {
    let named = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": "hello",
            "tool_choice": {"type": "function", "name": "lookup"},
        }))
        .expect("dated named tool choice");
    assert!(matches!(
        named.tool_choice,
        Some(ToolChoice::Named { ref name }) if name == "lookup"
    ));

    let allowed_value = serde_json::json!({
        "type": "allowed_tools",
        "mode": "required",
        "tools": [{"type": "function", "name": "lookup"}],
    });
    let allowed = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": "hello",
            "tool_choice": allowed_value.clone(),
        }))
        .expect("dated allowed_tools choice");
    assert!(matches!(
        allowed.tool_choice,
        Some(ToolChoice::Raw(ref value)) if value == &allowed_value
    ));

    ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": "hello",
            "tool_choice": {"type": "function", "function": {"name": "lookup"}},
        }))
        .expect_err("rolling nested function shape must fail");
}

#[test]
fn rejects_input_video_in_message_content() {
    let error = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_video",
                    "video_url": "data:video/mp4;base64,AA==",
                }],
            }],
        }))
        .expect_err("dated message content union excludes input_video");

    assert!(error.to_string().contains("input_video"));
}

#[test]
fn ignores_unknown_nested_fields() {
    ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "hello",
                    "content_extension": true
                }],
                "item_extension": true
            }],
            "reasoning": {"effort": "high", "reasoning_extension": true},
            "stream_options": {
                "include_obfuscation": true,
                "stream_extension": true
            },
            "text": {
                "format": {"type": "text", "format_extension": true},
                "text_extension": true
            },
            "tools": [{
                "type": "function",
                "name": "lookup",
                "tool_extension": true
            }]
        }))
        .expect("unknown fields must be ignored");
}

#[test]
fn rejects_invalid_known_function_tool_fields() {
    ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": "hello",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "strict": "yes"
            }]
        }))
        .expect_err("invalid known field must fail");
}

#[test]
fn preserves_reasoning_as_an_ordered_graph_item() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "private reasoning"}],
                    "content": [{"type": "reasoning_text", "text": "full reasoning"}],
                    "encrypted_content": "opaque"
                },
                {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "new instruction"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "answer"}]
                }
            ]
        }))
        .expect("dated request");

    let (summary, content, encrypted) = request.items[0]
        .reasoning_ref()
        .expect("typed reasoning item");
    assert_eq!(summary, ["private reasoning"]);
    assert_eq!(content, ["full reasoning"]);
    assert_eq!(encrypted, Some("opaque"));
    assert_eq!(request.items[0].id_ref(), Some("rs_1"));
    assert_eq!(request.items[1].role, Role::Developer);
    assert_eq!(request.items[2].role, Role::Assistant);
    let (encoded, _) = super::super::encoder::ResponsesEncoder
        .encode_request(&request)
        .expect("encode dated request");
    assert_eq!(
        encoded["input"][0]["summary"][0]["text"],
        "private reasoning"
    );
    assert_eq!(encoded["input"][0]["content"][0]["text"], "full reasoning");
}
#[test]
fn input_image_null_detail_is_treated_as_omitted() {
    let request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": "https://example.test/image.png",
                    "detail": null
                }]
            }]
        }))
        .expect("schema-valid null detail");
    let MessageContent::Blocks(blocks) = &request.items[0].content else {
        panic!("image block");
    };
    assert!(matches!(
        &blocks[0],
        ContentBlock::Image { detail: None, .. }
    ));
}

#[test]
fn rejects_message_content_without_a_type_discriminator() {
    ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "logical-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"text": "ambiguous"}]
            }]
        }))
        .expect_err("untyped message content must fail");
}

#[test]
fn rejects_malformed_dated_text_blocks() {
    for block in [
        serde_json::json!({"type": "input_text"}),
        serde_json::json!({"type": "input_text", "text": 42}),
    ] {
        ResponsesDecoder
            .decode_request(serde_json::json!({
                "model": "logical-model",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [block]
                }]
            }))
            .expect_err("malformed dated text block must fail");
    }
}

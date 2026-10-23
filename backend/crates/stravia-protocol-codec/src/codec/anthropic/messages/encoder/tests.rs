use super::*;

#[test]
fn anthropic_projects_only_explicit_summary_controls() {
    use stravia_runtime_contract::thinking::TargetThinkingControl;
    let mut request = crate::codec::anthropic::messages::decoder::AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "claude",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
    let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
    assert!(body.get("thinking").is_none());
    for summary in [
        None,
        Some(ReasoningSummary::None),
        Some(ReasoningSummary::Concise),
    ] {
        request.reasoning.summary = summary;
        request.reasoning.target_control = Some(TargetThinkingControl::Budget { value: 2048 });
        let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
        assert_eq!(
            body["thinking"].get("display").and_then(Value::as_str),
            summary.map(|value| if value == ReasoningSummary::None {
                "omitted"
            } else {
                "summarized"
            })
        );
    }
    request.reasoning.target_control = Some(TargetThinkingControl::Disabled);
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["thinking"]["type"], "disabled");
    assert_eq!(body["thinking"]["display"], "summarized");
}

#[test]
fn server_tool_results_keep_the_anthropic_wire_discriminator() {
    let encoded = encode_content_block_for_anthropic(ContentBlock::ServerToolResult {
        tool_use_id: "srv_123".into(),
        content: serde_json::json!([{"type": "text", "text": "result"}]),
        content_kind: Some(
            stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
        ),
        server_type: Some("web_search_tool_result".into()),
        cache_control: None,
    });

    assert_eq!(encoded[0]["type"], "web_search_tool_result");
    assert_eq!(encoded[0]["tool_use_id"], "srv_123");
    assert_eq!(encoded[0]["content"][0]["text"], "result");
}

#[test]
fn server_tool_uses_keep_the_anthropic_wire_discriminator() {
    let encoded = encode_content_block_for_anthropic(ContentBlock::ServerToolUse {
        id: "srv_123".into(),
        name: "web_search".into(),
        input: serde_json::json!({"query": "weather"}),
        server_type: Some("web_search_tool_use".into()),
        cache_control: None,
    });

    assert_eq!(encoded[0]["type"], "web_search_tool_use");
    assert_eq!(encoded[0]["id"], "srv_123");
    assert_eq!(encoded[0]["name"], "web_search");
    assert_eq!(encoded[0]["input"]["query"], "weather");
}

#[test]
fn synthetic_tool_ids_are_request_local_distinct_and_skip_supplied_ids() {
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::Assistant,
            content: MessageContent::Text(String::new().into()),
            tool_calls: Some(vec![
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "".into(),
                    name: "first".into(),
                    arguments: "{}".into(),
                },
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "tc_1".into(),
                    name: "external".into(),
                    arguments: "{}".into(),
                },
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "".into(),
                    name: "second".into(),
                    arguments: "{}".into(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        }],
    );

    let (first, _) = AnthropicEncoder
        .encode_request(request.clone())
        .expect("encode");
    let (repeated, _) = AnthropicEncoder.encode_request(request).expect("encode");
    let ids: Vec<&str> = first["messages"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["id"].as_str().unwrap())
        .collect();

    assert_eq!(ids, ["tc_2", "tc_1", "tc_3"]);
    assert_eq!(repeated["messages"], first["messages"]);
}

#[test]
fn preserves_none_tool_choice() {
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
    request.tool_choice = Some(ToolChoice::None);

    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["tool_choice"], serde_json::json!({"type": "none"}));
    let decoded = super::super::decoder::AnthropicDecoder
        .decode_request(body)
        .unwrap();
    assert!(matches!(decoded.tool_choice, Some(ToolChoice::None)));
    assert_eq!(decoded.items[0].content.to_text(), "hello");
}

#[test]
fn native_documents_keep_supported_sources_and_omit_audio() {
    use stravia_runtime_contract::protocol::ir::DocumentSource;
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Document {
                    source: DocumentSource::Base64Pdf {
                        data: "cGRm".into(),
                    },
                    title: Some("report".into()),
                    context: Some("reference material".into()),
                    cache_control: None,
                },
                ContentBlock::Document {
                    source: DocumentSource::PlainText {
                        data: "notes".into(),
                    },
                    title: None,
                    context: None,
                    cache_control: None,
                },
                ContentBlock::Audio {
                    source: MediaSource::Base64 {
                        media_type: "audio/wav".into(),
                        data: "YXVkaW8=".into(),
                    },
                },
                ContentBlock::Document {
                    source: DocumentSource::Blocks {
                        content: vec![
                            ContentBlock::Text {
                                text: "kept excerpt".to_owned().into(),
                                cache_control: None,
                            },
                            ContentBlock::Video {
                                source: MediaSource::Url("https://example.test/clip.mp4".into()),
                                media_type: None,
                            },
                            ContentBlock::ToolUse {
                                id: "nested-call".into(),
                                name: "probe".into(),
                                input: serde_json::json!({}),
                                cache_control: None,
                            },
                        ],
                    },
                    title: None,
                    context: None,
                    cache_control: None,
                },
                ContentBlock::Document {
                    source: DocumentSource::Blocks {
                        content: vec![ContentBlock::Unknown {
                            raw: serde_json::json!({"type": "unavailable_document_part"}),
                        }],
                    },
                    title: None,
                    context: None,
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(
        body["messages"][0]["content"],
        serde_json::json!([
            {
                "type": "document",
                "source": {"type": "base64", "media_type": "application/pdf", "data": "cGRm"},
                "title": "report",
                "context": "reference material"
            },
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "notes"}},
            {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": "kept excerpt"}]}}
        ])
    );
}

#[test]
fn canonical_files_and_images_use_official_anthropic_sources() {
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Image {
                    source: MediaSource::FileId {
                        file_id: "image-1".into(),
                        detail: None,
                    },
                    detail: None,
                    cache_control: None,
                },
                ContentBlock::Image {
                    source: MediaSource::Url("https://example.test/image.png".into()),
                    detail: None,
                    cache_control: None,
                },
                ContentBlock::File {
                    source: MediaSource::FileId {
                        file_id: "file-1".into(),
                        detail: None,
                    },
                    media_type: None,
                },
                ContentBlock::File {
                    source: MediaSource::Base64 {
                        media_type: "application/pdf".into(),
                        data: "cGRm".into(),
                    },
                    media_type: None,
                },
                ContentBlock::File {
                    source: MediaSource::Url("https://example.test/report.pdf".into()),
                    media_type: Some("application/pdf".into()),
                },
                ContentBlock::File {
                    source: MediaSource::Base64 {
                        media_type: "text/plain".into(),
                        data: "bm90ZXM=".into(),
                    },
                    media_type: None,
                },
                ContentBlock::File {
                    source: MediaSource::Base64 {
                        media_type: "application/zip".into(),
                        data: "emlw".into(),
                    },
                    media_type: None,
                },
                ContentBlock::Audio {
                    source: MediaSource::Url("https://example.test/audio.wav".into()),
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(
        body["messages"][0]["content"],
        serde_json::json!([
            {"type": "image", "source": {"type": "file", "file_id": "image-1"}},
            {"type": "image", "source": {"type": "url", "url": "https://example.test/image.png"}},
            {"type": "document", "source": {"type": "file", "file_id": "file-1"}},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "cGRm"}},
            {"type": "document", "source": {"type": "url", "url": "https://example.test/report.pdf"}},
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "notes"}}
        ])
    );
}

#[test]
fn native_file_and_document_content_decode_and_roundtrip() {
    use stravia_runtime_contract::protocol::ir::DocumentSource;
    let wire = serde_json::json!({
        "model": "model", "max_tokens": 4096,
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "file", "file_id": "image-1"}},
            {"type": "document", "source": {"type": "file", "file_id": "file-1"}, "title": "report", "context": "reference"},
            {"type": "document", "source": {"type": "content", "content": "excerpt"}, "title": "notes"},
            {"type": "document", "source": {"type": "content", "content": [
                {"type": "text", "text": "visible"},
                {"type": "image", "source": {"type": "url", "url": "https://example.test/image.png"}}
            ]}}
        ]}]
    });
    let request = crate::codec::anthropic::messages::decoder::AnthropicDecoder
        .decode_request(wire.clone())
        .unwrap();
    let MessageContent::Blocks(blocks) = &request.items[0].content else {
        panic!("expected blocks")
    };
    assert!(
        matches!(&blocks[0], ContentBlock::Image { source: MediaSource::FileId { file_id, .. }, .. } if file_id == "image-1")
    );
    assert!(
        matches!(&blocks[1], ContentBlock::File { source: MediaSource::FileId { file_id, .. }, .. } if file_id == "file-1")
    );
    assert!(
        matches!(&blocks[2], ContentBlock::Document { source: DocumentSource::Blocks { content }, .. } if matches!(&content[0], ContentBlock::Text { text, .. } if text.as_ref() == "excerpt"))
    );
    assert!(
        matches!(&blocks[3], ContentBlock::Document { source: DocumentSource::Blocks { content }, .. } if matches!(&content[1], ContentBlock::Image { source: MediaSource::Url(url), .. } if url == "https://example.test/image.png"))
    );
    let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
    assert_eq!(body["messages"], wire["messages"]);
    let mut canonical = request;
    canonical
        .meta
        .vendor
        .ingress
        .remove("__anthropic_raw_messages");
    let (body, _) = AnthropicEncoder.encode_request(canonical).unwrap();
    assert_eq!(
        body["messages"][0]["content"][2]["source"]["content"][0]["text"],
        "excerpt"
    );
    assert_eq!(
        body["messages"][0]["content"][3]["source"]["content"][1]["source"]["type"],
        "url"
    );
}

#[test]
fn native_audio_is_omitted_without_losing_supported_raw_fields() {
    let wire = serde_json::json!({
        "model": "model", "max_tokens": 4096,
        "messages": [
            {"role": "user", "content": [
                {"type": "document", "source": {"type": "file", "file_id": "file-1"}, "title": "report", "context": "reference", "cache_control": {"type": "ephemeral"}},
                {"type": "input_audio", "source": {"type": "base64", "media_type": "audio/wav", "data": "YXVkaW8="}}
            ]},
            {"role": "user", "content": [
                {"type": "input_audio", "source": {"type": "base64", "media_type": "audio/wav", "data": "YXVkaW8="}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call-1", "content": [
                    {"type": "image", "source": {"type": "file", "file_id": "image-1"}},
                    {"type": "input_audio", "source": {"type": "base64", "media_type": "audio/wav", "data": "YXVkaW8="}}
                ]}
            ]}
        ]
    });
    let request = crate::codec::anthropic::messages::decoder::AnthropicDecoder
        .decode_request(wire.clone())
        .unwrap();
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["messages"][0]["content"],
        serde_json::json!([wire["messages"][0]["content"][0].clone()])
    );
    assert_eq!(
        body["messages"][1]["content"][0]["content"],
        serde_json::json!([
            {"type": "image", "source": {"type": "file", "file_id": "image-1"}}
        ])
    );
}

#[test]
fn invalid_native_media_sources_are_errors() {
    for block in [
        serde_json::json!({"type": "image", "source": {"type": "file"}}),
        serde_json::json!({"type": "document", "source": {"type": "file"}}),
        serde_json::json!({"type": "document", "source": {"type": "base64", "media_type": "application/zip", "data": "emlw"}}),
        serde_json::json!({"type": "image", "source": {"type": "unavailable"}}),
    ] {
        assert!(crate::codec::anthropic::messages::decoder::AnthropicDecoder.decode_request(serde_json::json!({
            "model": "model", "max_tokens": 4096, "messages": [{"role": "user", "content": [block]}]
        })).is_err());
    }
    for data in ["not base64!", "/w=="] {
        let request = AiRequest::new(
            "model",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::File {
                    source: MediaSource::Base64 {
                        media_type: "text/plain".into(),
                        data: data.into(),
                    },
                    media_type: None,
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        assert!(AnthropicEncoder.encode_request(request).is_err());
    }
}

#[test]
fn tool_result_media_keeps_only_official_nested_blocks() {
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "result".to_owned().into(),
                    cache_control: None,
                },
                ContentBlock::File {
                    source: MediaSource::FileId {
                        file_id: "file-1".into(),
                        detail: None,
                    },
                    media_type: None,
                },
                ContentBlock::Image {
                    source: MediaSource::FileId {
                        file_id: "image-1".into(),
                        detail: None,
                    },
                    detail: None,
                    cache_control: None,
                },
                ContentBlock::Audio {
                    source: MediaSource::Base64 {
                        media_type: "audio/wav".into(),
                        data: "YXVkaW8=".into(),
                    },
                },
                ContentBlock::ToolUse {
                    id: "nested".into(),
                    name: "probe".into(),
                    input: serde_json::json!({}),
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: Some("call-1".into()),
            meta: None,
        }],
    );
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(
        body["messages"][0]["content"][0]["content"],
        serde_json::json!([
            {"type": "text", "text": "result"},
            {"type": "document", "source": {"type": "file", "file_id": "file-1"}},
            {"type": "image", "source": {"type": "file", "file_id": "image-1"}}
        ])
    );
}

#[test]
fn raw_tool_result_media_omits_only_unsupported_block_kinds() {
    let encoded = encode_content_block_for_anthropic(ContentBlock::ToolResult {
        tool_use_id: "call-1".into(),
        content: serde_json::json!([
            {"type": "image", "source": {"type": "file", "file_id": "image-1"}},
            {"type": "document", "source": {"type": "file", "file_id": "file-1"}},
            {"type": "document", "source": {"type": "content", "content": "excerpt"}},
            {"type": "input_audio", "source": {"type": "base64", "media_type": "audio/wav", "data": "YXVkaW8="}},
            {"type": "unavailable", "data": "discard"}
        ]),
        content_kind: Some(ToolResultContentKind::ContentBlocks),
        is_error: None,
        cache_control: None,
    });
    assert_eq!(
        encoded[0]["content"],
        serde_json::json!([
            {"type": "image", "source": {"type": "file", "file_id": "image-1"}},
            {"type": "document", "source": {"type": "file", "file_id": "file-1"}},
            {"type": "document", "source": {"type": "content", "content": "excerpt"}}
        ])
    );
}

#[test]
fn effort_control_encodes_adaptive_without_replaying_raw_thinking() {
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
            value: "xhigh".into(),
        },
    );
    request.meta.vendor.ingress.insert(
        "__anthropic_thinking".into(),
        serde_json::json!({"type": "disabled"}),
    );

    let (body, _) = AnthropicEncoder
        .encode_request(request)
        .expect("adaptive thinking");
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "xhigh");
}

#[test]
fn cross_protocol_controls_reach_anthropic_and_return_to_openai() {
    use crate::transform::ProtocolTransform;
    use stravia_runtime_contract::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    };
    let schema = serde_json::json!({
        "type": "object", "properties": {"answer": {"type": "string"}},
        "required": ["answer"], "additionalProperties": false
    });
    for (choice, parallel) in [("none", true), ("auto", false), ("required", true)] {
        let pair = ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                ANTHROPIC_MESSAGES_2023_06_01,
            )
            .unwrap();
        let request = pair.decode_request(serde_json::json!({
            "model": "claude", "messages": [{"role": "user", "content": "hello"}],
            "tool_choice": choice, "parallel_tool_calls": parallel, "stop": ["END"],
            "tools": [{"type": "function", "function": {"name": "answer", "parameters": schema, "strict": true}}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": schema, "strict": true}}
        })).unwrap();
        let body = pair.encode_request(request).unwrap().body;
        assert_eq!(
            body["tool_choice"]["type"],
            if choice == "required" { "any" } else { choice }
        );
        if choice == "none" {
            assert!(
                body["tool_choice"]
                    .get("disable_parallel_tool_use")
                    .is_none()
            );
        } else {
            assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], !parallel);
        }
        assert_eq!(body["output_config"]["format"]["schema"], schema);
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["stop_sequences"], serde_json::json!(["END"]));
        let reverse = ProtocolTransform::global()
            .bind(
                ANTHROPIC_MESSAGES_2023_06_01,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            )
            .unwrap();
        let output = reverse
            .encode_request(reverse.decode_request(body).unwrap())
            .unwrap()
            .body;
        assert_eq!(output["tool_choice"], choice);
        assert_eq!(output["response_format"]["json_schema"]["schema"], schema);
        assert_eq!(output["tools"][0]["function"]["strict"], true);
        assert_eq!(output["stop"], serde_json::json!(["END"]));
        if choice != "none" {
            assert_eq!(output["parallel_tool_calls"], parallel);
        }
    }
}

#[test]
fn canonical_output_controls_replace_native_values() {
    use stravia_runtime_contract::protocol::ir::ResponseFormat;
    let native_format = serde_json::json!({"type": "json_schema", "schema": {
        "type": "object", "properties": {}, "additionalProperties": false
    }});
    let mut request = super::super::decoder::AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "claude", "max_tokens": 2048,
            "messages": [{"role": "user", "content": "hello"}],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": false},
            "stop_sequences": ["END"],
            "output_config": {"format": native_format, "effort": "high"}
        }))
        .unwrap();
    assert!(matches!(
        request.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));
    assert_eq!(request.parallel_tool_calls, Some(true));
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "canonical".into(),
        description: None,
        schema: serde_json::json!({"type": "string"}),
        strict: None,
    });
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "low".into(),
        },
    );
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(
        body["output_config"],
        serde_json::json!({"format": {"type": "json_schema", "schema": {"type": "string"}}, "effort": "low"})
    );
    assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], false);
    assert_eq!(body["stop_sequences"], serde_json::json!(["END"]));
}

#[test]
fn canonical_schema_and_effort_merge_without_inventing_json_object_schema() {
    use stravia_runtime_contract::protocol::ir::ResponseFormat;
    let mut request = AiRequest::new(
        "claude",
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
    let schema =
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false});
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        description: None,
        schema: schema.clone(),
        strict: Some(true),
    });
    let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
    assert_eq!(
        body["output_config"],
        serde_json::json!({"effort": "high", "format": {"type": "json_schema", "schema": schema}})
    );
    request.response_format = Some(ResponseFormat::JsonObject);
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["output_config"], serde_json::json!({"effort": "high"}));
}

#[test]
fn raw_output_config_fallback_merges_and_invalid_native_format_still_errors() {
    use stravia_runtime_contract::protocol::ir::ResponseFormat;
    let mut request = AiRequest::new(
        "claude",
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
        schema: serde_json::json!({"type": "string"}),
        strict: None,
    });
    request.meta.vendor.ingress.insert(
        "__anthropic_output_config".into(),
        serde_json::json!({"effort": "medium"}),
    );
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["output_config"]["effort"], "medium");
    assert_eq!(
        body["output_config"]["format"]["schema"],
        serde_json::json!({"type": "string"})
    );
    for output_config in [
        serde_json::json!({"format": {"type": "json_object"}}),
        serde_json::json!({"format": {"type": "json_schema", "schema": false}}),
        serde_json::json!({"effort": 12}),
    ] {
        assert!(super::super::decoder::AnthropicDecoder.decode_request(serde_json::json!({
            "model": "claude", "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello"}], "output_config": output_config
        })).is_err());
    }
}

fn canonical_tool(
    name: impl Into<String>,
    schema: Value,
    strict: Option<bool>,
) -> stravia_runtime_contract::protocol::ir::ToolSpec {
    stravia_runtime_contract::protocol::ir::ToolSpec {
        name: name.into(),
        description: None,
        parameters: schema,
        strict,
        defer_loading: None,
    }
}

fn parameter_request() -> AiRequest {
    AiRequest::new(
        "claude",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
}

#[test]
fn strict39_keeps_nullable_schema_and_relaxes_only_true_strict() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "value": {"anyOf": [{"type": "string", "minLength": 2}, {"type": "null"}]},
            "extra": {"$ref": "https://example.test/schema"}
        },
        "required": ["value", "extra"],
        "additionalProperties": false,
        "$defs": {"recursive": {"$ref": "#/$defs/recursive"}},
        "default": {"minimum": 7}
    });
    let mut request = parameter_request();
    request.tools = Some(
        (0..39)
            .map(|index| canonical_tool(format!("tool_{index}"), schema.clone(), Some(index != 0)))
            .collect(),
    );
    request.response_format = Some(
        stravia_runtime_contract::protocol::ir::ResponseFormat::JsonSchema {
            name: "answer".into(),
            description: None,
            schema: schema.clone(),
            strict: Some(true),
        },
    );
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["tools"].as_array().unwrap().len(), 39);
    assert_eq!(body["tools"][0]["strict"], false);
    for tool in body["tools"].as_array().unwrap() {
        assert_eq!(tool["input_schema"], schema);
        assert_ne!(tool.get("strict"), Some(&Value::Bool(true)));
    }
    assert_eq!(body["output_config"]["format"]["schema"], schema);
}

#[test]
fn strict_budget_uses_aggregate_optional_parameters_and_union_branches() {
    for (optional, branches, expected_strict) in
        [(24, 0, true), (25, 0, false), (0, 16, true), (0, 17, false)]
    {
        let properties: serde_json::Map<String, Value> = (0..optional)
            .map(|index| {
                (
                    format!("optional_{index}"),
                    serde_json::json!({"type": ["string", "null"]}),
                )
            })
            .collect();
        let mut schema = serde_json::json!({"type": "object", "properties": properties});
        if branches > 0 {
            schema["anyOf"] = Value::Array(
                (0..branches)
                    .map(|_| serde_json::json!({"type": "string"}))
                    .collect(),
            );
        }
        let mut request = parameter_request();
        request.tools = Some(vec![
            canonical_tool(
                "a",
                serde_json::json!({"type": "object", "properties": {}}),
                Some(true),
            ),
            canonical_tool("b", schema.clone(), Some(true)),
        ]);
        let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
        assert_eq!(
            body["tools"][0].get("strict").and_then(Value::as_bool),
            expected_strict.then_some(true)
        );
        assert_eq!(
            body["tools"][1].get("strict").and_then(Value::as_bool),
            expected_strict.then_some(true)
        );
        assert_eq!(body["tools"][1]["input_schema"], schema);
    }
    // Neither schema individually exceeds 24 optional parameters.
    let mut request = parameter_request();
    let properties: serde_json::Map<String, Value> = (0..13)
        .map(|index| {
            (
                format!("optional_{index}"),
                serde_json::json!({"type": "string"}),
            )
        })
        .collect();
    let schema = serde_json::json!({"type": "object", "properties": properties});
    request.tools = Some(vec![canonical_tool("a", schema.clone(), Some(true))]);
    request.response_format = Some(
        stravia_runtime_contract::protocol::ir::ResponseFormat::JsonSchema {
            name: "answer".into(),
            description: None,
            schema: schema.clone(),
            strict: None,
        },
    );
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert!(body["tools"][0].get("strict").is_none());
    assert_eq!(body["output_config"]["format"]["schema"], schema);
}

#[test]
fn strict_tool_count_limit_includes_only_supported_native_tools() {
    use stravia_runtime_contract::protocol::ir::{ImageGenerationTool, WebSearchTool};
    for (function_count, strict) in [(19, true), (20, false)] {
        let mut request = parameter_request();
        request.tools = Some(
            (0..function_count)
                .map(|index| {
                    canonical_tool(
                        format!("tool_{index}"),
                        serde_json::json!({"type": "object"}),
                        Some(true),
                    )
                })
                .collect(),
        );
        request.parameters.native_tools = vec![
            NativeTool::WebSearch(WebSearchTool::default()),
            NativeTool::ImageGeneration(ImageGenerationTool::default()),
        ];
        let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), function_count + 1);
        assert_eq!(
            body["tools"][0].get("strict").and_then(Value::as_bool),
            strict.then_some(true)
        );
    }
}

#[test]
fn canonical_tools_and_controls_win_over_raw_tools_and_aliases() {
    let schema = serde_json::json!({"type": "object", "properties": {
        "minimum": {"type": ["integer", "null"], "minimum": 3}
    }});
    let mut request = parameter_request();
    request.tools = crate::codec::anthropic::messages::decoder::AnthropicDecoder
        .decode_request(serde_json::json!({
            "model": "model", "max_tokens": 1024,
            "messages": [{"role": "user", "content": "read"}],
            "tools": [{
                "type": "custom", "name": "read", "input_schema": schema,
                "strict": false, "defer_loading": false, "eager_input_streaming": false,
                "citations": {"enabled": false},
                "cache_control": {"type": "ephemeral", "ttl": "5m"},
                "allowed_callers": ["direct"],
                "input_examples": [{"minimum": 3, "private_parameter": "business"}]
            }]
        }))
        .unwrap()
        .tools;
    request.parallel_tool_calls = Some(true);
    request.generation.stop = Some(vec![]);
    request.parameters.top_k = Some(0);
    request.parameters.end_user_id = Some(String::new());
    request.parameters.service_tier = Some(ServiceTier::Default);
    request.meta.vendor.ingress.extend([
        (
            "__anthropic_raw_tools".into(),
            serde_json::json!([
                {"name": "read", "description": "stale", "strict": true,
                 "defer_loading": true, "eager_input_streaming": true,
                 "citations": {"enabled": true}, "allowed_callers": ["code_execution_20260120"],
                 "input_examples": [{"minimum": 99}],
                 "input_schema": {"type": "string"},
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"name": "deleted", "input_schema": {}}
            ]),
        ),
        ("__anthropic_top_k".into(), serde_json::json!(80)),
        ("__anthropic_container".into(), serde_json::json!("bag")),
        (
            "__anthropic_context_management".into(),
            serde_json::json!({"edits": []}),
        ),
        ("__anthropic_service_tier".into(), serde_json::json!("auto")),
        (
            "__anthropic_stop_sequences".into(),
            serde_json::json!(["stale"]),
        ),
        (
            "__anthropic_metadata".into(),
            serde_json::json!({"user_id": "stale", "unknown": true}),
        ),
    ]);
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["tools"][0]["input_schema"], schema);
    assert_eq!(body["tools"][0]["strict"], false);
    assert_eq!(body["tools"][0]["defer_loading"], false);
    for field in [
        "eager_input_streaming",
        "citations",
        "allowed_callers",
        "input_examples",
        "cache_control",
        "description",
    ] {
        assert!(body["tools"][0].get(field).is_none(), "{field}");
    }
    assert_eq!(body["top_k"], 0);
    assert!(body.get("context_management").is_none());
    assert!(body.get("container").is_none());
    assert!(body.get("inference_geo").is_none());
    assert_eq!(body["metadata"], serde_json::json!({"user_id": ""}));
    assert_eq!(body["service_tier"], "standard_only");
    assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], false);
    assert_eq!(body["stop_sequences"], serde_json::json!([]));
}

#[test]
fn unsupported_native_variants_and_service_tiers_are_omitted() {
    use stravia_runtime_contract::protocol::ir::{ImageGenerationTool, WebSearchTool};
    let mut request = parameter_request();
    request.parameters.service_tier = Some(ServiceTier::Priority);
    request.parameters.native_tools = vec![
        NativeTool::WebSearch(WebSearchTool {
            max_uses: Some(0),
            ..Default::default()
        }),
        NativeTool::ImageGeneration(ImageGenerationTool::default()),
    ];

    request.parameters.metadata = Some(std::collections::BTreeMap::from([(
        "user_id".into(),
        "not-an-anthropic-user".into(),
    )]));
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert!(body.get("service_tier").is_none());
    assert!(body.get("unknown").is_none());
    assert!(body.get("metadata").is_none());
    assert_eq!(
        body["tools"],
        serde_json::json!([{
            "type": "web_search_20250305", "name": "web_search", "max_uses": 0
        }])
    );
}

#[test]
fn canonical_reasoning_modes_and_levels_project_without_target_controls() {
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
    for (source, mode, expected) in [
        (
            serde_json::json!({"type": "adaptive"}),
            "auto",
            serde_json::json!({"type": "adaptive"}),
        ),
        (
            serde_json::json!({"type": "enabled", "budget_tokens": 2048}),
            "enabled",
            serde_json::json!({"type": "enabled", "budget_tokens": 2048}),
        ),
        (
            serde_json::json!({"type": "disabled"}),
            "disabled",
            serde_json::json!({"type": "disabled"}),
        ),
    ] {
        let request = super::super::decoder::AnthropicDecoder
            .decode_request(serde_json::json!({
                "model": "claude", "max_tokens": 4096,
                "messages": [{"role": "user", "content": "hello"}], "thinking": source
            }))
            .unwrap();
        assert_eq!(request.reasoning.mode.as_deref(), Some(mode));
        let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
        assert_eq!(body["thinking"], expected);
        assert!(
            body.get("output_config")
                .and_then(|config| config.get("effort"))
                .is_none(),
            "omitted effort must preserve the model default"
        );
    }
    let mut request = parameter_request();
    for (level, effort) in [
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::Xhigh, "xhigh"),
    ] {
        request.reasoning.level = Some(level);
        let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], effort);
    }
    request.generation.max_tokens = Some(2048);
    request.reasoning.mode = Some("enabled".into());
    request.reasoning.budget_tokens = Some(50_000);
    let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
    assert_eq!(body["thinking"]["budget_tokens"], 2047);
    request.reasoning.budget_tokens = Some(0);
    let (body, _) = AnthropicEncoder.encode_request(request.clone()).unwrap();
    assert_eq!(body["thinking"]["budget_tokens"], 1024);
    request.reasoning.target_control = Some(TargetThinkingControl::Disabled);
    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();
    assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
}

#[test]
fn enabled_thinking_without_output_limit_preserves_reasoning_budget() {
    let mut request = parameter_request();
    request.reasoning.mode = Some("enabled".into());
    request.reasoning.budget_tokens = Some(50_000);

    let (body, _) = AnthropicEncoder.encode_request(request).unwrap();

    assert_eq!(body["thinking"]["budget_tokens"], 50_000);
}

#[test]
fn enabled_thinking_rejects_an_impossible_generation_budget() {
    let mut request = parameter_request();
    request.generation.max_tokens = Some(1024);
    request.reasoning.mode = Some("enabled".into());
    assert!(AnthropicEncoder.encode_request(request).is_err());
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
fn signed_thinking_keeps_native_thinking_block_with_signature() {
    let request = assistant_blocks_request(vec![
        ContentBlock::Thinking {
            thinking: "visible reasoning".into(),
            signature: Some("opaque-sig".into()),
        },
        ContentBlock::Text {
            text: "answer".to_owned().into(),
            cache_control: None,
        },
    ]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content[0],
        serde_json::json!({
            "type": "thinking",
            "thinking": "visible reasoning",
            "signature": "opaque-sig",
        })
    );
    assert_eq!(
        content[1],
        serde_json::json!({"type": "text", "text": "answer"})
    );
}

#[test]
fn unsigned_thinking_degrades_to_text_block() {
    let request = assistant_blocks_request(vec![ContentBlock::Thinking {
        thinking: "unsigned reasoning".into(),
        signature: None,
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    // Anthropic 上游拒绝无签名 thinking 块，明文只能出现在 text 块里。
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [serde_json::json!({"type": "text", "text": "unsigned reasoning"})]
    );
}

#[test]
fn thinking_replay_responses_ciphertext_degrades_per_readable_segment() {
    let request = assistant_blocks_request(vec![ContentBlock::Reasoning {
        summary: vec!["summary-one".into(), "summary-two".into()],
        content: vec!["detail-one".into(), "detail-two".into()],
        encrypted_content: Some("ciphertext".into()),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");
    assert_eq!(
        body["messages"][1]["content"],
        serde_json::json!([
            {"type": "text", "text": "summary-one"},
            {"type": "text", "text": "summary-two"},
            {"type": "text", "text": "detail-one"},
            {"type": "text", "text": "detail-two"}
        ])
    );
}

#[test]
fn unencrypted_reasoning_degrades_to_text_blocks_per_segment() {
    let request = assistant_blocks_request(vec![ContentBlock::Reasoning {
        summary: vec!["summary".into(), String::new()],
        content: vec!["detail".into()],
        encrypted_content: None,
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [
            serde_json::json!({"type": "text", "text": "summary"}),
            serde_json::json!({"type": "text", "text": "detail"}),
        ]
    );
}

#[test]
fn redacted_thinking_keeps_native_redacted_block() {
    let request = assistant_blocks_request(vec![ContentBlock::RedactedThinking {
        data: "redacted-data".into(),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [serde_json::json!({"type": "redacted_thinking", "data": "redacted-data"})]
    );
}

#[test]
fn meta_reasoning_without_signature_degrades_to_text() {
    let request = AiRequest::new(
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
                content: MessageContent::Text("answer".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: Some(
                    stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
                        serde_json::json!({"reasoning_content": "unsigned reasoning"}),
                    ),
                ),
            },
        ],
    );

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [
            serde_json::json!({"type": "text", "text": "unsigned reasoning"}),
            serde_json::json!({"type": "text", "text": "answer"}),
        ]
    );
    assert!(!body.to_string().contains("\"thinking\""));
}

#[test]
fn blocks_item_with_tool_calls_keeps_calls_and_degrades_unsigned_thinking() {
    let mut request = assistant_blocks_request(vec![
        ContentBlock::Thinking {
            thinking: "unsigned reasoning".into(),
            signature: None,
        },
        ContentBlock::Text {
            text: "answer".to_owned().into(),
            cache_control: None,
        },
    ]);
    request.items[1].tool_calls = Some(vec![stravia_runtime_contract::protocol::ir::ToolCall {
        id: "call_1".into(),
        name: "glob".into(),
        arguments: "{}".into(),
    }]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    // thinking 明文降级为 text，tool_calls 以 tool_use 块补齐在末尾。
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        content.as_slice(),
        [
            serde_json::json!({"type": "text", "text": "unsigned reasoning"}),
            serde_json::json!({"type": "text", "text": "answer"}),
            serde_json::json!({"type": "tool_use", "id": "call_1", "name": "glob", "input": {}}),
        ]
    );
}

#[test]
fn consumed_system_items_preserve_newlines_and_concatenate_only_text_blocks() {
    let item = |role, content| AiItem {
        role,
        content,
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut request = AiRequest::new(
        "model",
        vec![
            item(Role::System, MessageContent::Text("系统".to_owned().into())),
            item(
                Role::Developer,
                MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "first".to_owned().into(),
                        cache_control: None,
                    },
                    ContentBlock::Thinking {
                        thinking: "not system text".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "second".to_owned().into(),
                        cache_control: None,
                    },
                ]),
            ),
            item(Role::System, MessageContent::Text(String::new().into())),
            item(Role::User, MessageContent::Text("hello".to_owned().into())),
        ],
    );
    request.instructions = Some("instructions".into());

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(body["system"], "instructions\n系统\nfirstsecond\n");
    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "hello"}]}
        ])
    );
}

#[test]
fn owned_normalization_merges_text_and_blocks_without_changing_shared_history() {
    let mut first = AiItem::output_text("first");
    first.role = Role::User;
    let mut second = AiItem::output_text("");
    second.role = Role::User;
    second.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: " \n ".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::Text {
            text: "second".to_owned().into(),
            cache_control: None,
        },
    ]);
    let request = AiRequest::new("model", vec![first, second]);
    let preserved = request.clone();

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role": "user", "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ]}
        ])
    );
    assert_eq!(preserved.items[0].content.to_text(), "first");
    assert_eq!(preserved.items[1].content.to_text(), " \n second");
}

#[test]
fn owned_raw_system_messages_keep_cache_controls_but_raw_tools_are_not_authoritative() {
    let mut request = AiRequest::new("model", vec![AiItem::output_text("ignored")]);
    request.instructions = Some("ignored instructions".into());
    let system = serde_json::json!([
        {"type": "text", "text": "native system", "cache_control": {"type": "ephemeral"}}
    ]);
    let messages = serde_json::json!([
        {"role": "user", "content": [
            {"type": "text", "text": "native message", "cache_control": {"type": "ephemeral"}}
        ]}
    ]);
    let tools = serde_json::json!([
        {"name": "native_tool", "input_schema": {"type": "object"},
         "cache_control": {"type": "ephemeral"}}
    ]);
    request
        .meta
        .vendor
        .ingress
        .insert("__anthropic_raw_system".into(), system.clone());
    request
        .meta
        .vendor
        .ingress
        .insert("__anthropic_raw_messages".into(), messages.clone());
    request
        .meta
        .vendor
        .ingress
        .insert("__anthropic_raw_tools".into(), tools.clone());

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(body["system"], system);
    assert_eq!(body["messages"], messages);
    assert!(body.get("tools").is_none());
}

#[test]
fn consuming_instruction_blocks_still_reserves_their_supplied_tool_ids() {
    let mut instruction = AiItem::output_text("");
    instruction.role = Role::System;
    instruction.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "instruction".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::ToolUse {
            id: "tc_1".into(),
            name: "reserved".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    ]);
    let call = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
        id: "".into(),
        name: "actual".into(),
        arguments: "{}".into(),
    });
    let request = AiRequest::new("model", vec![instruction, call]);

    let (body, _) = AnthropicEncoder.encode_request(request).expect("encode");

    assert_eq!(body["system"], "instruction");
    assert_eq!(body["messages"][0]["content"][0]["id"], "tc_2");
}

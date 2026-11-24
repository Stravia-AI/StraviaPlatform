use super::*;

#[test]
fn claude_native_thinking_has_model_and_output_cap_gates() {
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

    let mut request = BedrockConverseV1
        .decode_request(json!({
            "modelId": "anthropic.claude-3-7-sonnet-20250219-v1:0",
            "messages": [],
            "inferenceConfig": {"maxTokens": 4096, "temperature": 0, "topP": 0.5},
            "additionalModelRequestFields": {
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "low"},
                "top_k": 0,
                "unconfirmed": true
            }
        }))
        .unwrap();
    assert_eq!(request.reasoning.level, Some(ThinkingLevel::Low));
    request.reasoning.target_control = Some(TargetThinkingControl::Effort {
        value: "low".into(),
    });
    let (body, _) = BedrockConverseV1.encode_request(request.clone()).unwrap();
    assert_eq!(
        body["additionalModelRequestFields"]["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    assert!(
        body["additionalModelRequestFields"]
            .get("unconfirmed")
            .is_none()
    );
    assert!(body["additionalModelRequestFields"].get("top_k").is_none());
    assert!(body["inferenceConfig"].get("temperature").is_none());
    assert!(body["inferenceConfig"].get("topP").is_none());
    request.generation.max_tokens = Some(1024);
    assert!(BedrockConverseV1.encode_request(request.clone()).is_err());
    request.model = "amazon.nova-pro-v1:0".into();
    assert!(
        BedrockConverseV1
            .encode_request(request)
            .unwrap()
            .0
            .get("additionalModelRequestFields")
            .is_none()
    );
}

#[test]
fn bedrock_native_thinking_decodes_adaptive_and_disabled_without_legacy_budget() {
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

    let adaptive = BedrockConverseV1
        .decode_request(json!({
            "modelId": "anthropic.claude-opus-4-6-v1",
            "messages": [],
            "additionalModelRequestFields": {"thinking": {"type": "adaptive"}}
        }))
        .unwrap();
    assert_eq!(adaptive.reasoning.level, Some(ThinkingLevel::High));
    let (body, _) = BedrockConverseV1.encode_request(adaptive).unwrap();
    assert_eq!(
        body["additionalModelRequestFields"]["thinking"],
        json!({"type": "adaptive"})
    );
    assert_eq!(
        body["additionalModelRequestFields"]["output_config"]["effort"],
        "high"
    );

    let mut disabled = BedrockConverseV1
        .decode_request(json!({
            "modelId": "anthropic.claude-opus-4-6-v1",
            "messages": [],
            "additionalModelRequestFields": {"thinking": {"type": "disabled"}}
        }))
        .unwrap();
    assert_eq!(disabled.reasoning.level, Some(ThinkingLevel::Off));
    disabled.reasoning.target_control = Some(TargetThinkingControl::Effort {
        value: "none".into(),
    });
    let (body, _) = BedrockConverseV1.encode_request(disabled).unwrap();
    assert_eq!(
        body["additionalModelRequestFields"]["thinking"],
        json!({"type": "disabled"})
    );

    assert!(
        BedrockConverseV1
            .decode_request(json!({
                "modelId": "anthropic.claude-3-7-sonnet-20250219-v1:0",
                "messages": [],
                "additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": 1024}}
            }))
            .is_err()
    );
}

#[test]
fn bedrock_cache_points_keep_canonical_breakpoints_and_ttl() {
    let request = BedrockConverseV1
        .decode_request(json!({
            "modelId": "model",
            "system": [{"text": "system"}, {"cachePoint": {"type": "default", "ttl": "1h"}}],
            "messages": [{"role": "user", "content": [
                {"text": "hello"}, {"cachePoint": {"type": "default", "ttl": "5m"}}
            ]}]
        }))
        .unwrap();
    assert!(request.instructions.is_none());
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(
        body["system"][1]["cachePoint"],
        json!({"type": "default", "ttl": "1h"})
    );
    assert_eq!(
        body["messages"][0]["content"][1]["cachePoint"],
        json!({"type": "default", "ttl": "5m"})
    );
    assert!(
        BedrockConverseV1
            .decode_request(json!({
                "messages": [{"role": "user", "content": [
                    {"text": "hello"}, {"cachePoint": {"type": "default", "ttl": "unknown"}}
                ]}]
            }))
            .is_err()
    );
}

#[test]
fn bedrock_tools_retain_schema_null_and_choice_but_drop_tool_cache() {
    let request = BedrockConverseV1.decode_request(json!({
        "modelId": "model", "messages": [],
        "toolConfig": {
            "tools": [
                {"toolSpec": {"name": "lookup", "inputSchema": {"json": {"type": "object", "properties": {"value": {"type": ["string", "null"]}}}}}},
                {"cachePoint": {"type": "default", "ttl": "1h"}}
            ],
            "toolChoice": {"tool": {"name": "lookup"}}
        }
    })).unwrap();
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(
        body["toolConfig"]["toolChoice"],
        json!({"tool": {"name": "lookup"}})
    );
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["properties"]["value"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(body["toolConfig"]["tools"].as_array().unwrap().len(), 1);
}

#[test]
fn bedrock_metadata_is_string_only_and_unsupported_controls_are_dropped() {
    let mut request = BedrockConverseV1.decode_request(json!({
        "modelId": "model", "messages": [],
        "requestMetadata": {"caller": "test", "number": 1, "flag": true, "null": null, "nested": {"secret": "drop"}},
        "guardrailConfig": {"guardrailIdentifier": "guard", "guardrailVersion": "1", "trace": "enabled", "unknown": true, "streamProcessingMode": "async"},
        "performanceConfig": {"latency": "optimized", "unknown": true},
        "serviceTier": {"type": "priority", "unknown": true},
        "promptVariables": {"user": {"text": "hello", "unknown": true}},
        "outputConfig": {"textFormat": {"type": "json_schema", "structure": {"jsonSchema": {
            "schema": "{\"type\":\"object\",\"properties\":{\"unknown\":{\"type\":\"string\"}}}",
            "name": "response", "unknown": true
        }, "unknown": true}, "unknown": true}, "unknown": true},
        "additionalModelResponseFieldPaths": [],
        "additionalModelRequestFields": {"unconfirmed": true}
    })).unwrap();
    assert_eq!(
        request.parameters.metadata,
        Some(BTreeMap::from([("caller".into(), "test".into())]))
    );
    request.parameters.top_k = Some(0);
    request.parameters.store = Some(false);
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(body["requestMetadata"], json!({"caller": "test"}));
    for field in [
        "guardrailConfig",
        "performanceConfig",
        "promptVariables",
        "additionalModelResponseFieldPaths",
    ] {
        assert!(body.get(field).is_none(), "{field} must not be forwarded");
    }
    assert_eq!(body["serviceTier"], json!({"type": "priority"}));
    let definition = &body["outputConfig"]["textFormat"]["structure"]["jsonSchema"];
    assert_eq!(definition["name"], "response");
    assert_eq!(definition.as_object().unwrap().len(), 2);
    assert_eq!(
        serde_json::from_str::<Value>(definition["schema"].as_str().unwrap()).unwrap(),
        json!({"type": "object", "properties": {"unknown": {"type": "string"}}})
    );
    assert!(body.get("additionalModelRequestFields").is_none());
    assert!(body.get("top_k").is_none());
    assert!(body.get("store").is_none());
}

#[test]
fn bedrock_null_optional_controls_are_treated_as_absent() {
    for body in [
        json!({
            "messages": [], "system": null,
            "outputConfig": null, "requestMetadata": null,
            "serviceTier": null, "toolConfig": null,
            "additionalModelRequestFields": null
        }),
        json!({
            "messages": [],
            "outputConfig": {"textFormat": null},
            "toolConfig": {"tools": null, "toolChoice": null},
            "additionalModelRequestFields": {"thinking": null}
        }),
    ] {
        let request = BedrockConverseV1.decode_request(body).unwrap();
        assert!(request.items.is_empty());
        assert!(request.tools.is_none());
        assert!(request.tool_choice.is_none());
        assert!(request.response_format.is_none());
        assert!(request.parameters.metadata.is_none());
        assert!(request.parameters.service_tier.is_none());
        assert!(request.reasoning.level.is_none());
        let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
        for field in [
            "system",
            "outputConfig",
            "requestMetadata",
            "serviceTier",
            "toolConfig",
            "additionalModelRequestFields",
        ] {
            assert!(body.get(field).is_none(), "{field} must remain absent");
        }
    }
}

#[test]
fn bedrock_explicit_null_resets_only_the_matching_inherited_controls() {
    let request = BedrockConverseV1
        .decode_request(json!({
            "messages": [],
            "outputConfig": null,
            "serviceTier": null,
            "inferenceConfig": {"temperature": null, "topP": null, "maxTokens": null},
            "additionalModelRequestFields": {"thinking": null}
        }))
        .unwrap();
    for reset in [
        ParameterReset::ResponseFormat,
        ParameterReset::ServiceTier,
        ParameterReset::Temperature,
        ParameterReset::TopP,
        ParameterReset::MaxTokens,
        ParameterReset::Reasoning,
    ] {
        assert!(request.parameters.resets(reset));
    }
    let absent = BedrockConverseV1
        .decode_request(json!({"messages": []}))
        .unwrap();
    assert!(absent.parameters.resets.is_empty());
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(body, json!({"messages": []}));
}

#[test]
fn bedrock_service_tiers_only_map_shared_official_values() {
    for (wire, tier) in [
        ("default", ServiceTier::Default),
        ("flex", ServiceTier::Flex),
        ("priority", ServiceTier::Priority),
    ] {
        let request = BedrockConverseV1
            .decode_request(json!({
                "messages": [], "serviceTier": {"type": wire, "unknown": true}
            }))
            .unwrap();
        assert_eq!(request.parameters.service_tier, Some(tier));
        let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
        assert_eq!(body["serviceTier"], json!({"type": wire}));
    }
    for wire in ["auto", "reserved", "unknown"] {
        let request = BedrockConverseV1
            .decode_request(json!({"messages": [], "serviceTier": {"type": wire}}))
            .unwrap();
        assert!(request.parameters.service_tier.is_none());
        let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
        assert!(body.get("serviceTier").is_none());
    }
    let mut request = AiRequest::new("model", vec![]);
    request.parameters.service_tier = Some(ServiceTier::Auto);
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert!(body.get("serviceTier").is_none());
}

#[test]
fn bedrock_projects_unified_metadata_without_vendor_specific_state() {
    let mut request = AiRequest::new("model", vec![]);
    request.parameters.metadata = Some(BTreeMap::from([("caller".into(), "canonical".into())]));
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(body["requestMetadata"], json!({"caller": "canonical"}));
}

#[test]
fn bedrock_omits_unrepresentable_tool_choices_and_native_tools() {
    use stravia_runtime_contract::protocol::ir::{NativeTool, NativeToolKind, WebSearchTool};

    for choice in [
        ToolChoice::Allowed {
            names: vec!["lookup".into()],
            required: true,
        },
        ToolChoice::Native(NativeToolKind::WebSearch),
    ] {
        let mut request = BedrockConverseV1
            .decode_request(json!({
                "messages": [],
                "toolConfig": {"tools": [
                    {"toolSpec": {"name": "lookup", "inputSchema": {"json": {"type": "object"}}}}
                ]}
            }))
            .unwrap();
        request.tool_choice = Some(choice);
        request.parameters.native_tools = vec![NativeTool::WebSearch(WebSearchTool {
            max_uses: None,
            allowed_domains: vec![],
            blocked_domains: vec![],
            user_location: None,
            context_size: None,
        })];
        let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
        assert!(body["toolConfig"].get("toolChoice").is_none());
        assert_eq!(body["toolConfig"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "lookup");
    }
}

#[test]
fn bedrock_native_structured_output_normalizes_to_one_canonical_format() {
    let schema = json!({
        "type": "object",
        "properties": {"business": {"type": "object", "properties": {"unknown": {"type": "string"}}}}
    });
    let request = BedrockConverseV1.decode_request(json!({
        "modelId": "model", "messages": [],
        "outputConfig": {"unknown": true, "textFormat": {
            "type": "json_schema", "unknown": true,
            "structure": {"unknown": true, "jsonSchema": {
                "name": "native", "description": "native description",
                "schema": serde_json::to_string(&schema).unwrap(), "unknown": true, "strict": true
            }}
        }}
    })).unwrap();
    let Some(ResponseFormat::JsonSchema {
        name,
        description,
        schema: decoded_schema,
        strict,
    }) = &request.response_format
    else {
        panic!("native output schema must normalize to canonical JSON schema");
    };
    assert_eq!(name, "native");
    assert_eq!(decoded_schema, &schema);
    assert!(strict.is_none());
    assert_eq!(description.as_deref(), Some("native description"));
}

#[test]
fn bedrock_projects_cross_protocol_canonical_schema_without_native_output_config() {
    let schema =
        json!({"type": "object", "properties": {"preserve": {"type": ["string", "null"]}}});
    let mut request = AiRequest::new("model", vec![]);
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "canonical".into(),
        description: Some("canonical description".into()),
        schema: schema.clone(),
        strict: Some(true),
    });
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    let definition = &body["outputConfig"]["textFormat"]["structure"]["jsonSchema"];
    assert_eq!(body["outputConfig"]["textFormat"]["type"], "json_schema");
    assert_eq!(definition["name"], "canonical");
    assert_eq!(definition["description"], "canonical description");
    assert!(definition.get("strict").is_none());
    assert_eq!(
        serde_json::from_str::<Value>(definition["schema"].as_str().unwrap()).unwrap(),
        schema
    );
}

#[test]
fn bedrock_omits_output_formats_without_a_native_equivalent() {
    for format in [ResponseFormat::Text, ResponseFormat::JsonObject] {
        let mut request = AiRequest::new("model", vec![]);
        request.response_format = Some(format);
        let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
        assert!(body.get("outputConfig").is_none());
    }
}

#[test]
fn projects_nested_tool_results_to_native_carriers() {
    use stravia_runtime_contract::protocol::ir::ToolResultContentKind;
    let mixed = json!([
        {"type": "text", "text": "keep"},
        {"type": "input_text", "text": " more"},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aW1n"}},
        {"type": "unknown", "text": "drop"},
        {"type": "image", "source": {"type": "url", "url": "https://example.com/image.png"}}
    ]);
    let result = |kind, content| ContentBlock::ToolResult {
        tool_use_id: "call_1".into(),
        content_kind: Some(kind),
        content,
        is_error: Some(true),
        cache_control: None,
    };
    for role in [Role::User, Role::Tool] {
        let item = AiItem {
            role,
            content: MessageContent::Blocks(vec![result(
                ToolResultContentKind::ContentBlocks,
                mixed.clone(),
            )]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        };
        let (body, _) = BedrockConverseV1
            .encode_request(AiRequest::new("model", vec![item]))
            .unwrap();
        let output = &body["messages"][0]["content"][0]["toolResult"];
        assert_eq!(output["toolUseId"], "call_1");
        assert_eq!(output["status"], "error");
        assert_eq!(
            output["content"],
            json!([
                {"text": "keep"}, {"text": " more"},
                {"image": {"format": "png", "source": {"bytes": "aW1n"}}}
            ])
        );
        let item = AiItem {
            role,
            content: MessageContent::Blocks(vec![result(
                ToolResultContentKind::Json,
                mixed.clone(),
            )]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        };
        let (body, _) = BedrockConverseV1
            .encode_request(AiRequest::new("model", vec![item]))
            .unwrap();
        assert_eq!(
            body["messages"][0]["content"][0]["toolResult"]["content"],
            json!([{"json": mixed}])
        );
    }
}

#[test]
fn replays_native_bedrock_tool_result_blocks_without_json_wrapping() {
    let content = json!([
        {"text": "keep"}, {"json": [{"type": "image", "business": true}]},
        {"image": {"format": "png", "source": {"bytes": "aW1n"}}}
    ]);
    let request = BedrockConverseV1.decode_request(json!({"modelId": "model", "messages": [
        {"role": "user", "content": [{"toolResult": {"toolUseId": "call_1", "content": content, "status": "error"}}]}
    ]})).unwrap();
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(
        body["messages"][0]["content"][0]["toolResult"]["content"],
        content
    );
    assert_eq!(
        body["messages"][0]["content"][0]["toolResult"]["status"],
        "error"
    );
}

#[test]
fn omits_unrepresentable_nested_bedrock_results() {
    use stravia_runtime_contract::protocol::ir::ToolResultContentKind;
    for role in [Role::User, Role::Tool] {
        let item = AiItem {
            role,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content_kind: Some(ToolResultContentKind::ContentBlocks),
                content: json!([{"type": "unknown", "text": "drop"}]),
                is_error: Some(true),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        };
        let (body, _) = BedrockConverseV1
            .encode_request(AiRequest::new("model", vec![item]))
            .unwrap();
        assert_eq!(body["messages"], json!([]));
    }
}

#[test]
fn preserves_native_document_bytes_text_names_and_context() {
    let mut item = user_item("unused");
    item.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "read attachments".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::Document {
            source: DocumentSource::Base64Pdf {
                data: "JVBERi0xLjQK".into(),
            },
            title: Some("Quarterly report [2026] (final)-1".into()),
            context: Some("Use the supplied totals".into()),
            cache_control: None,
        },
        ContentBlock::Document {
            source: DocumentSource::PlainText {
                data: "Plain document\nSecond line".into(),
            },
            title: Some("invalid.pdf".into()),
            context: Some("text context".into()),
            cache_control: None,
        },
        ContentBlock::Document {
            source: DocumentSource::Url("https://example.com/private.pdf".into()),
            title: None,
            context: None,
            cache_control: None,
        },
        ContentBlock::Audio {
            source: MediaSource::Url("https://example.com/audio.wav".into()),
        },
    ]);
    let (body, _) = BedrockConverseV1
        .encode_request(AiRequest::new("model", vec![item]))
        .unwrap();
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"text": "read attachments"},
            {"document": {"format": "pdf", "name": "Quarterly report [2026] (final)-1",
                "source": {"bytes": "JVBERi0xLjQK"}, "context": "Use the supplied totals"}},
            {"document": {"format": "txt", "name": "document",
                "source": {"text": "Plain document\nSecond line"}, "context": "text context"}}
        ]}])
    );
}

#[test]
fn encodes_known_inline_file_document_formats() {
    for (mime, format) in [
        ("application/pdf", "pdf"),
        ("text/csv", "csv"),
        ("application/msword", "doc"),
        (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
        ),
        ("application/vnd.ms-excel", "xls"),
        (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
        ),
        ("text/html", "html"),
        ("text/plain", "txt"),
        ("text/markdown", "md"),
    ] {
        for override_mime in [false, true] {
            let mut item = user_item("unused");
            item.content = MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "read".to_owned().into(),
                    cache_control: None,
                },
                ContentBlock::File {
                    source: MediaSource::Base64 {
                        media_type: if override_mime {
                            "application/octet-stream"
                        } else {
                            mime
                        }
                        .into(),
                        data: "AAEC/w==".into(),
                    },
                    media_type: override_mime.then(|| mime.to_owned()),
                },
                ContentBlock::File {
                    source: MediaSource::Base64 {
                        media_type: "application/zip".into(),
                        data: "eA==".into(),
                    },
                    media_type: None,
                },
            ]);
            let (body, _) = BedrockConverseV1
                .encode_request(AiRequest::new("model", vec![item]))
                .unwrap();
            assert_eq!(
                body["messages"][0]["content"],
                json!([
                    {"text": "read"},
                    {"document": {"format": format, "name": "document", "source": {"bytes": "AAEC/w=="}}}
                ])
            );
        }
    }
}

#[test]
fn keeps_only_official_document_names_without_rewriting() {
    for (title, expected) in [
        (None, "document"),
        (Some(""), "document"),
        (Some("two  spaces"), "document"),
        (Some("report.pdf"), "document"),
        (Some("report_name"), "document"),
        (Some("A [B] (C)-1"), "A [B] (C)-1"),
    ] {
        let mut item = user_item("unused");
        item.content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "read".to_owned().into(),
                cache_control: None,
            },
            ContentBlock::Document {
                source: DocumentSource::PlainText {
                    data: "body".into(),
                },
                title: title.map(str::to_owned),
                context: None,
                cache_control: None,
            },
        ]);
        let (body, _) = BedrockConverseV1
            .encode_request(AiRequest::new("model", vec![item]))
            .unwrap();
        let document = &body["messages"][0]["content"][1]["document"];
        assert_eq!(document["name"], expected);
        assert!(document.get("context").is_none());
    }
    assert!(bedrock_document_name_valid(&"a".repeat(200)));
    assert!(!bedrock_document_name_valid(&"a".repeat(201)));
}

#[test]
fn adds_only_required_empty_text_carrier_for_document_only_users() {
    for source in [
        DocumentSource::Base64Pdf {
            data: "JVBERg==".into(),
        },
        DocumentSource::PlainText {
            data: "actual body".into(),
        },
    ] {
        let mut item = user_item("unused");
        item.content = MessageContent::Blocks(vec![ContentBlock::Document {
            source,
            title: Some("attachment".into()),
            context: Some("context stays metadata".into()),
            cache_control: None,
        }]);
        let (body, _) = BedrockConverseV1
            .encode_request(AiRequest::new("model", vec![item.clone()]))
            .unwrap();
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["content"][1], json!({"text": ""}));
        item.role = Role::Assistant;
        let (body, _) = BedrockConverseV1
            .encode_request(AiRequest::new("model", vec![item]))
            .unwrap();
        assert_eq!(body["messages"], json!([]));
    }
}

#[test]
fn encodes_converse_tool_config_not_chat_completions() {
    let mut request = AiRequest::new(
        "anthropic.claude",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("Hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.tools = Some(vec![ToolSpec {
        name: "weather".into(),
        description: None,
        parameters: json!({"type":"object"}),
        strict: None,
        defer_loading: None,
    }]);
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "weather"
    );
    assert!(body.get("messages").is_some());
    assert!(body.get("choices").is_none());
}

fn user_item(text: &str) -> AiItem {
    AiItem {
        role: Role::User,
        content: MessageContent::Text(text.to_owned().into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

fn assistant_block(block: ContentBlock) -> AiItem {
    AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![block]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

#[test]
fn replays_signed_thinking_and_redacted_natively() {
    let request = AiRequest::new(
        "anthropic.claude",
        vec![
            user_item("hi"),
            assistant_block(ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: Some("sig_protected".into()),
            }),
            assistant_block(ContentBlock::RedactedThinking {
                data: "cmVkYWN0ZWQ=".into(),
            }),
            assistant_block(ContentBlock::Reasoning {
                summary: vec!["summary".into()],
                content: vec!["detail".into()],
                encrypted_content: Some("enc_protected".into()),
            }),
        ],
    );

    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[1]["content"][0],
        json!({"reasoningContent": {"reasoningText": {
            "text": "let me think",
            "signature": "sig_protected",
        }}})
    );
    assert_eq!(
        messages[2]["content"][0],
        json!({"reasoningContent": {"redactedContent": "cmVkYWN0ZWQ="}})
    );
    // encrypted_content 借 reasoningText.signature 原生承载。
    assert_eq!(
        messages[3]["content"][0],
        json!({"reasoningContent": {"reasoningText": {
            "text": "summary\ndetail",
            "signature": "enc_protected",
        }}})
    );
}

#[test]
fn downgrades_unsigned_reasoning_to_text_blocks() {
    // Claude on Bedrock 拒绝无签名 reasoningContent，明文只能走 text 块。
    let request = AiRequest::new(
        "anthropic.claude",
        vec![
            assistant_block(ContentBlock::Thinking {
                thinking: "plain thought".into(),
                signature: None,
            }),
            assistant_block(ContentBlock::Reasoning {
                summary: vec!["s1".into()],
                content: vec![String::new(), "c1".into()],
                encrypted_content: None,
            }),
        ],
    );

    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"], json!([{"text": "plain thought"}]));
    // 每段非空文本一个 text 块，保持原位置顺序。
    assert_eq!(
        messages[1]["content"],
        json!([{"text": "s1"}, {"text": "c1"}])
    );
}

#[test]
fn skips_empty_assistant_without_breaking_tool_pairing() {
    let with_call = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
            data: "cmVkYWN0ZWQ=".into(),
        }]),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            name: "weather".into(),
            arguments: "{}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new(
        "anthropic.claude",
        vec![
            user_item("a"),
            // 无签名且无明文：编码为空，整条跳过。
            assistant_block(ContentBlock::Thinking {
                thinking: String::new(),
                signature: None,
            }),
            // redacted 有原生载体，且 tool call 必须保留。
            with_call,
            AiItem::function_call_output("call_1", json!("sunny")),
            user_item("b"),
        ],
    );

    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"],
        json!([
            {"reasoningContent": {"redactedContent": "cmVkYWN0ZWQ="}},
            {"toolUse": {"toolUseId": "call_1", "name": "weather", "input": {}}},
        ])
    );
    assert_eq!(
        messages[2]["content"][0]["toolResult"]["toolUseId"],
        "call_1"
    );
    assert_eq!(messages[3]["content"], json!([{"text": "b"}]));
}

#[test]
fn omits_unsupported_controls_roles_and_noninline_images() {
    let mut system = user_item("system-only");
    system.role = Role::System;
    let mut developer = user_item("developer-only");
    developer.role = Role::Developer;
    let mut user = user_item("keep");
    user.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "keep".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::Image {
            source: MediaSource::Url("https://example.com/image.png".into()),
            detail: None,
            cache_control: None,
        },
        ContentBlock::Image {
            source: MediaSource::FileId {
                file_id: "file_1".into(),
                detail: None,
            },
            detail: None,
            cache_control: None,
        },
    ]);
    let mut request = AiRequest::new("model", vec![system, developer, user]);
    request.generation.frequency_penalty = Some(0.5);
    request.generation.presence_penalty = Some(0.5);
    request.generation.seed = Some(1);
    request.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::High);
    request.response_format =
        Some(stravia_runtime_contract::protocol::ir::ResponseFormat::JsonObject);
    request.tools = Some(vec![ToolSpec {
        name: "weather".into(),
        description: None,
        parameters: json!({"type": "object"}),
        strict: None,
        defer_loading: None,
    }]);
    request.tool_choice = Some(ToolChoice::Native(
        stravia_runtime_contract::protocol::ir::NativeToolKind::WebSearch,
    ));
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"text": "keep"}]}])
    );
    assert_eq!(
        body["system"],
        json!([{"text": "system-only"}, {"text": "developer-only"}])
    );
    assert!(body.get("inferenceConfig").is_none());
    assert!(body["toolConfig"].get("toolChoice").is_none());
}

#[test]
fn preserves_text_while_dropping_role_specific_unsupported_blocks() {
    let blocks = vec![
        ContentBlock::Text {
            text: "keep".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::Audio {
            source: MediaSource::Url("https://example.com/audio.wav".into()),
        },
    ];
    let mut item = user_item("unused");
    item.content = MessageContent::Blocks(blocks);
    assert_eq!(
        encode_user_content(&item.content).unwrap(),
        vec![json!({"text": "keep"})]
    );
    item.role = Role::Assistant;
    assert_eq!(
        encode_assistant_content(&item).unwrap(),
        vec![json!({"text": "keep"})]
    );
    item.role = Role::Tool;
    item.tool_call_id = Some("call_1".into());
    assert_eq!(
        encode_tool_result(&item).unwrap()[0]["toolResult"]["content"],
        json!([{"text": "keep"}])
    );
}

#[test]
fn drops_malformed_and_nonobject_calls_with_their_results() {
    for arguments in ["not-json", "[]", "null", "42", "\"text\""] {
        let mut assistant = assistant_block(ContentBlock::Thinking {
            thinking: String::new(),
            signature: None,
        });
        assistant.tool_calls = Some(vec![ToolCall {
            id: "bad".into(),
            name: "weather".into(),
            arguments: arguments.into(),
        }]);
        let request = AiRequest::new(
            "model",
            vec![
                assistant.clone(),
                AiItem::function_call_output("bad", json!("result")),
                user_item("keep"),
            ],
        );
        let body = stravia_protocol_codec::transform::ProtocolTransform::encode_request_with(
            &BedrockConverseV1,
            request,
        )
        .unwrap()
        .body;
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"text": "keep"}]}])
        );
        let mut response = AiResponse::new("response", "model");
        assistant.content = MessageContent::Blocks(Vec::new());
        response.items.push(assistant);
        assert_eq!(
            BedrockConverseV1.encode_response(&response)["output"]["message"]["content"],
            json!([])
        );
    }
}

#[test]
fn bedrock_legacy_claude_budget_reserves_output_buffer_below_max_tokens() {
    let mut request = AiRequest::new(
        "anthropic.claude-sonnet-4-5-20250929-v1:0",
        vec![user_item("think")],
    );
    request.generation.max_tokens = Some(4096);
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "high".into(),
        },
    );

    let body = stravia_protocol_codec::transform::ProtocolTransform::encode_request_with(
        &BedrockConverseV1,
        request,
    )
    .expect("encode")
    .body;
    assert_eq!(
        body["additionalModelRequestFields"]["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024})
    );

    let mut too_small = AiRequest::new(
        "anthropic.claude-sonnet-4-5-20250929-v1:0",
        vec![user_item("think")],
    );
    too_small.generation.max_tokens = Some(1024);
    too_small.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "low".into(),
        },
    );
    assert!(
        stravia_protocol_codec::transform::ProtocolTransform::encode_request_with(
            &BedrockConverseV1,
            too_small,
        )
        .is_err()
    );
}

#[test]
fn bedrock_always_on_and_between_tools_off_downgrades_strip_sampling_parameters() {
    for (model, expected_field) in [
        ("anthropic.claude-opus-5-5-v1:0", "output_config"),
        ("anthropic.claude-sonnet-5-5-v1:0", "thinking"),
    ] {
        let mut request = AiRequest::new(model, vec![user_item("brief")]);
        request.generation.temperature = Some(0.7);
        request.generation.top_p = Some(0.9);
        request.parameters.top_k = Some(40);
        request.reasoning.target_control = Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: "none".into(),
            },
        );
        let (body, _) = BedrockConverseV1.encode_request(request).expect("encode");
        assert!(body["inferenceConfig"].get("temperature").is_none());
        assert!(body["inferenceConfig"].get("topP").is_none());
        assert!(body["additionalModelRequestFields"].get("top_k").is_none());
        assert!(
            body["additionalModelRequestFields"]
                .get(expected_field)
                .is_some()
        );
    }
}

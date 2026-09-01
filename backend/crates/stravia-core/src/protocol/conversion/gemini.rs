use super::*;

#[test]
fn gemini_tool_result_correlation_success() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_abc".to_string(),
                name: "read_file".to_string(),
                arguments: "{\"path\":\"src/main.rs\"}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "read_file".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
    ];
    let mut ai_req = AiRequest::new("minimax-m2.7", messages);
    ai_req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    ai_req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    normalize_request_tool_results(&mut ai_req);
    assert_eq!(
        ai_req.items[1].tool_call_id.as_deref(),
        Some("call_abc"),
        "tool result should be correlated to previous assistant tool_call id"
    );
}
#[test]
fn gemini_tool_result_id_hint_matches_out_of_order_calls() {
    let messages = vec![
        AiItem {
            role: IrRole::Assistant,
            content: IrMessageContent::Text(String::new()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_a".to_string(),
                    name: "Glob".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    id: "call_b".to_string(),
                    name: "Bash".to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "call_b".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: IrRole::Tool,
            content: IrMessageContent::Blocks(vec![IrContentBlock::ToolResult {
                tool_use_id: "call_a".to_string(),
                content: serde_json::json!({"ok": true}),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
    ];
    let mut ai_req = AiRequest::new("minimax-m2.7", messages);
    ai_req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    ai_req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);

    normalize_request_tool_results(&mut ai_req);
    assert_eq!(ai_req.items[1].tool_call_id.as_deref(), Some("call_b"));
    assert_eq!(ai_req.items[2].tool_call_id.as_deref(), Some("call_a"));
}
#[test]
fn gemini_stream_formatter_keeps_tool_name_for_argument_deltas() {
    let mut fmt = GoogleStreamFormatter::new();
    let deltas = vec![
        IrStreamDelta::MessageStart {
            id: "x".to_string(),
            model: "m".to_string(),
        },
        IrStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".to_string(),
            name: "run_shell_command".to_string(),
        },
        IrStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"command\":\"ls -la\"}".to_string(),
        },
    ];
    let events = fmt.format_deltas(&deltas);
    let mut saw_named_call = false;
    let mut saw_command_arg = false;
    for ev in events {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&ev.data) else {
            continue;
        };
        let part = v
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .and_then(|arr| arr.first())
            .and_then(|p| p.get("functionCall"));
        if let Some(fc) = part {
            if fc.get("name").and_then(|n| n.as_str()) == Some("run_shell_command") {
                saw_named_call = true;
            }
            if fc
                .get("args")
                .and_then(|a| a.get("command"))
                .and_then(|c| c.as_str())
                == Some("ls -la")
            {
                saw_command_arg = true;
            }
        }
    }
    assert!(saw_named_call);
    assert!(saw_command_arg);
}
#[test]
fn gemini_stream_formatter_normalizes_common_tool_argument_aliases() {
    let mut fmt = GoogleStreamFormatter::new();
    let deltas = vec![
        IrStreamDelta::MessageStart {
            id: "x".to_string(),
            model: "m".to_string(),
        },
        IrStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".to_string(),
            name: "glob".to_string(),
        },
        IrStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{\"include_pattern\":\"**/*.py\",\"search_root\":\"/tmp/work\",\"exclude_pattern\":\"**/.venv/**\"}".to_string(),
        },
    ];
    let events = fmt.format_deltas(&deltas);
    let payload = events
        .iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .find_map(|v| {
            v.get("candidates")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
                .and_then(|arr| arr.first())
                .and_then(|p| p.get("functionCall"))
                .cloned()
        })
        .expect("functionCall payload");

    assert_eq!(payload.get("name").and_then(|v| v.as_str()), Some("glob"));
    let args = payload.get("args").expect("args object");
    assert_eq!(
        args.get("pattern").and_then(|v| v.as_str()),
        Some("**/*.py")
    );
    assert_eq!(
        args.get("root_dir").and_then(|v| v.as_str()),
        Some("/tmp/work")
    );
    assert_eq!(
        args.get("exclude_patterns")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str()),
        Some("**/.venv/**")
    );
}
#[test]
fn gemini_encoder_sanitizes_unsupported_json_schema_fields() {
    let messages = vec![AiItem {
        role: IrRole::User,
        content: IrMessageContent::Text("hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let tools = Some(vec![ToolSpec {
        name: "glob".to_string(),
        description: Some("glob files".to_string()),
        parameters: serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string"},
                "items": {
                    "type": "array",
                    "items": {
                        "$ref": "#/$defs/entry",
                        "ref": "legacy"
                    }
                }
            },
            "$defs": {
                "entry": {"type":"string"}
            }
        }),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let mut req = AiRequest::new("gemini-2.5-flash", messages);
    req.stream = StreamConfig {
        enabled: false,
        include_usage: false,
    };
    req.tools = tools;
    req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let (body, _) = GoogleEncoder.encode_request(&req).expect("encode");
    let params = body
        .get("tools")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("functionDeclarations"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("parameters"))
        .cloned()
        .expect("parameters");

    let rendered = params.to_string();
    assert!(!rendered.contains("$schema"));
    assert!(!rendered.contains("additionalProperties"));
    assert!(!rendered.contains("$ref"));
    assert!(!rendered.contains("\"ref\""));
    assert!(!rendered.contains("$defs"));
}
#[test]
fn gemini_file_data_round_trip_preserves_uri_and_mime_type() {
    use crate::protocol::codec::google::gemini::decoder::GoogleDecoder;

    // Simulate an inbound request with a PDF fileData part.
    let inbound = serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "fileData": {
                    "fileUri": "https://example.com/doc.pdf",
                    "mimeType": "application/pdf"
                }
            }]
        }]
    });

    // Decode to IR, then re-encode.
    let mut req = GoogleDecoder.decode_request(inbound).expect("decode");
    req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    let (outbound, _) = GoogleEncoder.encode_request(&req).expect("encode");

    let parts = outbound["contents"][0]["parts"].as_array().expect("parts");
    let fd = &parts[0]["fileData"];
    assert_eq!(
        fd["fileUri"].as_str(),
        Some("https://example.com/doc.pdf"),
        "fileUri must survive round-trip"
    );
    assert_eq!(
        fd["mimeType"].as_str(),
        Some("application/pdf"),
        "mimeType must survive round-trip"
    );
}
#[test]
fn gemini_decoder_file_data_routes_image_to_image_block() {
    use crate::protocol::codec::google::gemini::decoder::GoogleDecoder;

    let body = serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "fileData": {
                    "fileUri": "https://example.com/photo.jpg",
                    "mimeType": "image/jpeg"
                }
            }]
        }]
    });

    let decoder = GoogleDecoder;
    let req = decoder.decode_request(body).expect("decode");

    let msg = &req.items[0];
    match &msg.content {
        IrMessageContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 1);
            match &blocks[0] {
                IrContentBlock::Image { source, .. } => match source {
                    MediaSource::Url(url) => {
                        assert_eq!(url, "https://example.com/photo.jpg");
                    }
                    _ => panic!("expected MediaSource::Url"),
                },
                other => panic!("expected ContentBlock::Image for image/ mimeType, got {other:?}"),
            }
        }
        other => panic!("expected Blocks, got {other:?}"),
    }
}
#[test]
fn gemini_encoder_file_data_without_mime_type_omits_mime_type() {
    let messages = vec![AiItem {
        role: IrRole::User,
        content: IrMessageContent::Blocks(vec![IrContentBlock::File {
            source: MediaSource::Url("https://example.com/unknown.bin".into()),
            media_type: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let req = AiRequest::new("gemini-2.5-flash", messages);

    let (body, _) = GoogleEncoder.encode_request(&req).expect("encode");

    let parts = body["contents"][0]["parts"]
        .as_array()
        .expect("parts array");
    let fd = &parts[0]["fileData"];
    assert_eq!(
        fd["fileUri"].as_str(),
        Some("https://example.com/unknown.bin")
    );
    assert!(
        fd.get("mimeType").is_none(),
        "mimeType must be absent when media_type is None"
    );
}

// ── Claude Code >=2.1.154 mid-conversation system messages ────────────────────

/// Basic: inline system role is decoded as Role::System and kept at its position.
#[test]
fn gemini_encoder_file_data_with_mime_type_emits_mime_type() {
    let messages = vec![AiItem {
        role: IrRole::User,
        content: IrMessageContent::Blocks(vec![IrContentBlock::File {
            source: MediaSource::Url("https://example.com/report.pdf".into()),
            media_type: Some("application/pdf".into()),
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }];
    let req = AiRequest::new("gemini-2.5-flash", messages);

    let (body, _) = GoogleEncoder.encode_request(&req).expect("encode");

    let parts = body["contents"][0]["parts"]
        .as_array()
        .expect("parts array");
    assert_eq!(parts.len(), 1);
    let fd = &parts[0]["fileData"];
    assert_eq!(
        fd["fileUri"].as_str(),
        Some("https://example.com/report.pdf")
    );
    assert_eq!(fd["mimeType"].as_str(), Some("application/pdf"));
}

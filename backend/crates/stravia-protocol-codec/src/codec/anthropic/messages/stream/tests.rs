use super::*;
use crate::codec::open_responses::parser::ResponsesStreamParser;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;

fn assert_anthropic_thought_signature_stays_on_gemini_thought(split_chunks: bool) {
    use crate::codec::google::gemini::stream::GoogleStreamFormatter;

    let blocks = [
        make_sse_block(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_signed_tool","type":"message","role":"assistant","content":[],"model":"claude-3-7-sonnet","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        ),
        make_sse_block(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        ),
        make_sse_block(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Check the weather first."}}"#,
        ),
        make_sse_block(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"anthropic-thought-S"}}"#,
        ),
        make_sse_block(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        make_sse_block(
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_weather","name":"get_weather","input":{}}}"#,
        ),
        make_sse_block(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
        ),
        make_sse_block(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}"#,
        ),
        make_sse_block(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        make_sse_block(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":20}}"#,
        ),
        make_sse_block("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let chunks = if split_chunks {
        blocks.to_vec()
    } else {
        vec![blocks.concat()]
    };
    let mut parser = AnthropicStreamParser::new();
    let mut formatter = GoogleStreamFormatter::new();
    let mut parts = Vec::new();
    for chunk in chunks {
        let deltas = parser.parse_chunk(&chunk).expect("native Anthropic SSE");
        for event in formatter.format_deltas(&deltas) {
            let body: Value = serde_json::from_str(&event.data).expect("Gemini wire JSON");
            if let Some(wire_parts) = body["candidates"][0]["content"]["parts"].as_array() {
                parts.extend(wire_parts.iter().cloned());
            }
        }
    }
    assert_eq!(
        parts,
        vec![
            serde_json::json!({"text": "Check the weather first.", "thought": true}),
            serde_json::json!({"text": "", "thought": true, "thoughtSignature": "anthropic-thought-S"}),
            serde_json::json!({"functionCall": {"id": "toolu_weather", "name": "get_weather", "args": {"city": "Paris"}}}),
        ],
        "a completed signed thought must not donate its signature to the following call"
    );
}

#[test]
fn anthropic_signed_thought_then_tool_preserves_gemini_part_ownership() {
    assert_anthropic_thought_signature_stays_on_gemini_thought(false);
}

#[test]
fn anthropic_signed_thought_then_tool_preserves_gemini_part_ownership_across_chunks() {
    assert_anthropic_thought_signature_stays_on_gemini_thought(true);
}

fn make_sse_block(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

#[test]
fn stream_formatter_encodes_canonical_stream_error() {
    let mut formatter = AnthropicStreamFormatter::new();
    let events = formatter.format_deltas(&[AiStreamDelta::StreamError {
        error: stravia_runtime_contract::protocol::ir::AiError::new(
            stravia_runtime_contract::protocol::ir::AiErrorKind::StreamMidError,
            "stream aborted",
        ),
    }]);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("error"));
    let body: Value = serde_json::from_str(&events[0].data).expect("error JSON");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "stream_mid_error");
    assert_eq!(body["error"]["message"], "stream aborted");
}

#[test]
fn stream_formatter_preserves_raw_tool_arguments_and_same_batch_completion() {
    // Unlike unary tool_use.input, streaming partial_json is a string carrier:
    // even malformed or nonobject argument bytes remain representable verbatim.
    for fragments in [
        vec![r#"{"key":"value"}"#, " trailing invalid bytes"],
        vec!["not JSON"],
        vec!["[]"],
        vec!["null"],
        vec!["42"],
        vec![r#""scalar""#],
        vec![""],
        vec![r#"{"city":"#, r#""Paris"}"#],
    ] {
        let mut formatter = AnthropicStreamFormatter::new();
        let mut deltas = vec![
            AiStreamDelta::ThinkingDelta("signed thought".into()),
            AiStreamDelta::ThinkingSignature("source-signature".into()),
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "toolu_raw".into(),
                name: "raw_probe".into(),
            },
        ];
        deltas.extend(
            fragments
                .iter()
                .map(|arguments| AiStreamDelta::ToolCallDelta {
                    index: 0,
                    arguments: (*arguments).into(),
                }),
        );
        deltas.extend([
            AiStreamDelta::TextDelta("continued text".into()),
            AiStreamDelta::Usage(Usage {
                prompt_tokens: 17,
                completion_tokens: 9,
                ..Usage::default()
            }),
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ]);

        let events = formatter.format_deltas(&deltas);
        let payloads: Vec<Value> = events
            .iter()
            .map(|event| serde_json::from_str(&event.data).expect("valid native event JSON"))
            .collect();
        let raw_fragments: Vec<&str> = payloads
            .iter()
            .filter(|body| body["delta"]["type"] == "input_json_delta")
            .map(|body| {
                body["delta"]["partial_json"]
                    .as_str()
                    .expect("string carrier")
            })
            .collect();
        assert_eq!(
            raw_fragments, fragments,
            "raw argument bytes must not be projected"
        );
        let tool_start = payloads
            .iter()
            .find(|body| body["content_block"]["type"] == "tool_use")
            .expect("native tool block start");
        assert_eq!(tool_start["content_block"]["id"], "toolu_raw");
        assert_eq!(tool_start["content_block"]["name"], "raw_probe");
        assert_eq!(tool_start["content_block"]["input"], serde_json::json!({}));
        assert!(
            payloads
                .iter()
                .any(|body| body["delta"]["signature"] == "source-signature")
        );
        assert!(
            payloads
                .iter()
                .any(|body| body["delta"]["text"] == "continued text")
        );
        let completion = payloads
            .iter()
            .find(|body| body["type"] == "message_delta")
            .expect("successful completion after raw arguments");
        assert_eq!(completion["delta"]["stop_reason"], "end_turn");
        assert_eq!(completion["usage"]["input_tokens"], 17);
        assert_eq!(completion["usage"]["output_tokens"], 9);
        assert_eq!(
            events.last().unwrap().event.as_deref(),
            Some("message_stop")
        );
        assert!(
            events
                .iter()
                .all(|event| event.event.as_deref() != Some("error"))
        );

        let wire: String = events
            .iter()
            .map(|event| make_sse_block(event.event.as_deref().unwrap(), &event.data))
            .collect();
        let decoded = AnthropicStreamParser::new()
            .parse_chunk(&wire)
            .expect("raw argument strings do not invalidate source-wire JSON");
        let decoded_fragments: Vec<&str> = decoded
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(decoded_fragments, fragments);
    }
}

// ── AnthropicResponseParser ──

#[test]
fn upstream_compaction_is_rejected_in_unary_and_stream() {
    let body = serde_json::json!({"content": [{"type": "compaction", "content": "summary", "signature": "sig"}], "stop_reason": "compaction"});
    assert!(AnthropicResponseParser.parse_response(body).is_err());
    let mut parser = AnthropicStreamParser::new();
    let event = make_sse_block(
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"compaction","signature":"sig"}}"#,
    );
    assert!(parser.parse_chunk(&event).is_err());
}

#[test]
fn test_parse_response_text_only() {
    let resp = serde_json::json!({
        "id": "msg_1",
        "model": "claude-3-5-sonnet",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let r = AnthropicResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.output_text(), "hello");
    assert!(r.reasoning_items().next().is_none());
    assert_eq!(r.stop_reason.as_deref(), Some("stop"));
}

#[test]
fn test_parse_response_thinking_and_text() {
    // Ollama returns thinking + text blocks in non-stream response.
    let resp = serde_json::json!({
        "id": "msg_2",
        "model": "qwen3",
        "content": [
            {"type": "thinking", "thinking": "let me think...", "signature": "sig_resp"},
            {"type": "text", "text": "hi there"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    });
    let r = AnthropicResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.output_text(), "hi there");
    assert_eq!(
        r.reasoning_items().next(),
        Some(("let me think...", Some("sig_resp")))
    );
}

#[test]
fn test_format_response_includes_thinking_signature() {
    let mut resp = AiResponse::new("msg_sig", "claude-3-7-sonnet");
    resp.push_reasoning("think", Some("sig_resp".to_string()));
    resp.push_output_text("answer");
    resp.stop_reason = Some("stop".to_string());

    let out = AnthropicResponseFormatter.format_response(&resp);
    let thinking = out
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .expect("thinking block");
    assert_eq!(
        thinking.get("type").and_then(|v| v.as_str()),
        Some("thinking")
    );
    assert_eq!(
        thinking.get("signature").and_then(|v| v.as_str()),
        Some("sig_resp")
    );
}

#[test]
fn responses_cipher_response_emits_unsigned_anthropic_public_thinking() {
    use crate::transform::ProtocolTransform;
    use stravia_runtime_contract::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24,
    };

    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let wire = crate::codec::open_responses::formatter::response_resource_snapshot(
        "resp_cipher",
        "reasoning-model",
        "completed",
        vec![serde_json::json!({
            "type": "reasoning",
            "id": "rs_cipher",
            "summary": [{"type": "summary_text", "text": "Public summary"}],
            "content": [{"type": "reasoning_text", "text": "Public detail"}],
            "encrypted_content": "responses-cipher-not-an-anthropic-signature"
        })],
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let response = pair
        .decode_response(wire)
        .expect("Responses response decode");
    let encoded = pair
        .encode_response(&response)
        .expect("Anthropic response encode");
    let blocks = encoded["content"].as_array().expect("Anthropic content");
    let thinking: Vec<_> = blocks
        .iter()
        .filter(|block| block["type"] == "thinking")
        .collect();
    let public_text: String = thinking
        .iter()
        .filter_map(|block| block["thinking"].as_str())
        .collect();
    assert!(public_text.contains("Public summary"), "{encoded}");
    assert!(public_text.contains("Public detail"), "{encoded}");
    assert!(
        thinking
            .iter()
            .all(|block| block.get("signature").is_none()),
        "{encoded}"
    );
    assert!(
        !encoded
            .to_string()
            .contains("responses-cipher-not-an-anthropic-signature")
    );
}

#[test]
fn test_parse_response_tool_use() {
    let resp = serde_json::json!({
        "id": "msg_3",
        "model": "claude-3-5-sonnet",
        "content": [{
            "type": "tool_use",
            "id": "toolu_01",
            "name": "get_weather",
            "input": {"city": "Paris"}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 15, "output_tokens": 8}
    });
    let r = AnthropicResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.tool_calls().count(), 1);
    assert_eq!(
        r.tool_calls().next().map(|call| call.name.as_str()),
        Some("get_weather")
    );
    assert_eq!(r.stop_reason.as_deref(), Some("tool_calls"));
}

// ── AnthropicStreamParser ──

#[test]
fn test_stream_basic_text() {
    let sse = [
            make_sse_block(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet","stop_reason":null,"usage":{"input_tokens":9,"output_tokens":0}}}"#,
            ),
            make_sse_block(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
            ),
            make_sse_block("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            make_sse_block(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            ),
            make_sse_block("message_stop", r#"{"type":"message_stop"}"#),
        ]
        .concat();

    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let has_text = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::TextDelta(t) if t == "hello"));
    let has_done = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::Done { stop_reason } if stop_reason == "stop"));
    assert!(has_text, "expected TextDelta('hello'), got: {deltas:?}");
    assert!(has_done, "expected Done(stop), got: {deltas:?}");
}

#[test]
fn test_stream_thinking_delta_no_signature_delta() {
    // Ollama sends thinking_delta events but omits signature_delta entirely.
    // Parser must not fail and must emit ReasoningDelta.
    let sse = [
            make_sse_block(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","content":[],"model":"qwen3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
            ),
            make_sse_block(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step one"}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" step two"}}"#,
            ),
            // No signature_delta here (Ollama omits it)
            make_sse_block("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            make_sse_block(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
            ),
            make_sse_block("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
            make_sse_block(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
            ),
            make_sse_block("message_stop", r#"{"type":"message_stop"}"#),
        ]
        .concat();

    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let reasoning: Vec<_> = deltas
        .iter()
        .filter_map(|d| {
            if let AiStreamDelta::ThinkingDelta(t) = d {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        !reasoning.is_empty(),
        "expected ReasoningDelta events, got: {deltas:?}"
    );
    assert!(
        reasoning.contains(&"step one"),
        "expected 'step one', got: {reasoning:?}"
    );

    let has_text = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::TextDelta(t) if t == "answer"));
    assert!(has_text, "expected TextDelta('answer'), got: {deltas:?}");
}

#[test]
fn test_stream_signature_delta_is_captured() {
    // Native Anthropic sends signature_delta after thinking block.
    let sse = [
            make_sse_block(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_3","model":"claude-3-7-sonnet","content":[],"stop_reason":null,"usage":{"input_tokens":8,"output_tokens":0}}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think"}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc123"}}"#,
            ),
            make_sse_block(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            ),
        ]
        .concat();

    let mut parser = AnthropicStreamParser::new();
    let result = parser.parse_chunk(&sse);
    assert!(result.is_ok(), "parser must not fail on signature_delta");

    let deltas = result.unwrap();
    let signature = deltas.iter().find_map(|d| {
        if let AiStreamDelta::ThinkingSignature(sig) = d {
            Some(sig.as_str())
        } else {
            None
        }
    });
    assert_eq!(signature, Some("abc123"));
}

#[test]
fn test_stream_formatter_emits_signature_delta() {
    let mut formatter = AnthropicStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "msg_4".to_string(),
            model: "claude-3-7-sonnet".to_string(),
        },
        AiStreamDelta::ThinkingDelta("think".to_string()),
        AiStreamDelta::ThinkingSignature("abc123".to_string()),
    ]);

    let has_signature = events
        .iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
        .any(|json| {
            json["delta"]["type"] == "signature_delta"
                && json["delta"]["signature"].as_str() == Some("abc123")
        });
    assert!(has_signature, "expected signature_delta event");
}

#[test]
fn signed_thinking_keeps_stream_history_order_across_byte_boundaries() {
    use crate::accumulator::StreamResponseAccumulator;
    use crate::codec::open_responses::stream::ResponsesStreamFormatter;
    use crate::transform::ProtocolTransform;
    use serde_json::json;
    use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;

    for thinking in ["", "可见思考"] {
        for explicit_empty_delta in [false, true] {
            let mut events = vec![json!({"type":"message_start", "message": {
                "id":"msg_signed", "model":"claude", "content":[],
                "usage":{"input_tokens":1, "output_tokens":0}
            }})];
            for (index, signature) in [(0, "sig-first"), (2, "sig-second")] {
                events.push(json!({"type":"content_block_start", "index":index,
                    "content_block":{"type":"thinking", "thinking":"", "signature":""}}));
                if explicit_empty_delta {
                    events.push(json!({"type":"content_block_delta", "index":index,
                        "delta":{"type":"thinking_delta", "thinking":""}}));
                }
                if !thinking.is_empty() {
                    events.push(json!({"type":"content_block_delta", "index":index,
                        "delta":{"type":"thinking_delta", "thinking":thinking}}));
                }
                events.push(json!({"type":"content_block_delta", "index":index,
                    "delta":{"type":"signature_delta", "signature":signature}}));
                events.push(json!({"type":"content_block_stop", "index":index}));
                if index == 0 {
                    events.extend([
                        json!({"type":"content_block_start", "index":1, "content_block":{"type":"text", "text":""}}),
                        json!({"type":"content_block_delta", "index":1, "delta":{"type":"text_delta", "text":"answer"}}),
                        json!({"type":"content_block_stop", "index":1}),
                    ]);
                }
            }
            events.extend([
                json!({"type":"content_block_start", "index":3,
                    "content_block":{"type":"tool_use", "id":"call_read", "name":"read", "input":{}}}),
                json!({"type":"content_block_delta", "index":3,
                    "delta":{"type":"input_json_delta", "partial_json":"{\"path\":\"a\"}"}}),
                json!({"type":"content_block_stop", "index":3}),
                json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":2}}),
                json!({"type":"message_stop"}),
            ]);
            let sse: String = events
                .iter()
                .map(|event| make_sse_block(event["type"].as_str().unwrap(), &event.to_string()))
                .collect();
            for chunk_size in [sse.len(), 1, 7, 13] {
                let mut decoder = ProtocolTransform::global()
                    .decode_stream(ANTHROPIC_MESSAGES_2023_06_01)
                    .unwrap();
                let mut accumulator = StreamResponseAccumulator::default();
                let mut formatter = ResponsesStreamFormatter::new();
                let mut wire = Vec::new();
                for chunk in sse.as_bytes().chunks(chunk_size) {
                    let deltas = decoder.decode_chunk(chunk).unwrap();
                    for delta in &deltas {
                        accumulator.apply_with_identity(delta);
                    }
                    wire.extend(formatter.format_deltas(&deltas));
                }
                let end = decoder.finish().unwrap();
                for delta in &end {
                    accumulator.apply_with_identity(delta);
                }
                wire.extend(formatter.format_deltas(&end));
                let history = accumulator.into_ai_response();
                assert_eq!(history.items.len(), 4);
                assert_eq!(
                    history.items[0].thinking_ref(),
                    Some((thinking, Some("sig-first")))
                );
                assert_eq!(history.items[1].output_text_ref(), Some("answer"));
                assert_eq!(
                    history.items[2].thinking_ref(),
                    Some((thinking, Some("sig-second")))
                );
                let call = history.items[3].function_call_ref().unwrap();
                assert_eq!(call.id, "call_read");
                assert_eq!(call.arguments, r#"{"path":"a"}"#);

                let payloads: Vec<Value> = wire
                    .iter()
                    .map(|event| serde_json::from_str(&event.data).unwrap())
                    .collect();
                for event_type in ["response.output_item.added", "response.output_item.done"] {
                    let items: Vec<_> = payloads
                        .iter()
                        .filter(|event| event["type"] == event_type)
                        .map(|event| {
                            (
                                event["output_index"].as_u64().unwrap(),
                                event["item"]["type"].as_str().unwrap(),
                            )
                        })
                        .collect();
                    assert_eq!(
                        items,
                        [
                            (0, "reasoning"),
                            (1, "message"),
                            (2, "reasoning"),
                            (3, "function_call")
                        ]
                    );
                }
                let completed = payloads
                    .iter()
                    .find(|event| event["type"] == "response.completed")
                    .unwrap();
                let output = completed["response"]["output"].as_array().unwrap();
                let delivered: Vec<_> = payloads
                    .iter()
                    .filter(|event| event["type"] == "response.output_item.done")
                    .map(|event| event["item"].clone())
                    .collect();
                assert_eq!(&delivered, output);
                assert_eq!(output[1]["content"][0]["text"], "answer");
                assert!(
                    !serde_json::to_string(&payloads)
                        .unwrap()
                        .contains("sig-first")
                );
                assert!(
                    !serde_json::to_string(&payloads)
                        .unwrap()
                        .contains("sig-second")
                );
            }
        }
    }
}

#[test]
fn open_responses_completed_reasoning_keeps_public_text_without_anthropic_signature_delta() {
    let created = crate::codec::open_responses::formatter::response_resource_snapshot(
        "resp_1",
        "gpt-5.6-luna",
        "in_progress",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let upstream = [
            make_sse_block(
                "response.created",
                &serde_json::json!({
                    "type": "response.created",
                    "sequence_number": 0,
                    "response": created
                })
                .to_string(),
            ),
            make_sse_block(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"opaque-reasoning"}}"#,
            ),
            make_sse_block(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Public summary"}],"content":[{"type":"reasoning_text","text":"Public detail"}],"encrypted_content":"opaque-reasoning"}}"#,
            ),
        ]
        .concat();
    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&upstream)
        .expect("Open Responses stream");
    let events = AnthropicStreamFormatter::new().format_deltas(&deltas);

    let payloads: Vec<Value> = events
        .iter()
        .map(|event| serde_json::from_str(&event.data).expect("Anthropic SSE JSON"))
        .collect();
    assert!(
        payloads
            .iter()
            .all(|json| json["delta"]["type"] != "signature_delta"),
        "Responses cipher must not become an Anthropic signature: {payloads:?}"
    );
    let public_text: String = payloads
        .iter()
        .filter_map(|json| json["delta"]["thinking"].as_str())
        .collect();
    assert!(public_text.contains("Public summary"), "{payloads:?}");
    assert!(public_text.contains("Public detail"), "{payloads:?}");
    assert!(
        events
            .iter()
            .all(|event| !event.data.contains("opaque-reasoning"))
    );
}

#[test]
fn test_stream_tool_use() {
    let sse = [
            make_sse_block(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_4","model":"claude-3-5-sonnet","content":[],"stop_reason":null,"usage":{"input_tokens":20,"output_tokens":0}}}"#,
            ),
            make_sse_block(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{}}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            ),
            make_sse_block(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}"#,
            ),
            make_sse_block("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            make_sse_block(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}"#,
            ),
        ]
        .concat();

    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let has_tool_start = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::ToolCallStart { name, .. } if name == "get_weather"));
    let has_tool_delta = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::ToolCallDelta { .. }));
    let has_done_tool = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::Done { stop_reason } if stop_reason == "tool_calls"));
    assert!(
        has_tool_start,
        "expected ToolCallStart(get_weather), got: {deltas:?}"
    );
    assert!(has_tool_delta, "expected ToolCallDelta, got: {deltas:?}");
    assert!(has_done_tool, "expected Done(tool_calls), got: {deltas:?}");
}

// ── P2: RawEvent forwarding ───────────────────────────────────────────────

#[test]
fn test_unknown_content_block_start_emits_raw_event() {
    // GLM server-side tool (e.g. webReader) sends a content_block_start with
    // type "server_tool_use". The parser must emit RawEvent instead of dropping it.
    let sse = [
            make_sse_block(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_5","model":"glm-5","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}"#,
            ),
            make_sse_block(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtool_01","name":"webReader","input":{}}}"#,
            ),
            make_sse_block(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            make_sse_block(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
            ),
        ]
        .concat();

    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let raw_str = deltas.iter().find_map(|d| {
        if let AiStreamDelta::Unknown { raw } = d {
            if raw.contains("content_block_start") {
                Some(raw.clone())
            } else {
                None
            }
        } else {
            None
        }
    });
    assert!(
        raw_str.is_some(),
        "expected Unknown for server_tool_use, got: {deltas:?}"
    );
    let raw_str = raw_str.unwrap();
    assert!(
        raw_str.contains("content_block_start"),
        "event type mismatch: {raw_str}"
    );
    let data_part = raw_str.split_once("\ndata: ").map(|x| x.1).unwrap_or("{}");
    let data: serde_json::Value = serde_json::from_str(data_part).unwrap_or_default();
    assert_eq!(
        data.pointer("/content_block/type").and_then(|v| v.as_str()),
        Some("server_tool_use"),
    );
}

#[test]
fn test_unknown_top_level_event_emits_raw_event() {
    // A future or provider-specific event type must not be silently dropped.
    let sse = make_sse_block(
        "web_search_result",
        r#"{"type":"web_search_result","results":[{"title":"foo","url":"https://example.com"}]}"#,
    );

    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let raw = deltas
        .iter()
        .find(|d| matches!(d, AiStreamDelta::Unknown { raw } if raw.contains("web_search_result")));
    assert!(
        raw.is_some(),
        "expected Unknown for web_search_result, got: {deltas:?}"
    );
}

#[test]
fn test_raw_event_forwarded_verbatim_by_formatter() {
    // AnthropicStreamFormatter must emit a verbatim SSE event for RawEvent.
    let raw_data = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "server_tool_use", "id": "srv_01", "name": "webSearch", "input": {}}
    });

    let mut formatter = AnthropicStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "msg_6".to_string(),
            model: "glm-5".to_string(),
        },
        AiStreamDelta::Unknown {
            raw: format!("event: content_block_start\ndata: {raw_data}"),
        },
    ]);

    let raw_forwarded = events.iter().find(|ev| {
        ev.event.as_deref() == Some("content_block_start") && ev.data.contains("server_tool_use")
    });
    assert!(
        raw_forwarded.is_some(),
        "formatter must forward RawEvent verbatim; events: {events:?}",
    );
}

#[test]
fn test_unknown_content_block_delta_type_emits_raw_event() {
    // Unknown delta types (citations_delta, web_search_tool_result_delta, etc.)
    // must be forwarded as RawEvent, not silently dropped.
    let sse = make_sse_block(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{"url":"https://example.com","title":"Example"}}}"#,
    );

    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let raw = deltas.iter().find(
        |d| matches!(d, AiStreamDelta::Unknown { raw } if raw.contains("content_block_delta")),
    );
    assert!(
        raw.is_some(),
        "expected Unknown for citations_delta, got: {deltas:?}"
    );
}

// ── Bug-fix ordering tests (Task 0) ──

#[test]
fn test_usage_delta_before_message_start() {
    // Bug 0a: Usage must appear BEFORE MessageStart in the delta list so the
    // formatter has the correct input_tokens when it emits message_start SSE.
    let sse = make_sse_block(
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","model":"glm","usage":{"input_tokens":10,"output_tokens":0}}}"#,
    );
    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let usage_pos = deltas
        .iter()
        .position(|d| matches!(d, AiStreamDelta::Usage(_)));
    let start_pos = deltas
        .iter()
        .position(|d| matches!(d, AiStreamDelta::MessageStart { .. }));
    assert!(
        usage_pos.is_some() && start_pos.is_some(),
        "both deltas must be present; got: {deltas:?}",
    );
    assert!(
        usage_pos.unwrap() < start_pos.unwrap(),
        "Usage must precede MessageStart; got: {deltas:?}",
    );
}

#[test]
fn test_usage_delta_before_done() {
    // Bug 0b: Usage must appear BEFORE Done in the delta list so the formatter
    // has the correct output_tokens when it emits message_delta SSE.
    let sse = make_sse_block(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":43}}"#,
    );
    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let usage_pos = deltas
        .iter()
        .position(|d| matches!(d, AiStreamDelta::Usage(_)));
    let done_pos = deltas
        .iter()
        .position(|d| matches!(d, AiStreamDelta::Done { .. }));
    assert!(
        usage_pos.is_some() && done_pos.is_some(),
        "both deltas must be present; got: {deltas:?}",
    );
    assert!(
        usage_pos.unwrap() < done_pos.unwrap(),
        "Usage must precede Done; got: {deltas:?}",
    );
}

#[test]
fn test_partial_final_usage_updates_known_initial_zero_through_accumulator() {
    use crate::accumulator::StreamResponseAccumulator;

    for (input, cache_read, cache_creation, output) in
        [(100, 50, 200, 301), (100, 0, 0, 301), (0, 0, 0, 0)]
    {
        let mut parser = AnthropicStreamParser::new();
        let mut accumulator = StreamResponseAccumulator::default();
        let start = make_sse_block(
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": "usage-regression",
                    "model": "claude",
                    "usage": {
                        "input_tokens": input,
                        "output_tokens": 0,
                        "cache_read_input_tokens": cache_read,
                        "cache_creation_input_tokens": cache_creation
                    }
                }
            })
            .to_string(),
        );
        accumulator.apply_all(&parser.parse_chunk(&start).unwrap());
        assert!(accumulator.usage.required_components_known);
        assert_eq!(accumulator.usage.completion_tokens, 0);

        let final_event = make_sse_block(
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": output}
            })
            .to_string(),
        );
        let final_deltas = parser.parse_chunk(&final_event).unwrap();
        let AiStreamDelta::Usage(final_usage) = &final_deltas[0] else {
            panic!("final usage must precede Done");
        };
        assert!(final_usage.required_components_known);
        assert_eq!(final_usage.completion_tokens, output);
        assert_eq!(
            final_usage.prompt_tokens,
            input + cache_read + cache_creation
        );
        accumulator.apply_all(&final_deltas);
        accumulator.apply_all(&parser.finish().unwrap());
        let response = accumulator.into_ai_response();
        assert_eq!(response.usage.completion_tokens, output);
        assert_eq!(
            response.usage.prompt_tokens,
            input + cache_read + cache_creation
        );
        assert_eq!(
            response.usage.total_tokens,
            input + cache_read + cache_creation + output
        );
        assert_eq!(response.usage.cache_read_tokens, Some(cache_read));
        assert_eq!(response.usage.cache_creation_tokens, Some(cache_creation));
        assert!(response.usage.required_components_known);
    }
}

#[test]
fn test_message_delta_input_tokens_read() {
    // Bug 0c: input_tokens from message_delta.usage must be captured.
    // ZhipuAI / MiniMax publish the real input count here instead of message_start.
    let sse = make_sse_block(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":60,"output_tokens":43}}"#,
    );
    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let usage = deltas
        .iter()
        .find_map(|d| {
            if let AiStreamDelta::Usage(u) = d {
                Some(u)
            } else {
                None
            }
        })
        .expect("Usage delta must be present");
    assert_eq!(usage.prompt_tokens, 60, "prompt_tokens must be 60");
    assert_eq!(usage.completion_tokens, 43, "completion_tokens must be 43");
}

// ── New usage-field extraction tests (Task 2 – parser) ──

#[test]
fn test_cache_fields_extracted_from_message_start() {
    let sse = make_sse_block(
        "message_start",
        r#"{"type":"message_start","message":{"id":"m","model":"c","usage":{"input_tokens":100,"output_tokens":0,"cache_read_input_tokens":50,"cache_creation_input_tokens":200}}}"#,
    );
    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let usage = deltas
        .iter()
        .find_map(|d| {
            if let AiStreamDelta::Usage(u) = d {
                Some(u)
            } else {
                None
            }
        })
        .expect("Usage delta must be present");
    assert_eq!(usage.prompt_tokens, 350);
    assert_eq!(usage.total_tokens, 350);
    assert_eq!(usage.cache_read_tokens, Some(50));
    assert_eq!(usage.cache_creation_tokens, Some(200));
}

#[test]
fn test_server_tool_use_extracted_from_message_delta() {
    let sse = make_sse_block(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10,"server_tool_use":{"web_search_requests":3,"web_fetch_requests":1}}}"#,
    );
    let mut parser = AnthropicStreamParser::new();
    let deltas = parser.parse_chunk(&sse).unwrap();

    let usage = deltas
        .iter()
        .find_map(|d| {
            if let AiStreamDelta::Usage(u) = d {
                Some(u)
            } else {
                None
            }
        })
        .expect("Usage delta must be present");
    let stu = usage
        .server_tool_use
        .as_ref()
        .expect("server_tool_use must be Some");
    assert_eq!(stu.web_search_requests, 3);
    assert_eq!(stu.web_fetch_requests, 1);
}

// ── New usage-field emission tests (Task 2 – formatter) ──

#[test]
fn test_formatter_message_start_includes_cache_fields() {
    // Usage delta carrying cache fields must appear in the message_start SSE output.
    let usage = Usage {
        prompt_tokens: 350,
        completion_tokens: 0,
        cache_read_tokens: Some(50),
        cache_creation_tokens: Some(200),
        server_tool_use: None,
        ..Usage::default()
    };
    let mut formatter = AnthropicStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::Usage(usage),
        AiStreamDelta::MessageStart {
            id: "m1".into(),
            model: "c".into(),
        },
    ]);

    let start_ev = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_start"))
        .expect("message_start event must be emitted");
    let json: serde_json::Value = serde_json::from_str(&start_ev.data).unwrap();
    let u = json
        .pointer("/message/usage")
        .expect("/message/usage must exist");
    assert_eq!(u["input_tokens"].as_u64(), Some(100));
    assert_eq!(u["cache_read_input_tokens"].as_u64(), Some(50));
    assert_eq!(u["cache_creation_input_tokens"].as_u64(), Some(200));
    assert!(
        u.get("server_tool_use").is_none(),
        "server_tool_use must be absent when None"
    );
}

#[test]
fn test_formatter_message_delta_includes_new_fields() {
    // message_delta SSE must carry cache and server_tool_use when present.
    let usage = Usage {
        prompt_tokens: 60,
        completion_tokens: 43,
        cache_read_tokens: Some(10),
        cache_creation_tokens: None,
        server_tool_use: Some(ServerToolUsage {
            web_search_requests: 2,
            web_fetch_requests: 0,
        }),
        ..Usage::default()
    };
    let mut formatter = AnthropicStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "m2".into(),
            model: "c".into(),
        },
        AiStreamDelta::Usage(usage),
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);

    let delta_ev = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .expect("message_delta event must be emitted");
    let json: serde_json::Value = serde_json::from_str(&delta_ev.data).unwrap();
    let u = &json["usage"];
    assert_eq!(u["input_tokens"].as_u64(), Some(50));
    assert_eq!(u["output_tokens"].as_u64(), Some(43));
    assert_eq!(u["cache_read_input_tokens"].as_u64(), Some(10));
    assert!(
        u.get("cache_creation_input_tokens").is_none(),
        "None field must be absent"
    );
    assert_eq!(
        u["server_tool_use"]["web_search_requests"].as_u64(),
        Some(2)
    );
}

#[test]
fn test_format_response_includes_cache_fields() {
    let mut resp = AiResponse::new("m3", "claude");
    resp.push_output_text("hi".to_string());
    resp.stop_reason = Some("stop".to_string());
    resp.usage = Usage {
        prompt_tokens: 20,
        completion_tokens: 5,
        cache_read_tokens: Some(3),
        cache_creation_tokens: Some(7),
        server_tool_use: Some(ServerToolUsage {
            web_search_requests: 1,
            web_fetch_requests: 0,
        }),
        ..Usage::default()
    };
    let json = AnthropicResponseFormatter.format_response(&resp);
    let u = &json["usage"];
    assert_eq!(u["input_tokens"].as_u64(), Some(10));
    assert_eq!(u["output_tokens"].as_u64(), Some(5));
    assert_eq!(u["cache_read_input_tokens"].as_u64(), Some(3));
    assert_eq!(u["cache_creation_input_tokens"].as_u64(), Some(7));
    assert_eq!(
        u["server_tool_use"]["web_search_requests"].as_u64(),
        Some(1)
    );
}

// ── End-to-end round-trip: ZhipuAI pattern (Task 0 + Task 2) ──

#[test]
fn test_roundtrip_zhipuai_input_tokens_from_message_delta() {
    // ZhipuAI sends input_tokens=0 in message_start but the real value in message_delta.
    // After Bug 0b+0c fixes, output_tokens in the SSE must be correct and
    // formatter.usage() must capture input_tokens=60 from message_delta.
    let sse_start = make_sse_block(
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_z","model":"glm-5","usage":{"input_tokens":0,"output_tokens":0}}}"#,
    );
    let sse_text = make_sse_block(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    );
    let sse_delta = make_sse_block(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":60,"output_tokens":43}}"#,
    );

    let mut parser = AnthropicStreamParser::new();
    let mut all_deltas = vec![];
    for chunk in &[sse_start, sse_text, sse_delta] {
        all_deltas.extend(parser.parse_chunk(chunk).unwrap());
    }

    let mut formatter = AnthropicStreamFormatter::new();
    let events = formatter.format_deltas(&all_deltas);

    let delta_ev = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .expect("message_delta event must be emitted");
    let delta_json: serde_json::Value = serde_json::from_str(&delta_ev.data).unwrap();
    assert_eq!(
        delta_json["usage"]["output_tokens"].as_u64(),
        Some(43),
        "output_tokens must be 43 (Bug 0b: Usage before Done)",
    );

    // formatter.usage() must reflect input_tokens from message_delta (Bug 0c)
    assert_eq!(
        formatter.usage().prompt_tokens,
        60,
        "prompt_tokens=60 from message_delta must be captured in formatter state",
    );
}

// ── redacted_thinking 上游解析 ──

fn anthropic_sse(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .map(|event| make_sse_block(event["type"].as_str().unwrap(), &event.to_string()))
        .collect()
}

#[test]
fn parse_response_keeps_redacted_thinking_payload_in_block_order() {
    let content = serde_json::json!([
        {"type": "thinking", "thinking": "visible", "signature": "sig-visible"},
        {"type": "redacted_thinking", "data": "opaque-redacted"},
        {"type": "text", "text": "answer"},
        {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a"}}
    ]);
    let parsed = AnthropicResponseParser
        .parse_response(serde_json::json!({
            "id": "msg_redacted",
            "model": "claude",
            "content": content,
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();

    let replayed = AnthropicResponseFormatter.format_response(&parsed);
    assert_eq!(replayed["content"], content);
}

#[test]
fn parse_response_skips_redacted_thinking_without_payload() {
    let parsed = AnthropicResponseParser
        .parse_response(serde_json::json!({
            "id": "msg_empty_redacted",
            "model": "claude",
            "content": [
                {"type": "redacted_thinking", "data": ""},
                {"type": "redacted_thinking"},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();

    assert_eq!(parsed.items.len(), 1);
    assert_eq!(parsed.output_text(), "answer");
}

fn redacted_between_thinking_and_text_stream() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"type":"message_start","message":{"id":"msg_stream_redacted","model":"claude","usage":{"input_tokens":3,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"visible"}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-visible"}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque-redacted"}}),
        serde_json::json!({"type":"content_block_stop","index":1}),
        serde_json::json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"answer"}}),
        serde_json::json!({"type":"content_block_stop","index":2}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        serde_json::json!({"type":"message_stop"}),
    ]
}

#[test]
fn stream_redacted_thinking_is_a_typed_completed_item_not_unknown() {
    let blocks = anthropic_sse(&redacted_between_thinking_and_text_stream());
    for chunks in [vec![blocks.concat()], blocks] {
        let mut parser = AnthropicStreamParser::new();
        let mut deltas = Vec::new();
        for chunk in chunks {
            deltas.extend(parser.parse_chunk(&chunk).unwrap());
        }

        let redacted = deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::ItemDone { item, .. } => match &item.content {
                    MessageContent::Blocks(blocks) => match blocks.as_slice() {
                        [ContentBlock::RedactedThinking { data }] => Some(data.as_str()),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(redacted, vec!["opaque-redacted"]);
        assert!(
            !deltas.iter().any(|delta| matches!(
                delta,
                AiStreamDelta::Unknown { raw } if raw.contains("redacted_thinking")
            )),
            "redacted_thinking must not be forwarded as an opaque Unknown event: {deltas:?}"
        );
    }
}

#[test]
fn stream_redacted_thinking_round_trips_through_anthropic_formatter_in_order() {
    let blocks = anthropic_sse(&redacted_between_thinking_and_text_stream());
    let mut parser = AnthropicStreamParser::new();
    let mut formatter = AnthropicStreamFormatter::new();
    let mut started = Vec::new();
    for chunk in &blocks {
        let deltas = parser.parse_chunk(chunk).unwrap();
        for event in formatter.format_deltas(&deltas) {
            if event.event.as_deref() != Some("content_block_start") {
                continue;
            }
            let body: serde_json::Value = serde_json::from_str(&event.data).unwrap();
            let block = &body["content_block"];
            started.push((
                body["index"].as_u64().unwrap(),
                block["type"].as_str().unwrap().to_owned(),
                block["data"].as_str().map(str::to_owned),
            ));
        }
    }

    assert_eq!(
        started,
        vec![
            (0, "thinking".to_owned(), None),
            (
                1,
                "redacted_thinking".to_owned(),
                Some("opaque-redacted".to_owned())
            ),
            (2, "text".to_owned(), None),
        ]
    );
}

#[test]
fn stream_accumulator_keeps_redacted_thinking_in_block_order() {
    let blocks = anthropic_sse(&redacted_between_thinking_and_text_stream());
    let mut parser = AnthropicStreamParser::new();
    let mut accumulator = crate::accumulator::StreamResponseAccumulator::default();
    for chunk in &blocks {
        accumulator.apply_all(&parser.parse_chunk(chunk).unwrap());
    }

    let replayed = AnthropicResponseFormatter.format_response(&accumulator.into_ai_response());
    assert_eq!(
        replayed["content"],
        serde_json::json!([
            {"type": "thinking", "thinking": "visible", "signature": "sig-visible"},
            {"type": "redacted_thinking", "data": "opaque-redacted"},
            {"type": "text", "text": "answer"}
        ])
    );
}

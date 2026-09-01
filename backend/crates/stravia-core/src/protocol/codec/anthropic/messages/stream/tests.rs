use super::*;
use crate::protocol::codec::open_responses::parser::ResponsesStreamParser;
use crate::protocol::ir::{AiResponse, AiStreamDelta};

fn make_sse_block(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

#[test]
fn stream_formatter_encodes_canonical_stream_error() {
    let mut formatter = AnthropicStreamFormatter::new();
    let events = formatter.format_deltas(&[AiStreamDelta::StreamError {
        error: crate::protocol::ir::AiError::new(
            crate::protocol::ir::AiErrorKind::StreamMidError,
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

// ── AnthropicResponseParser ──

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
fn standard_empty_block_starts_do_not_emit_unknown_deltas() {
    let sse = [
            make_sse_block(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            ),
            make_sse_block(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
        ]
        .join("");

    let deltas = AnthropicStreamParser::default()
        .parse_chunk(&sse)
        .expect("standard block starts");

    assert!(deltas.is_empty(), "unexpected canonical deltas: {deltas:?}");
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
            json.get("delta")
                .and_then(|delta| delta.get("signature"))
                .and_then(|signature| signature.as_str())
                == Some("abc123")
        });
    assert!(has_signature, "expected signature_delta event");
}

#[test]
fn open_responses_encrypted_reasoning_reaches_anthropic_signature_delta() {
    let created = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
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
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"opaque-reasoning"}}"#,
            ),
        ]
        .concat();
    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&upstream)
        .expect("Open Responses stream");
    let events = AnthropicStreamFormatter::new().format_deltas(&deltas);

    assert!(
        events
            .iter()
            .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
            .any(|json| {
                json["delta"]["type"] == "signature_delta"
                    && json["delta"]["signature"] == "opaque-reasoning"
            })
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

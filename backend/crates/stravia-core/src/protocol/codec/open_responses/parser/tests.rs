use super::*;
use crate::protocol::ir::AiStreamDelta;

fn sse_event(event: &str, data: &str) -> String {
    let mut payload: Value = serde_json::from_str(data).expect("SSE fixture JSON");
    if let Some(partial) = payload.get("response").cloned() {
        let id = partial
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("response");
        let model = partial
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("model");
        let status = partial
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("in_progress");
        let mut response = super::super::formatter::response_resource_snapshot(
            id,
            model,
            status,
            Vec::new(),
            Value::Null,
            Value::Null,
            Value::Null,
        );

        response
            .as_object_mut()
            .expect("response object")
            .extend(partial.as_object().expect("partial response").clone());
        payload["response"] = response;
    }
    format!("event: {event}\ndata: {payload}\n\n")
}

fn dated_response(partial: Value) -> Value {
    let mut response = super::super::formatter::response_resource_snapshot(
        "response",
        "model",
        "completed",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    response
        .as_object_mut()
        .expect("response object")
        .extend(partial.as_object().expect("partial object").clone());
    response
}

fn sse_data(data: &str) -> String {
    format!("data: {data}\n\n")
}
#[test]
fn stream_rejects_partial_response_resource_snapshots() {
    let error = ResponsesStreamParser::new()
            .parse_chunk(
                "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"model\":\"model\",\"status\":\"in_progress\"}}\n\n",
            )
            .expect_err("partial response resource");

    assert!(
        error.to_string().contains("missing required field"),
        "unexpected error: {error}"
    );
}

#[test]
fn stream_error_preserves_upstream_status() {
    let deltas = ResponsesStreamParser::new()
            .parse_chunk(
                "event: error\ndata: {\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"System messages are not allowed\"}}\n\n",
            )
            .expect("error event");

    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::StreamError { error }
            if error.status_code == Some(400)
                && error.message == "System messages are not allowed"
    )));
}
#[test]
fn stream_accepts_known_codex_response_resource_extensions() {
    let response = dated_response(serde_json::json!({
        "id": "resp_codex",
        "model": "gpt-5.4",
        "status": "in_progress",
        "moderation": null,
        "prompt_cache_retention": "24h",
        "tool_usage": {"web_search": {"num_requests": 0}},
        "user": null,
        "reasoning": {
            "effort": "none",
            "summary": null,
            "context": "current_turn",
            "mode": "standard"
        }
    }));
    let event = serde_json::json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": response
    });

    ResponsesStreamParser::new()
        .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
        .expect("known Codex response extensions");
}

#[test]
fn stream_accepts_gpt56_prompt_cache_options() {
    let response = dated_response(serde_json::json!({
        "id": "resp_gpt56",
        "model": "gpt-5.6-luna",
        "status": "in_progress",
        "prompt_cache_options": {
            "mode": "implicit",
            "ttl": "30m"
        }
    }));
    let event = serde_json::json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": response
    });

    ResponsesStreamParser::new()
        .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
        .expect("GPT-5.6 prompt cache options");
}

#[test]
fn stream_omits_provider_function_output_schema() {
    let response = dated_response(serde_json::json!({
        "id": "resp_gpt56",
        "model": "gpt-5.6-luna",
        "status": "in_progress",
        "tools": [{
            "type": "function",
            "name": "web_search",
            "description": "Search the public web.",
            "parameters": {"type": "object"},
            "output_schema": {"type": "object"}
        }]
    }));
    let event = serde_json::json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": response
    });

    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
        .expect("provider function output schema is response metadata");

    let metadata = deltas
        .iter()
        .find_map(|delta| match delta {
            AiStreamDelta::ResponseMetadata { metadata } => Some(metadata),
            _ => None,
        })
        .expect("response metadata");
    assert!(metadata["tools"][0].get("output_schema").is_none());
}

#[test]
fn stream_accepts_openai_responses_events_without_sequence_numbers() {
    let response = dated_response(serde_json::json!({
        "id": "resp_openai",
        "model": "gpt-5.4-mini",
        "status": "in_progress"
    }));
    let event = serde_json::json!({
        "type": "response.created",
        "response": response
    });

    ResponsesStreamParser::new()
        .parse_chunk(&format!("event: response.created\ndata: {event}\n\n"))
        .expect("OpenAI Responses omits sequence_number");
}

#[test]
fn stream_accepts_rolling_reasoning_text_events() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-luna","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[]}}"#,
            ),
            sse_event(
                "response.reasoning_text.delta",
                r#"{"type":"response.reasoning_text.delta","sequence_number":2,"item_id":"rs_1","output_index":0,"content_index":0,"delta":"considering"}"#,
            ),
            sse_event(
                "response.reasoning_text.done",
                r#"{"type":"response.reasoning_text.done","sequence_number":3,"item_id":"rs_1","output_index":0,"content_index":0,"text":"considering"}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&sse)
        .expect("rolling reasoning text events");

    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::ThinkingDeltaWithMetadata {
            text,
            output_index: Some(0),
            content_index: Some(0),
            ..
        } if text == "considering"
    )));
}

#[test]
fn stream_accepts_provider_reasoning_metadata() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-sol","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"internal_chat_message_metadata_passthrough":{"source":"codex"}}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"internal_chat_message_metadata_passthrough":{"source":"codex"}}}"#,
            ),
        ]
        .concat();

    ResponsesStreamParser::new()
        .parse_chunk(&sse)
        .expect("provider reasoning metadata");
}

#[test]
fn stream_accepts_provider_function_call_metadata() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-luna","status":"in_progress","provider_extension":true}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"todowrite","arguments":"","status":"in_progress","internal_chat_message_metadata_passthrough":{"create_time":1787534585.512904,"turn_id":"turn_1"},"metadata":{"turn_id":"turn_1"},"provider_extension":true}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"todowrite","arguments":"{\"todos\":[]}","status":"completed","internal_chat_message_metadata_passthrough":{"create_time":1787534585.512904,"turn_id":"turn_1"},"metadata":{"turn_id":"turn_1"},"provider_extension":true}}"#,
            ),
        ]
        .concat();

    ResponsesStreamParser::new()
        .parse_chunk(&sse)
        .expect("provider function call metadata");
}

#[test]
fn stream_preserves_encrypted_reasoning_from_completed_item() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-5.6-luna","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"opaque-reasoning"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"opaque-reasoning"}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&sse)
        .expect("encrypted reasoning item");

    assert!(
        !deltas
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::ThinkingSignature(_)))
    );
    assert!(
        deltas
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::ProtectedThinkingStart { index: 0 }))
    );
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::ItemDone { index: 0, item }
            if item.reasoning_ref().is_some_and(
                |(_, _, encrypted)| encrypted == Some("opaque-reasoning")
            )
    )));
}

#[test]
fn stream_accepts_openai_responses_without_done_sentinel() {
    let created = dated_response(serde_json::json!({
        "id": "resp_openai",
        "model": "gpt-5.4-mini",
        "status": "in_progress"
    }));
    let completed = dated_response(serde_json::json!({
        "id": "resp_openai",
        "model": "gpt-5.4-mini",
        "status": "completed"
    }));
    let mut parser = ResponsesStreamParser::new();
    parser
            .parse_chunk(&format!(
                "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{created}}}\n\n"
            ))
            .expect("created event");
    parser
            .parse_chunk(&format!(
                "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n"
            ))
            .expect("completed event");
    parser.finish().expect("terminal event is sufficient");
}

// ── ResponsesResponseParser ──

#[test]
fn test_parse_response_message_output() {
    let resp = dated_response(serde_json::json!({
        "id": "resp_1",
        "model": "gpt-4o",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}],
                "internal_chat_message_metadata_passthrough": {"source": "codex"},
                "metadata": {"kind": "assistant"}
            }
        ],
        "usage": {
            "input_tokens": 5,
            "output_tokens": 3,
            "total_tokens": 8,
            "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 2},
            "output_tokens_details": {"reasoning_tokens": 0}
        }
    }));
    let r = ResponsesResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.output_text(), "hello");
    assert_eq!(r.stop_reason.as_deref(), Some("stop"));
    assert_eq!(r.usage.prompt_tokens, 5);
    assert_eq!(r.usage.cache_creation_tokens, Some(2));
}
#[test]
fn test_parse_response_preserves_registered_items() {
    let resp = dated_response(serde_json::json!({
        "id": "resp_agent",
        "model": "gpt-4o",
        "status": "completed",
        "output": [{
            "id": "agent_1",
            "type": "stravia:agent_result",
            "status": "completed",
            "turn_id": "aturn_1"
        }, {
            "id": "media_1",
            "type": "stravia:media_result",
            "status": "completed",
            "turn_id": "aturn_media",
            "completion": "complete"
        }]
    }));

    let response = ResponsesResponseParser.parse_response(resp).unwrap();
    let items = &response.items;
    assert_eq!(items.len(), 2);
    let agent = items[0].unknown_ref().expect("agent result");
    assert_eq!(agent["type"], "stravia:agent_result");
    assert_eq!(agent["turn_id"], "aturn_1");
    let media = items[1].unknown_ref().expect("media result");
    assert_eq!(media["type"], "stravia:media_result");
    assert_eq!(media["turn_id"], "aturn_media");
    assert_eq!(media["completion"], "complete");
}

#[test]
fn test_parse_response_rejects_unregistered_namespaced_items() {
    let resp = dated_response(serde_json::json!({
        "output": [{
            "id": "unknown_1",
            "type": "provider:future_item",
            "status": "completed",
            "payload": "must not cross the protocol boundary"
        }]
    }));

    let error = ResponsesResponseParser
        .parse_response(resp)
        .expect_err("unregistered output extension");
    assert!(
        error
            .to_string()
            .contains("unregistered Open Responses output extension")
    );
}

#[test]
fn test_parse_response_with_encrypted_content_plaintext() {
    // Ollama's Responses API returns reasoning as plaintext in encrypted_content field.
    // The parser must not fail, and should extract text from the content array.
    let resp = dated_response(serde_json::json!({
        "id": "resp_2",
        "model": "qwen3",

        "status": "completed",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "thinking..."}],
                // encrypted_content is plaintext in Ollama — parser must not crash
                "encrypted_content": "plaintext-not-base64"
            },
            {
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "answer"}]
            }
        ],
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20,
            "total_tokens": 30,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }
    }));
    let result = ResponsesResponseParser.parse_response(resp);
    assert!(
        result.is_ok(),
        "parser must not fail on plaintext encrypted_content"
    );
    let r = result.unwrap();
    assert_eq!(r.output_text(), "answer");
}
#[test]
fn reasoning_summary_and_content_round_trip_without_merging() {
    let response = dated_response(serde_json::json!({
        "output": [{
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "short summary"}],
            "content": [{"type": "reasoning_text", "text": "full reasoning"}],
            "encrypted_content": "opaque"
        }]
    }));

    let canonical = ResponsesResponseParser
        .parse_response(response)
        .expect("parse reasoning item");
    let (summary, content, encrypted) = canonical.items[0]
        .reasoning_ref()
        .expect("typed reasoning item");
    assert_eq!(summary, ["short summary"]);
    assert_eq!(content, ["full reasoning"]);
    assert_eq!(encrypted, Some("opaque"));
    let formatted = super::super::formatter::ResponsesResponseFormatter.format_response(&canonical);

    assert_eq!(
        formatted["output"][0]["summary"],
        serde_json::json!([{"type": "summary_text", "text": "short summary"}])
    );
    assert_eq!(
        formatted["output"][0]["content"],
        serde_json::json!([{"type": "reasoning_text", "text": "full reasoning"}])
    );
}

#[test]
fn rejects_schema_invalid_core_output_items() {
    for output in [
        serde_json::json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "hello"}]
        }),
        serde_json::json!({
            "type": "function_call",
            "id": "fc_1",
            "status": "completed",
            "call_id": "",
            "name": "lookup",
            "arguments": "{}"
        }),
        serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "status": "completed"
        }),
    ] {
        let error = ResponsesResponseParser
            .parse_response(dated_response(serde_json::json!({
                "output": [output]
            })))
            .expect_err("schema-invalid output item");
        assert!(
            error.to_string().contains("output item"),
            "unexpected error: {error}"
        );
    }
}
#[test]
fn test_parse_response_function_call_output() {
    let resp = dated_response(serde_json::json!({
        "id": "resp_3",
        "model": "gpt-4o",
        "status": "completed",
        "output": [
            {
                "type": "function_call",
                "id": "fc_1",
                "status": "completed",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"city\":\"Paris\"}"
            }
        ],
        "usage": {
            "input_tokens": 15,
            "output_tokens": 10,
            "total_tokens": 25,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }
    }));
    let r = ResponsesResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.tool_calls().count(), 1);
    let call = r.tool_calls().next().expect("function call");
    assert_eq!(call.id, "call_abc");
    assert_eq!(call.name, "get_weather");
}

// ── ResponsesStreamParser ──

#[test]
fn test_stream_output_text_delta() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"gpt-4o","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"hello","logprobs":[{"token":"hello","logprob":-0.1}],"obfuscation":"pad"}"#,
            ),
            sse_event(
                "response.content_part.done",
                r#"{"type":"response.content_part.done","sequence_number":4,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"hello","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[],"logprobs":[]}]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":6,"response":{"id":"resp_1","model":"gpt-4o","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::TextDeltaWithMetadata {
            text,
            logprobs,
            obfuscation: Some(obfuscation),
            output_index: Some(0),
            content_index: Some(0),
        } if text == "hello"
            && logprobs[0]["token"] == "hello"
            && obfuscation == "pad"
    )));
    assert!(
        deltas
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
    );
}

#[test]
fn test_stream_reasoning_summary_text_delta() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_2","model":"qwen3","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
            ),
            sse_event(
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","sequence_number":2,"item_id":"rs_1","output_index":0,"summary_index":0,"delta":"thinking step","obfuscation":"pad"}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking step"}]}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":4,"output_index":1,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":5,"item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":6,"item_id":"msg_1","output_index":1,"content_index":0,"delta":"answer text"}"#,
            ),
            sse_event(
                "response.content_part.done",
                r#"{"type":"response.content_part.done","sequence_number":7,"item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","text":"answer text","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":8,"output_index":1,"item":{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"answer text","annotations":[],"logprobs":[]}]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":9,"response":{"id":"resp_2","model":"qwen3","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::ReasoningSummaryDelta {
            text,
            obfuscation: Some(obfuscation),
            output_index: Some(0),
            content_index: Some(0),
        } if text == "thinking step" && obfuscation == "pad"
    )));
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::TextDeltaWithMetadata { text, .. } if text == "answer text"
    )));
}

#[test]
fn item_done_only_summary_becomes_hook_visible_semantic_delta() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_summary","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_provider","summary":[],"content":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"reasoning","id":"rs_provider","summary":[{"type":"summary_text","text":"late summary"}],"content":[]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":3,"response":{"id":"resp_summary","model":"model","status":"completed","output":[],"usage":null}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&sse)
        .expect("valid item-done summary");
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::ReasoningSummaryDelta {
            text,
            output_index: Some(0),
            content_index: Some(0),
            ..
        } if text == "late summary"
    )));
    let events = super::super::stream::ResponsesStreamFormatter::new().format_deltas(&deltas);
    let added = events
        .iter()
        .position(|event| event.event.as_deref() == Some("response.reasoning_summary_part.added"))
        .expect("summary part added");
    let done = events
        .iter()
        .position(|event| event.event.as_deref() == Some("response.reasoning_summary_part.done"))
        .expect("summary part done");
    assert!(added < done);
}

#[test]
fn refusal_delta_remains_distinct_from_output_text() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_refusal","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_refusal","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_refusal","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":""}}"#,
            ),
            sse_event(
                "response.refusal.delta",
                r#"{"type":"response.refusal.delta","sequence_number":3,"item_id":"msg_refusal","output_index":0,"content_index":0,"delta":"cannot comply"}"#,
            ),
        ]
        .concat();
    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&sse)
        .expect("dated refusal events");
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::RefusalDeltaWithIndex {
            text,
            output_index: 0,
            content_index: 0,
        } if text == "cannot comply"
    )));
    assert!(
        !deltas
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::TextDelta(_)))
    );
}

#[test]
fn test_done_sentinel_before_terminal_event_is_rejected() {
    let sse = sse_data("[DONE]");
    let mut parser = ResponsesStreamParser::new();
    let error = parser
        .parse_chunk(&sse)
        .expect_err("bare transport terminator must not invent completion");
    assert!(error.to_string().contains("before a terminal"));
}

#[test]
fn rejects_rolling_or_mismatched_provider_events() {
    for sse in [
        sse_event(
            "response.future.delta",
            r#"{"type":"response.future.delta","sequence_number":0}"#,
        ),
        sse_event(
            "response.output_text.delta",
            r#"{"type":"response.refusal.delta","sequence_number":0,"delta":"no"}"#,
        ),
    ] {
        let error = ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect_err("provider event must match the dated contract");
        assert!(
            error.to_string().contains("unsupported")
                || error.to_string().contains("does not match")
        );
    }
}

#[test]
fn rejects_malformed_done_event_references_and_payloads() {
    let message_prefix = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
        ]
        .concat();
    let mut parser = ResponsesStreamParser::new();
    parser.parse_chunk(&message_prefix).expect("message prefix");
    let error = parser
            .parse_chunk(&sse_event(
                "response.output_text.done",
                r#"{"type":"response.output_text.done","sequence_number":3,"item_id":"wrong","output_index":0,"content_index":0,"text":"answer","logprobs":[]}"#,
            ))
            .expect_err("done event with a mismatched item id");
    assert!(error.to_string().contains("item_id"));

    let function_prefix = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_2","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"","status":"in_progress"}}"#,
            ),
        ]
        .concat();
    let mut parser = ResponsesStreamParser::new();
    parser
        .parse_chunk(&function_prefix)
        .expect("function prefix");
    let error = parser
            .parse_chunk(&sse_event(
                "response.function_call_arguments.done",
                r#"{"type":"response.function_call_arguments.done","sequence_number":2,"item_id":"fc_1","output_index":0,"arguments":"{"}"#,
            ))
            .expect_err("done event with invalid final arguments");
    assert!(error.to_string().contains("arguments"));
}

#[test]
fn test_stream_function_call_done_does_not_duplicate_start() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"fc_1","output_index":0,"delta":"{\"city\":\"Par"}"#,
            ),
            sse_event(
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"fc_1","output_index":0,"delta":"is\"}"}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
    assert_eq!(
        deltas
            .iter()
            .filter(|delta| matches!(delta, AiStreamDelta::ToolCallStart { .. }))
            .count(),
        1
    );
    assert_eq!(
        deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
                _ => None,
            })
            .collect::<String>(),
        r#"{"city":"Paris"}"#
    );
}

#[test]
fn test_stream_function_call_done_emits_arguments_when_no_deltas() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":"{\"city\":\"Paris\"}","status":"completed"}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
    assert!(deltas.iter().any(
            |delta| matches!(delta, AiStreamDelta::ToolCallDelta { arguments, .. } if arguments == r#"{"city":"Paris"}"#)
        ));
}
#[test]
fn test_stream_registered_items_round_trip_once_on_done() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":1,"item":{"id":"agent_1","type":"stravia:agent_result","status":"in_progress","turn_id":"aturn_1"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":2,"output_index":1,"item":{"id":"agent_1","type":"stravia:agent_result","status":"completed","turn_id":"aturn_1"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":3,"output_index":2,"item":{"id":"media_1","type":"stravia:media_result","status":"in_progress","turn_id":"aturn_media","completion":"complete"}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":4,"output_index":2,"item":{"id":"media_1","type":"stravia:media_result","status":"completed","turn_id":"aturn_media","completion":"complete"}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new().parse_chunk(&sse).unwrap();
    let results = deltas
        .iter()
        .filter_map(|delta| match delta {
            AiStreamDelta::Unknown { raw } => Some(raw),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert!(results[0].contains(r#""type":"stravia:agent_result""#));
    assert!(results[1].contains(r#""type":"stravia:media_result""#));
}

#[test]
fn non_stream_output_text_annotations_survive_same_protocol_round_trip() {
    let response = dated_response(serde_json::json!({
        "id": "resp_annotation",
        "model": "model",
        "output": [{
            "type": "message",
            "id": "msg_annotation",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "Example",
                "annotations": [{
                    "type": "url_citation",
                    "start_index": 0,
                    "end_index": 7,
                    "url": "https://example.test",
                    "title": "Example"
                }],
                "logprobs": []
            }]
        }]
    }));
    let canonical = ResponsesResponseParser
        .parse_response(response)
        .expect("dated response");
    let encoded = super::super::formatter::ResponsesResponseFormatter.format_response(&canonical);

    assert_eq!(
        encoded["output"][0]["content"][0]["annotations"][0]["url"],
        "https://example.test"
    );
    assert_eq!(
        encoded["output"][0]["content"][0]["annotations"][0]["start_index"],
        0
    );
    assert_eq!(
        encoded["output"][0]["content"][0]["annotations"][0]["end_index"],
        7
    );
}

#[test]
fn preserves_output_text_annotations_for_same_protocol_reencoding() {
    let sse = [
            sse_event(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_provider","model":"model","status":"in_progress"}}"#,
            ),
            sse_event(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_provider","status":"in_progress","role":"assistant","content":[]}}"#,
            ),
            sse_event(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_provider","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_text.annotation.added",
                r#"{"type":"response.output_text.annotation.added","sequence_number":3,"item_id":"msg_provider","output_index":0,"content_index":0,"annotation_index":0,"annotation":{"type":"url_citation","start_index":0,"end_index":7,"url":"https://example.test","title":"Example"}}"#,
            ),
            sse_event(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_provider","output_index":0,"content_index":0,"delta":"Example","logprobs":[]}"#,
            ),
            sse_event(
                "response.content_part.done",
                r#"{"type":"response.content_part.done","sequence_number":5,"item_id":"msg_provider","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Example","annotations":[{"type":"url_citation","start_index":0,"end_index":7,"url":"https://example.test","title":"Example"}],"logprobs":[]}}"#,
            ),
            sse_event(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"type":"message","id":"msg_provider","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Example","annotations":[{"type":"url_citation","start_index":0,"end_index":7,"url":"https://example.test","title":"Example"}],"logprobs":[]}]}}"#,
            ),
            sse_event(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":7,"response":{"id":"resp_provider","model":"model","status":"completed","output":[],"usage":null}}"#,
            ),
        ]
        .concat();

    let deltas = ResponsesStreamParser::new()
        .parse_chunk(&sse)
        .expect("dated annotation event");
    let events = super::super::stream::ResponsesStreamFormatter::new().format_deltas(&deltas);
    let annotation = events
        .iter()
        .find(|event| event.event.as_deref() == Some("response.output_text.annotation.added"))
        .expect("re-encoded annotation");
    let body: Value = serde_json::from_str(&annotation.data).expect("annotation JSON");

    assert_eq!(body["type"], "response.output_text.annotation.added");
    assert_eq!(body["sequence_number"], 5);
    assert_eq!(body["annotation"]["url"], "https://example.test");
    assert_ne!(body["item_id"], "msg_provider");
}

#[test]
fn rejects_annotation_events_missing_required_dated_fields() {
    let malformed = [
        serde_json::json!({
            "type": "response.output_text.annotation.added",
            "sequence_number": 3,
            "item_id": "msg_provider",
            "output_index": 0,
            "content_index": 0,
            "annotation": null
        }),
        serde_json::json!({
            "type": "response.output_text.annotation.added",
            "sequence_number": 3,
            "item_id": "msg_provider",
            "output_index": 0,
            "content_index": 0,
            "annotation_index": 0
        }),
        serde_json::json!({
            "type": "response.output_text.annotation.added",
            "sequence_number": 3,
            "item_id": "msg_provider",
            "output_index": 0,
            "content_index": 0,
            "annotation_index": 0,
            "annotation": {"type": "url_citation", "url": "https://example.test"}
        }),
    ];

    for event in malformed {
        let sse = [
                sse_event(
                    "response.created",
                    r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_provider","model":"model","status":"in_progress"}}"#,
                ),
                sse_event(
                    "response.output_item.added",
                    r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_provider","status":"in_progress","role":"assistant","content":[]}}"#,
                ),
                sse_event(
                    "response.content_part.added",
                    r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_provider","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}"#,
                ),
                sse_event(
                    "response.output_text.annotation.added",
                    &event.to_string(),
                ),
            ]
            .concat();

        let error = ResponsesStreamParser::new()
            .parse_chunk(&sse)
            .expect_err("malformed dated annotation event");
        assert!(error.to_string().contains("annotation"));
    }
}
#[test]
fn rejects_response_resource_missing_a_required_field() {
    let mut response = dated_response(serde_json::json!({
        "id": "resp_provider",
        "model": "logical-model"
    }));
    response
        .as_object_mut()
        .expect("response object")
        .remove("temperature");

    let error = ResponsesResponseParser
        .parse_response(response)
        .expect_err("missing required response field must fail");

    assert!(error.to_string().contains("temperature"));
}
#[test]
fn rejects_unknown_response_status() {
    let error = ResponsesResponseParser
        .parse_response(dated_response(serde_json::json!({
            "status": "future_status"
        })))
        .expect_err("dated status enum is closed");

    assert!(error.to_string().contains("response status"));
}

#[test]
fn sse_parser_handles_crlf_split_at_every_boundary() {
    let created = sse_event(
        "response.created",
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","model":"model","status":"in_progress"}}"#,
    );
    let completed = sse_event(
        "response.completed",
        r#"{"type":"response.completed","sequence_number":1,"response":{"id":"resp_1","model":"model","status":"completed"}}"#,
    );
    let stream = format!("{created}{completed}data: [DONE]\n\n").replace('\n', "\r\n");

    for split in 0..=stream.len() {
        let mut parser = ResponsesStreamParser::new();
        let mut deltas = parser
            .parse_chunk(&stream[..split])
            .expect("first arbitrary chunk");
        deltas.extend(
            parser
                .parse_chunk(&stream[split..])
                .expect("second arbitrary chunk"),
        );
        deltas.extend(parser.finish().expect("complete dated stream"));
        assert!(
            deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::Done { .. })),
            "missing terminal delta at byte split {split}"
        );
    }
}

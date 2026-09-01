use super::*;
use crate::protocol::ir::{AiResponse, AiStreamDelta};

#[test]
fn formatters_emit_cached_prompt_token_details() {
    let mut response = AiResponse::new("chatcmpl-cache", "model");
    response.usage = Usage {
        prompt_tokens: 100,
        completion_tokens: 20,
        cache_read_tokens: Some(80),
        cache_creation_tokens: Some(10),
        ..Usage::default()
    };
    let formatted = OpenAIResponseFormatter.format_response(&response);
    assert_eq!(formatted["usage"]["prompt_tokens"], 100);
    assert_eq!(
        formatted["usage"]["prompt_tokens_details"]["cached_tokens"],
        80
    );
    assert!(
        formatted["usage"]["prompt_tokens_details"]
            .get("cache_creation_tokens")
            .is_none()
    );

    let mut formatter = OpenAIStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::Usage(response.usage),
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let terminal: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(terminal["usage"]["prompt_tokens"], 100);
    assert_eq!(
        terminal["usage"]["prompt_tokens_details"]["cached_tokens"],
        80
    );
}

fn data_sse(json: &str) -> String {
    format!("data: {json}\n\n")
}

#[test]
fn stream_formatter_encodes_canonical_stream_error() {
    let mut formatter = OpenAIStreamFormatter::new();
    let events = formatter.format_deltas(&[AiStreamDelta::StreamError {
        error: crate::protocol::ir::AiError::new(
            crate::protocol::ir::AiErrorKind::StreamMidError,
            "stream aborted",
        ),
    }]);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, None);
    let body: Value = serde_json::from_str(&events[0].data).expect("error JSON");
    assert_eq!(body["error"]["type"], "stream_mid_error");
    assert_eq!(body["error"]["message"], "stream aborted");
}

// ── extract_usage cache-token variants ──

#[test]
fn extract_usage_deepseek_prompt_cache_hit_tokens() {
    let resp = serde_json::json!({
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 800,
            "prompt_cache_miss_tokens": 200
        }
    });
    let u = extract_usage(&resp);
    assert_eq!(u.prompt_tokens, 1000);
    assert_eq!(u.completion_tokens, 50);
    assert_eq!(u.cache_read_tokens, Some(800));
}

#[test]
fn extract_usage_openai_prompt_tokens_details_cached() {
    let resp = serde_json::json!({
        "usage": {
            "prompt_tokens": 1500,
            "completion_tokens": 100,
            "prompt_tokens_details": { "cached_tokens": 1200 }
        }
    });
    let u = extract_usage(&resp);
    assert_eq!(u.prompt_tokens, 1500);
    assert_eq!(u.cache_read_tokens, Some(1200));
}

#[test]
fn extract_usage_gemini_cached_content_token_count() {
    let resp = serde_json::json!({
        "usage": {
            "prompt_tokens": 2000,
            "completion_tokens": 200,
            "cached_content_token_count": 1700
        }
    });
    let u = extract_usage(&resp);
    assert_eq!(u.cache_read_tokens, Some(1700));
}

#[test]
fn extract_usage_no_cache_field_yields_none() {
    let resp = serde_json::json!({
        "usage": {
            "prompt_tokens": 500,
            "completion_tokens": 50
        }
    });
    let u = extract_usage(&resp);
    assert_eq!(u.prompt_tokens, 500);
    assert_eq!(u.cache_read_tokens, None);
}

// ── OpenAIResponseParser ──

#[test]
fn test_parse_response_basic() {
    let resp = serde_json::json!({
        "id": "chatcmpl-1",
        "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    });
    let r = OpenAIResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.output_text(), "hi");
    assert_eq!(r.stop_reason.as_deref(), Some("stop"));
    assert_eq!(r.usage.prompt_tokens, 5);
    assert_eq!(r.usage.completion_tokens, 2);
}

#[test]
fn test_parse_response_with_reasoning_content() {
    let resp = serde_json::json!({
        "id": "chatcmpl-2",
        "model": "deepseek-r1",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "answer",
                "reasoning_content": "my reasoning"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    });
    let r = OpenAIResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.output_text(), "answer");
    assert_eq!(
        r.reasoning_items().next().map(|(text, _)| text),
        Some("my reasoning")
    );
}

// ── OpenAIStreamParser – tool call streaming ──

#[test]
fn test_stream_tool_call_fragments() {
    // First chunk carries id + name with empty arguments.
    // Subsequent chunks carry only argument fragments (no id).
    let chunks = [
            data_sse(r#"{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cit"}}]},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"y\":\"Paris\"}"}}]},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
            data_sse("[DONE]"),
        ]
        .concat();

    let mut parser = OpenAIStreamParser::new();
    let deltas = parser.parse_chunk(&chunks).unwrap();

    let has_tool_start = deltas
            .iter()
            .any(|d| matches!(d, AiStreamDelta::ToolCallStart { id, name, .. } if id == "call_abc" && name == "get_weather"));
    assert!(
        has_tool_start,
        "expected ToolCallStart with id+name, got: {deltas:?}"
    );

    let args: String = deltas
        .iter()
        .filter_map(|d| {
            if let AiStreamDelta::ToolCallDelta { arguments, .. } = d {
                Some(arguments.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        args.contains("Paris"),
        "tool call arguments fragments not accumulated: {args}"
    );
}

#[test]
fn test_stream_think_tags_across_chunks() {
    // <think> and </think> may span chunk boundaries.
    let chunks = [
            data_sse(r#"{"id":"chatcmpl-2","model":"qwen3","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-2","model":"qwen3","choices":[{"index":0,"delta":{"content":"<think>rea"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-2","model":"qwen3","choices":[{"index":0,"delta":{"content":"soning</think>answer"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-2","model":"qwen3","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            data_sse("[DONE]"),
        ]
        .concat();

    let mut parser = OpenAIStreamParser::new();
    let mut deltas = parser.parse_chunk(&chunks).unwrap();
    deltas.extend(parser.finish().unwrap());

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
    let full_reasoning = reasoning.concat();
    assert!(
        full_reasoning.contains("reasoning"),
        "expected reasoning content in ThinkingDelta, got: {full_reasoning}"
    );

    let text: Vec<_> = deltas
        .iter()
        .filter_map(|d| {
            if let AiStreamDelta::TextDelta(t) = d {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        text.iter().any(|t| t.contains("answer")),
        "expected 'answer' in TextDelta, got: {text:?}"
    );
}

#[test]
fn test_stream_no_think_tags() {
    let chunks = [
            data_sse(r#"{"id":"chatcmpl-3","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-3","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-3","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            data_sse("[DONE]"),
        ]
        .concat();

    let mut parser = OpenAIStreamParser::new();
    let mut deltas = parser.parse_chunk(&chunks).unwrap();
    deltas.extend(parser.finish().unwrap());

    let has_text = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::TextDelta(t) if t.contains("hello")));
    let has_reasoning = deltas
        .iter()
        .any(|d| matches!(d, AiStreamDelta::ThinkingDelta(_)));
    assert!(has_text, "expected TextDelta('hello'), got: {deltas:?}");
    assert!(
        !has_reasoning,
        "should not have ThinkingDelta when no think tags, got: {deltas:?}"
    );
}
#[test]
fn test_extract_reasoning_mlx_field_name() {
    // mlx-lm uses "reasoning" instead of "reasoning_content".
    // Both field names must produce a reasoning delta.
    let msg =
        serde_json::json!({"role": "assistant", "content": "answer", "reasoning": "my reasoning"});
    let extracted = extract_reasoning_from_message(&msg);
    assert_eq!(
        extracted.as_deref(),
        Some("my reasoning"),
        "extract_reasoning_from_message must accept 'reasoning' field name (mlx-lm compat)"
    );
}

#[test]
fn test_parse_response_with_reasoning_field() {
    // Non-streaming response from mlx-lm: message has "reasoning" not "reasoning_content".
    let resp = serde_json::json!({
        "id": "chatcmpl-mlx",
        "model": "qwen3-35b",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "final answer",
                "reasoning": "step by step thinking"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    });
    let r = OpenAIResponseParser.parse_response(resp).unwrap();
    assert_eq!(r.output_text(), "final answer");
    assert_eq!(
        r.reasoning_items().next().map(|(text, _)| text),
        Some("step by step thinking"),
        "parse_response must extract reasoning from 'reasoning' field (mlx-lm compat)"
    );
}

#[test]
fn test_format_response_includes_reasoning_content() {
    // The response formatter must emit reasoning_content when it is present.
    let mut internal = AiResponse::new("chatcmpl-test", "qwen3");
    internal.push_output_text("visible text".to_string());
    internal.push_reasoning("hidden chain of thought", None);
    internal.stop_reason = Some("stop".to_string());
    internal.usage = Usage {
        prompt_tokens: 10,
        completion_tokens: 5,
        ..Usage::default()
    };
    let formatted = OpenAIResponseFormatter.format_response(&internal);
    let msg = &formatted["choices"][0]["message"];
    assert_eq!(msg["content"].as_str(), Some("visible text"));
    assert_eq!(
        msg["reasoning_content"].as_str(),
        Some("hidden chain of thought"),
        "format_response must include reasoning_content in the message"
    );
}

#[test]
fn test_stream_reasoning_field_from_mlx() {
    // Streaming SSE chunks from mlx-lm use "reasoning" in the delta.
    let chunks = [
            data_sse(r#"{"id":"chatcmpl-mlx","model":"qwen3","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-mlx","model":"qwen3","choices":[{"index":0,"delta":{"content":"final ","reasoning":"thinking"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-mlx","model":"qwen3","choices":[{"index":0,"delta":{"content":"answer","reasoning":" done"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-mlx","model":"qwen3","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            data_sse("[DONE]"),
        ]
        .concat();

    let mut parser = OpenAIStreamParser::new();
    let mut deltas = parser.parse_chunk(&chunks).unwrap();
    deltas.extend(parser.finish().unwrap());

    let reasoning: String = deltas
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
        reasoning.contains("thinking"),
        "expected 'thinking' in ThinkingDelta, got: {reasoning}"
    );
    assert!(
        reasoning.contains("done"),
        "expected 'done' in ThinkingDelta, got: {reasoning}"
    );

    let text: String = deltas
        .iter()
        .filter_map(|d| {
            if let AiStreamDelta::TextDelta(t) = d {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        text.contains("final answer"),
        "expected 'final answer' in TextDelta, got: {text:?}"
    );
}

#[test]
fn test_stream_finish_reason_empty_string_ignored() {
    let chunks = [
            data_sse(r#"{"id":"chatcmpl-zh","model":"claude-opus-4p7","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":""}]}"#),
            data_sse(r#"{"id":"chatcmpl-zh","model":"claude-opus-4p7","choices":[{"index":0,"delta":{"content":"Hello!"},"finish_reason":""}]}"#),
            data_sse(r#"{"id":"chatcmpl-zh","model":"claude-opus-4p7","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            data_sse("[DONE]"),
        ]
        .concat();

    let mut parser = OpenAIStreamParser::new();
    let deltas = parser.parse_chunk(&chunks).unwrap();

    let done_count = deltas
        .iter()
        .filter(|d| matches!(d, AiStreamDelta::Done { .. }))
        .count();
    assert_eq!(
        done_count, 1,
        "expected exactly 1 Done, got {done_count}: {deltas:?}"
    );

    let done = deltas
        .iter()
        .find_map(|d| {
            if let AiStreamDelta::Done { stop_reason } = d {
                Some(stop_reason.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(done, "stop", "Done stop_reason must be 'stop'");
}

#[test]
fn test_stream_duplicate_done_only_one_emitted() {
    let chunks = [
            data_sse(r#"{"id":"chatcmpl-mi","model":"mimo-v2.5","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-mi","model":"mimo-v2.5","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#),
            data_sse(r#"{"id":"chatcmpl-mi","model":"mimo-v2.5","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            data_sse("[DONE]"),
        ]
        .concat();

    let mut parser = OpenAIStreamParser::new();
    let deltas = parser.parse_chunk(&chunks).unwrap();

    let done_count = deltas
        .iter()
        .filter(|d| matches!(d, AiStreamDelta::Done { .. }))
        .count();
    assert_eq!(
        done_count, 1,
        "expected exactly 1 Done (finish_reason + [DONE] deduped), got {done_count}: {deltas:?}"
    );
}

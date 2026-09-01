use super::*;

#[test]
fn openai_to_responses_reasoning_and_function_call_items() {
    let mut resp = IrAiResponse::new("resp_1", "minimax-m2.7");
    resp.push_reasoning("chain", None);
    resp.extend_tool_calls(vec![ToolCall {
        id: "call_123".to_string(),
        name: "ls".to_string(),
        arguments: "{\"path\":\".\"}".to_string(),
    }]);
    resp.push_output_text("done");
    resp.stop_reason = Some("stop".to_string());

    let out = ResponsesResponseFormatter.format_response(&resp);
    let output = out
        .get("output")
        .and_then(|v| v.as_array())
        .expect("output should be array");
    assert!(
        output
            .iter()
            .any(|item| item.get("type").and_then(|v| v.as_str()) == Some("reasoning"))
    );
    assert!(
        output
            .iter()
            .any(|item| item.get("type").and_then(|v| v.as_str()) == Some("function_call"))
    );
    assert!(
        output
            .iter()
            .any(|item| item.get("type").and_then(|v| v.as_str()) == Some("message"))
    );
}
#[test]
fn responses_decoder_ignores_empty_message_content_item() {
    let body = serde_json::json!({
        "model": "MiniMax-M2.7-Code-Claude",
        "input": [
            { "type": "message", "role": "user", "content": [] },
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "帮我查看当前目录下有哪些文件" }]
            }
        ]
    });

    let req = ResponsesDecoder
        .decode_request(body)
        .expect("decode request should succeed");
    assert_eq!(req.items.len(), 1);
    assert_eq!(req.items[0].role, IrRole::User);
    assert_eq!(
        req.items[0].content.to_text(),
        "帮我查看当前目录下有哪些文件"
    );
}
#[test]
fn responses_encoder_targets_slash_v1_responses_and_preserves_stream_choice() {
    let req = responses_request(
        vec![AiItem {
            role: IrRole::User,
            content: IrMessageContent::Text("hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
        false,
    );

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    assert_eq!(body.get("stream").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        body.get("store").and_then(|v| v.as_bool()),
        Some(true),
        "Open Responses defaults to local persistence"
    );
    assert_eq!(
        ResponsesEncoder.egress_path("gpt-5.4", false),
        "/v1/responses"
    );
}
#[test]
fn responses_encoder_keeps_instructions_distinct_from_system_messages() {
    let mut req = responses_request(
        vec![
            AiItem {
                role: IrRole::System,
                content: IrMessageContent::Text("system context".to_string()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: IrRole::User,
                content: IrMessageContent::Text("hi".to_string()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
        false,
    );
    req.instructions = Some("request instructions".into());

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    assert_eq!(
        body.get("instructions").and_then(|v| v.as_str()),
        Some("request instructions")
    );
    let input = body.get("input").and_then(|v| v.as_array()).expect("input");
    assert_eq!(input.len(), 2);
    assert_eq!(
        input[0].get("role").and_then(|v| v.as_str()),
        Some("system")
    );
    assert_eq!(input[1].get("role").and_then(|v| v.as_str()), Some("user"));
}
#[test]
fn responses_encoder_emits_function_call_and_function_call_output_items() {
    let req = responses_request(
        vec![
            AiItem {
                role: IrRole::Assistant,
                content: IrMessageContent::Text(String::new()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_abc".to_string(),
                    name: "list_dir".to_string(),
                    arguments: "{\"path\":\".\"}".to_string(),
                }]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: IrRole::Tool,
                content: IrMessageContent::Text("file1\nfile2".to_string()),
                tool_calls: None,
                tool_call_id: Some("call_abc".to_string()),
                meta: None,
            },
        ],
        false,
    );

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    let input = body.get("input").and_then(|v| v.as_array()).expect("input");
    assert_eq!(
        input.len(),
        2,
        "one function_call + one function_call_output"
    );

    assert_eq!(
        input[0].get("type").and_then(|v| v.as_str()),
        Some("function_call")
    );
    assert_eq!(
        input[0].get("call_id").and_then(|v| v.as_str()),
        Some("call_abc")
    );
    assert_eq!(
        input[0].get("name").and_then(|v| v.as_str()),
        Some("list_dir")
    );
    assert_eq!(
        input[0].get("arguments").and_then(|v| v.as_str()),
        Some("{\"path\":\".\"}"),
    );

    assert_eq!(
        input[1].get("type").and_then(|v| v.as_str()),
        Some("function_call_output")
    );
    assert_eq!(
        input[1].get("call_id").and_then(|v| v.as_str()),
        Some("call_abc")
    );
    assert_eq!(
        input[1].get("output").and_then(|v| v.as_str()),
        Some("file1\nfile2")
    );
}
#[test]
fn responses_encoder_preserves_max_output_tokens() {
    let mut req = responses_request(
        vec![AiItem {
            role: IrRole::User,
            content: IrMessageContent::Text("hi".to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
        false,
    );
    req.generation.max_tokens = Some(128);

    let (body, _) = ResponsesEncoder.encode_request(&req).expect("encode");
    assert_eq!(
        body.get("max_output_tokens")
            .and_then(|value| value.as_u64()),
        Some(128)
    );
}
#[test]
fn responses_stream_parser_extracts_text_and_usage() {
    let output_item = json!({
        "id": "msg_1",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "Hello", "annotations": []}]
    });
    let created = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_1",
        "gpt-5.4",
        "in_progress",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let completed = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_1",
        "gpt-5.4",
        "completed",
        vec![output_item.clone()],
        Value::Null,
        Value::Null,
        json!({
            "input_tokens": 7,
            "output_tokens": 2,
            "total_tokens": 9,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }),
    );
    let sse = [
        responses_sse_event("response.created", 0, json!({"response": created})),
        responses_sse_event(
            "response.output_item.added",
            1,
            json!({"output_index": 0, "item": {"id": "msg_1", "type": "message", "status": "in_progress", "role": "assistant", "content": []}}),
        ),
        responses_sse_event(
            "response.content_part.added",
            2,
            json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []}}),
        ),
        responses_sse_event(
            "response.output_text.delta",
            3,
            json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "delta": "Hel"}),
        ),
        responses_sse_event(
            "response.output_text.delta",
            4,
            json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "delta": "lo"}),
        ),
        responses_sse_event(
            "response.content_part.done",
            5,
            json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "part": {"type": "output_text", "text": "Hello", "annotations": [], "logprobs": []}}),
        ),
        responses_sse_event(
            "response.output_item.done",
            6,
            json!({"output_index": 0, "item": output_item}),
        ),
        responses_sse_event(
            "response.completed",
            7,
            json!({"response": completed}),
        ),
    ]
    .concat();

    let mut parser = ResponsesStreamParser::new();
    let deltas = parser.parse_chunk(&sse).expect("parse");

    let mut saw_start = false;
    let mut text_concat = String::new();
    let mut usage_input = 0;
    let mut usage_output = 0;
    let mut done_reason: Option<String> = None;

    for delta in &deltas {
        match delta {
            IrStreamDelta::MessageStart { id, model } => {
                saw_start = true;
                assert_eq!(id, "resp_1");
                assert_eq!(model, "gpt-5.4");
            }
            IrStreamDelta::TextDelta(t) => text_concat.push_str(t),
            IrStreamDelta::TextDeltaWithMetadata { text, .. } => {
                text_concat.push_str(text);
            }
            IrStreamDelta::Usage(u) => {
                usage_input = u.prompt_tokens;
                usage_output = u.completion_tokens;
            }
            IrStreamDelta::Done { stop_reason } => done_reason = Some(stop_reason.clone()),
            _ => {}
        }
    }

    assert!(saw_start);
    assert_eq!(text_concat, "Hello");
    assert_eq!(usage_input, 7);
    assert_eq!(usage_output, 2);
    assert_eq!(done_reason.as_deref(), Some("stop"));
}
#[test]
fn responses_stream_parser_extracts_function_call() {
    let output_item = json!({
        "id": "fc_1",
        "type": "function_call",
        "status": "completed",
        "call_id": "call_xyz",
        "name": "ls",
        "arguments": "{\"a\":1}"
    });
    let created = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_1",
        "gpt-5.4",
        "in_progress",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let completed = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_1",
        "gpt-5.4",
        "completed",
        vec![output_item.clone()],
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let sse = [
        responses_sse_event("response.created", 0, json!({"response": created})),
        responses_sse_event(
            "response.output_item.added",
            1,
            json!({"output_index": 0, "item": {"id": "fc_1", "type": "function_call", "status": "in_progress", "call_id": "call_xyz", "name": "ls", "arguments": ""}}),
        ),
        responses_sse_event(
            "response.function_call_arguments.delta",
            2,
            json!({"output_index": 0, "item_id": "fc_1", "delta": "{\"a\":1"}),
        ),
        responses_sse_event(
            "response.function_call_arguments.delta",
            3,
            json!({"output_index": 0, "item_id": "fc_1", "delta": "}"}),
        ),
        responses_sse_event(
            "response.output_item.done",
            4,
            json!({"output_index": 0, "item": output_item}),
        ),
        responses_sse_event(
            "response.completed",
            5,
            json!({"response": completed}),
        ),
    ]
    .concat();

    let mut parser = ResponsesStreamParser::new();
    let deltas = parser.parse_chunk(&sse).expect("parse");

    let mut got_start = false;
    let mut arg_concat = String::new();
    for delta in &deltas {
        match delta {
            IrStreamDelta::ToolCallStart { id, name, .. } => {
                got_start = true;
                assert_eq!(id, "call_xyz");
                assert_eq!(name, "ls");
            }
            IrStreamDelta::ToolCallDelta { arguments, .. } => arg_concat.push_str(arguments),
            _ => {}
        }
    }
    assert!(got_start);
    assert_eq!(arg_concat, "{\"a\":1}");
}
#[test]
fn responses_response_parser_extracts_text_tool_calls_and_usage() {
    let body = serde_json::json!({
        "id": "resp_42",
        "object": "response",
        "created_at": 1,
        "completed_at": 2,
        "model": "gpt-5.4",
        "status": "completed",
        "incomplete_details": null,
        "previous_response_id": null,
        "instructions": null,
        "output": [
            {
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Hi ", "annotations": []},
                    {"type": "output_text", "text": "there", "annotations": []}
                ]
            },
            {
                "id": "fc_1",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_1",
                "name": "search",
                "arguments": "{\"q\":\"rust\"}"
            }
        ],
        "error": null,
        "tools": [],
        "tool_choice": "auto",
        "truncation": "disabled",
        "parallel_tool_calls": true,
        "text": {"format": {"type": "text"}},
        "top_p": null,
        "presence_penalty": null,
        "frequency_penalty": null,
        "top_logprobs": null,
        "temperature": null,
        "reasoning": null,
        "usage": {
            "input_tokens": 11,
            "output_tokens": 3,
            "total_tokens": 14,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        },
        "max_output_tokens": null,
        "max_tool_calls": null,
        "store": false,
        "background": false,
        "service_tier": "default",
        "metadata": {},
        "safety_identifier": null,
        "prompt_cache_key": null
    });

    let resp = ResponsesResponseParser.parse_response(body).expect("parse");

    assert_eq!(resp.id, "resp_42");
    assert_eq!(resp.model, "gpt-5.4");
    assert_eq!(resp.output_text(), "Hi there");
    assert_eq!(resp.stop_reason.as_deref(), Some("tool_calls"));
    assert_eq!(resp.usage.prompt_tokens, 11);
    assert_eq!(resp.usage.completion_tokens, 3);
    assert_eq!(resp.tool_calls().count(), 1);
    let call = resp.tool_calls().next().expect("function call");
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "search");
    assert_eq!(call.arguments, "{\"q\":\"rust\"}");
}
#[test]
fn responses_decoder_rejects_empty_function_call_names() {
    let error = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt-5.4",
            "input": [{
                "type": "function_call",
                "call_id": "call_aec52c641b094ce0aae3ce3cc526068c",
                "name": "",
                "arguments": "{\"cmd\":\"git status --short\"}"
            }]
        }))
        .expect_err("dated protocol requires a non-empty function name");

    assert!(error.to_string().contains("non-empty call_id and name"));
}

use super::*;

#[tokio::test]
async fn responses_thinking_paragraphs_replay_original_parts_through_chat() {
    use axum::Json;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    fn snapshot(status: &str, output: Vec<Value>) -> Value {
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            "resp-paragraphs",
            "provider-model",
            status,
            output,
            Value::Null,
            Value::Null,
            Value::Null,
        )
    }

    fn sse(items: &[Value]) -> String {
        let mut events = vec![json!({
            "type": "response.created", "response": snapshot("in_progress", Vec::new())
        })];
        for (output_index, item) in items.iter().enumerate() {
            let mut started = item.clone();
            started["summary"] = json!([]);
            started["content"] = json!([]);
            events.push(json!({
                "type": "response.output_item.added", "output_index": output_index,
                "item": started
            }));
            for (summary_index, part) in item["summary"].as_array().unwrap().iter().enumerate() {
                events.push(json!({
                    "type": "response.reasoning_summary_part.added", "output_index": output_index,
                    "item_id": item["id"], "summary_index": summary_index,
                    "part": {"type": "summary_text", "text": ""}
                }));
                let text = part["text"].as_str().unwrap();
                // An empty delta must not make a paragraph; splitting a bold heading must
                // not create a boundary inside the same semantic part either.
                for delta in [&text[..4], "", &text[4..]] {
                    events.push(json!({
                        "type": "response.reasoning_summary_text.delta", "output_index": output_index,
                        "item_id": item["id"], "summary_index": summary_index, "delta": delta
                    }));
                }
                events.push(json!({
                    "type": "response.reasoning_summary_text.done", "output_index": output_index,
                    "item_id": item["id"], "summary_index": summary_index, "text": text
                }));
                events.push(json!({
                    "type": "response.reasoning_summary_part.done", "output_index": output_index,
                    "item_id": item["id"], "summary_index": summary_index, "part": part
                }));
            }
            events.push(json!({
                "type": "response.output_item.done", "output_index": output_index, "item": item
            }));
        }
        events.push(json!({"type": "response.completed", "response": snapshot("completed", items.to_vec())}));
        let mut body = String::new();
        for (sequence, mut event) in events.into_iter().enumerate() {
            event["sequence_number"] = json!(sequence);
            body.push_str(&format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            ));
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    #[derive(Clone)]
    struct Fixture {
        requests: Arc<Mutex<Vec<Value>>>,
        items: Vec<Value>,
    }

    async fn handle(State(fixture): State<Fixture>, Json(body): Json<Value>) -> Response {
        let first = {
            let mut requests = fixture
                .requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            requests.push(body.clone());
            requests.len() == 1
        };
        let items = if first { fixture.items } else { Vec::new() };
        if body["stream"] == true {
            ([(header::CONTENT_TYPE, "text/event-stream")], sse(&items)).into_response()
        } else {
            Json(snapshot("completed", items)).into_response()
        }
    }

    // Ignore transport-only HTML comments, not source whitespace. This models the
    // visible Markdown and also permits comparing independently generated references.
    fn visible(mut carrier: &str) -> String {
        let mut result = String::new();
        while let Some(start) = carrier.find("<!-- stravia-") {
            result.push_str(&carrier[..start]);
            let end = carrier[start..]
                .find(" -->")
                .expect("closed private comment")
                + start
                + 4;
            carrier = &carrier[end..];
        }
        result.push_str(carrier);
        result
    }

    let parts = [
        " \r\n**First heading**\r\nbody with trailing space ",
        "**Second heading**\r\nsecond body ",
        "Full reasoning\r\nwith original whitespace \n",
        "**Public heading**\r\npublic body\r\n ",
    ];
    let items = vec![
        json!({
            "type": "reasoning", "id": "rs_protected",
            "summary": [
                {"type": "summary_text", "text": parts[0]},
                {"type": "summary_text", "text": parts[1]}
            ],
            "content": [{"type": "reasoning_text", "text": parts[2]}],
            "encrypted_content": "opaque-paragraph-cipher"
        }),
        json!({
            "type": "reasoning", "id": "rs_public",
            "summary": [{"type": "summary_text", "text": parts[3]}],
            "content": []
        }),
    ];
    let mut displays = Vec::new();
    for stream in [true, false] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/responses", post(handle))
            .with_state(Fixture {
                requests: Arc::clone(&requests),
                items: items.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let data_dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
        let model = "thinking-paragraph-replay";
        configure_route_with_protocol(
            &gateway,
            model,
            &[format!("http://{address}/v1")],
            "test-http",
            "open-responses",
        )
        .await;
        let headers = authorized_headers(&gateway).await;
        let response = execute_protocol_request(
            gateway.clone(),
            model,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
            stream,
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let message = if stream {
            let mut reasoning = String::new();
            let mut content = String::new();
            for data in std::str::from_utf8(&body)
                .unwrap()
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
            {
                if data == "[DONE]" {
                    continue;
                }
                let event: Value = serde_json::from_str(data).expect("Chat SSE JSON");
                let delta = &event["choices"][0]["delta"];
                if let Some(text) = delta["reasoning_content"].as_str() {
                    reasoning.push_str(text);
                }
                if let Some(text) = delta["content"].as_str() {
                    content.push_str(text);
                }
            }
            json!({"role": "assistant", "reasoning_content": reasoning, "content": content})
        } else {
            let response: Value = serde_json::from_slice(&body).unwrap();
            response["choices"][0]["message"].clone()
        };
        let reasoning = message["reasoning_content"]
            .as_str()
            .expect("visible thinking");
        let displayed = visible(reasoning);
        let mut previous_end = None;
        for part in parts {
            let start = displayed.find(part).unwrap_or_else(|| panic!("source bytes changed or delta boundary inserted: stream={stream}, {displayed:?}"));
            if let Some(end) = previous_end {
                let gap: &str = &displayed[end..start];
                assert!(
                    gap.contains("\n\n"),
                    "independent thinking parts require a Markdown paragraph: stream={stream}, gap={gap:?}, {displayed:?}"
                );
            }
            previous_end = Some(start + part.len());
        }
        let carriers = format!(
            "{reasoning}{}",
            message["content"].as_str().unwrap_or_default()
        );
        assert_eq!(
            carriers
                .matches(crate::history_marker::HISTORY_MARKER_PREFIX)
                .count(),
            2,
            "one recoverable marker per independent block, not per summary part: {carriers}"
        );
        displays.push(displayed);

        let request = crate::protocol::transform::ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            )
            .unwrap()
            .decode_request(json!({
                "model": model,
                "messages": [
                    {"role": "user", "content": "test"}, message,
                    {"role": "user", "content": "continue"}
                ]
            }))
            .expect("decode the actual client assistant message");
        let response = execute_non_stream_request_with_headers(gateway, headers, request).await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let captured = requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            captured.len(),
            2,
            "one original generation and one continuation"
        );
        let input = captured[1]["input"].as_array().expect("Responses input");
        assert!(
            input
                .iter()
                .all(|item| item["type"] == "reasoning" || item["role"] == "user"),
            "display-only whitespace must not become an upstream assistant text item: {input:?}"
        );
        let replayed = input
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .collect::<Vec<_>>();
        assert_eq!(
            replayed.len(),
            items.len(),
            "independent reasoning blocks survive replay"
        );
        for (actual, expected) in replayed.iter().zip(&items) {
            assert_eq!(
                actual["summary"], expected["summary"],
                "exact summary parts, including CRLF and spaces"
            );
            assert_eq!(
                actual["content"], expected["content"],
                "no fabricated reasoning content"
            );
            assert_eq!(
                actual["encrypted_content"], expected["encrypted_content"],
                "exact protected cipher and public-block absence"
            );
        }
        let upstream = captured[1].to_string();
        assert!(
            !upstream.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
            "{upstream}"
        );
        assert!(
            !upstream.contains(crate::history_marker::PROJECTION_DELIMITER_PREFIX),
            "{upstream}"
        );
        server.abort();
    }
    assert_eq!(
        displays[0], displays[1],
        "streaming and unary Markdown agree"
    );
}

#[tokio::test]
async fn non_stream_projection_matches_ordered_content_and_replays_canonical_history() {
    let platform_round = serde_json::json!({
        "id": "chatcmpl-projected-platform",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "reasoning_content": "R1",
                "content": "C1",
                "tool_calls": [{
                    "id": "platform-call",
                    "type": "function",
                    "function": {
                        "name": "stravia__ordered_tool",
                        "arguments": "{\"index\":1}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let final_round = serde_json::json!({
        "id": "chatcmpl-projected-final",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "reasoning_content": "R2",
                "content": "C2"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 2,
            "completion_tokens": 2,
            "total_tokens": 4
        }
    });
    let (base_url, provider_calls, requests) = serve_openai_sequence_with_requests(vec![
        platform_round,
        final_round,
        openai_response("after replay"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "projected-platform", &[base_url]).await;
    let headers = authorized_headers(&gateway).await;

    let initial_request = AiRequest::new(
        "projected-platform",
        vec![stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Text("test".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let response =
        execute_non_stream_request_with_headers(gateway.clone(), headers.clone(), initial_request)
            .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("projected response body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("projected response JSON");
    let reasoning = body["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .expect("reasoning_content");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("content");
    let c1 = content.find("C1").expect("first Text");
    let platform_marker = content
        .find(crate::history_marker::HISTORY_MARKER_PREFIX)
        .expect("content-carried Platform Marker");
    let r2 = content.find("> R2").expect("quoted Post-Text Thinking");
    let thinking_marker = content
        [platform_marker + crate::history_marker::HISTORY_MARKER_PREFIX.len()..]
        .find(crate::history_marker::HISTORY_MARKER_PREFIX)
        .map(|offset| platform_marker + crate::history_marker::HISTORY_MARKER_PREFIX.len() + offset)
        .expect("content-carried Thinking Marker");
    let c2 = content.rfind("C2").expect("second Text");
    assert!(
        c1 < platform_marker
            && platform_marker < r2
            && r2 < thinking_marker
            && thinking_marker < c2,
        "{content}"
    );
    assert_eq!(
        content
            .matches(crate::history_marker::PROJECTION_DELIMITER_PREFIX)
            .count(),
        2,
        "{content}"
    );
    assert!(!content.contains(":text:"), "{content}");
    assert_eq!(
        content
            .matches(crate::history_marker::HISTORY_MARKER_PREFIX)
            .count(),
        2,
        "{content}"
    );

    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    let captured = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let second_body = captured[1]
        .split_once("\r\n\r\n")
        .expect("provider request body")
        .1;
    let second_body: serde_json::Value =
        serde_json::from_str(second_body).expect("provider request JSON");
    let messages = second_body["messages"]
        .as_array()
        .expect("provider messages");
    let reasoning_index = messages
        .iter()
        .position(|message| message["reasoning_content"] == "R1")
        .unwrap_or_else(|| panic!("canonical first reasoning message: {messages:?}"));
    let text_index = messages
        .iter()
        .position(|message| message["content"] == "C1")
        .expect("canonical platform prelude message");
    let call_index = messages
        .iter()
        .position(|message| {
            message["tool_calls"][0]["id"]
                .as_str()
                .is_some_and(|id| id == "platform-call")
        })
        .expect("canonical platform assistant message");
    let result_index = messages
        .iter()
        .position(|message| message["tool_call_id"] == "platform-call")
        .expect("canonical platform result message");
    assert!(
        reasoning_index < text_index && text_index < call_index && call_index < result_index,
        "{messages:?}"
    );
    assert!(
        !second_body
            .to_string()
            .contains(crate::history_marker::HISTORY_MARKER_PREFIX)
    );
    assert!(
        !second_body
            .to_string()
            .contains(crate::history_marker::PROJECTION_DELIMITER_PREFIX)
    );
    drop(captured);

    let replay_request = crate::protocol::registry::ProtocolRegistry::global()
        .adapter(&OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .expect("OpenAI Chat adapter")
        .decode_request(serde_json::json!({
            "model": "projected-platform",
            "messages": [
                {"role": "user", "content": "test"},
                {
                    "role": "assistant",
                    "reasoning_content": reasoning,
                    "content": content
                },
                {"role": "user", "content": "continue"}
            ]
        }))
        .expect("client replay request");
    let replay_response =
        execute_non_stream_request_with_headers(gateway, headers, replay_request).await;
    if replay_response.status() != StatusCode::OK {
        let status = replay_response.status();
        let error = to_bytes(replay_response.into_body(), usize::MAX)
            .await
            .expect("replay error body");
        panic!("{status}: {}", String::from_utf8_lossy(&error));
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);

    let captured = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let replay_body = captured[2]
        .split_once("\r\n\r\n")
        .expect("replay provider request body")
        .1;
    let replay_body: serde_json::Value =
        serde_json::from_str(replay_body).expect("replay provider request JSON");
    let replay_messages = replay_body["messages"]
        .as_array()
        .expect("replay provider messages");
    let message_text = |message: &serde_json::Value| {
        message["content"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                message["content"].as_array().map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>()
                })
            })
            .unwrap_or_default()
    };
    let r1 = replay_messages
        .iter()
        .position(|message| message["reasoning_content"] == "R1")
        .expect("replayed R1");
    let c1 = replay_messages
        .iter()
        .position(|message| message_text(message) == "C1")
        .unwrap_or_else(|| panic!("replayed C1: {replay_messages:?}"));
    let call = replay_messages
        .iter()
        .position(|message| message["tool_calls"][0]["id"] == "platform-call")
        .expect("replayed Platform ToolCall");
    let result = replay_messages
        .iter()
        .position(|message| message["tool_call_id"] == "platform-call")
        .expect("replayed Platform ToolResult");
    let r2 = replay_messages
        .iter()
        .position(|message| message["reasoning_content"] == "R2")
        .expect("replayed R2");
    let c2 = replay_messages
        .iter()
        .position(|message| message_text(message) == "C2")
        .expect("replayed C2");
    assert!(
        r1 < c1 && c1 < call && call < result && result < r2 && r2 <= c2,
        "{replay_messages:?}"
    );
    let replay_wire = replay_body.to_string();
    assert!(
        !replay_wire.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{replay_wire}"
    );
    assert!(
        !replay_wire.contains(crate::history_marker::PROJECTION_DELIMITER_PREFIX),
        "{replay_wire}"
    );
}

#[tokio::test]
async fn failed_platform_call_and_successful_retry_preserve_marker_and_result_order() {
    let platform_round = |id: &str, reasoning: &str, text: &str, call_id: &str, index: u64| {
        serde_json::json!({
            "id": id,
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "reasoning_content": reasoning,
                    "content": text,
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": "stravia__ordered_tool",
                            "arguments": serde_json::json!({"index": index}).to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        })
    };
    let (base_url, provider_calls, requests) = serve_openai_sequence_with_requests(vec![
        platform_round("retry-1", "R1", "attempt one", "platform-call-1", 1),
        platform_round("retry-2", "R2", "attempt two", "platform-call-2", 2),
        serde_json::json!({
            "id": "retry-final",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "reasoning_content": "R3",
                    "content": "final answer"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        }),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(RetryingOrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "platform-retry-order", &[base_url]).await;

    let response = execute_non_stream(gateway, "platform-retry-order").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("retry response body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("retry response JSON");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("retry content");
    let marker_positions = content
        .match_indices(crate::history_marker::HISTORY_MARKER_PREFIX)
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    assert_eq!(marker_positions.len(), 4, "{content}");
    let first_attempt = content
        .find("attempt one")
        .expect("first attempt narration");
    let second_reasoning = content.find("> R2").expect("retry reasoning");
    let second_attempt = content.find("attempt two").expect("retry narration");
    let final_reasoning = content.find("> R3").expect("final reasoning");
    let final_answer = content.find("final answer").expect("final answer");
    assert!(
        first_attempt < marker_positions[0]
            && marker_positions[0] < second_reasoning
            && second_reasoning < marker_positions[1]
            && marker_positions[1] < second_attempt
            && second_attempt < marker_positions[2]
            && marker_positions[2] < final_reasoning
            && final_reasoning < marker_positions[3]
            && marker_positions[3] < final_answer,
        "{content}"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1, 2]
    );

    let captured = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let third_body = captured[2]
        .split_once("\r\n\r\n")
        .expect("third provider request body")
        .1;
    let third_body: serde_json::Value =
        serde_json::from_str(third_body).expect("third provider request JSON");
    let messages = third_body["messages"]
        .as_array()
        .expect("third provider messages");
    let call_1 = messages
        .iter()
        .position(|message| message["tool_calls"][0]["id"] == "platform-call-1")
        .expect("first Platform ToolCall");
    let result_1 = messages
        .iter()
        .position(|message| message["tool_call_id"] == "platform-call-1")
        .expect("failed Platform ToolResult");
    let call_2 = messages
        .iter()
        .position(|message| message["tool_calls"][0]["id"] == "platform-call-2")
        .expect("retry Platform ToolCall");
    let result_2 = messages
        .iter()
        .position(|message| message["tool_call_id"] == "platform-call-2")
        .expect("successful Platform ToolResult");
    assert!(
        call_1 < result_1 && result_1 < call_2 && call_2 < result_2,
        "{messages:?}"
    );
}

#[tokio::test]
async fn platform_stream_projects_post_text_thinking_into_ordered_content() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse_projected_platform_leg(),
        openai_sse_reasoning_and_text("R2", "C2", 22, 2),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "projected-platform-stream", &[base_url]).await;

    let response = execute_stream(gateway, "projected-platform-stream").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("projected stream body");
    let body = String::from_utf8(body.to_vec()).expect("UTF-8 stream");
    let mut runs = Vec::<(&str, String)>::new();
    for event in body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
    {
        let delta = &event["choices"][0]["delta"];
        let next = delta["reasoning_content"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| ("reasoning", text))
            .or_else(|| {
                delta["content"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .map(|text| ("content", text))
            });
        let Some((kind, text)) = next else {
            continue;
        };
        if let Some((last_kind, bytes)) = runs.last_mut()
            && *last_kind == kind
        {
            bytes.push_str(text);
        } else {
            runs.push((kind, text.to_owned()));
        }
    }

    assert_eq!(
        runs.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
        vec!["reasoning", "content"],
        "{body}"
    );
    assert!(
        runs[1]
            .1
            .contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{body}"
    );
    assert_eq!(
        runs[1]
            .1
            .matches(crate::history_marker::PROJECTION_DELIMITER_PREFIX)
            .count(),
        2,
        "one Post-Text Thinking block must use one Preview delimiter pair: {body}"
    );
    let content = &runs[1].1;
    let c1 = content.find("C1").expect("first Text");
    let platform_marker = content
        .find(crate::history_marker::HISTORY_MARKER_PREFIX)
        .expect("Platform Marker");
    let preview = content.find("> R2").expect("quoted Thinking preview");
    let thinking_marker = content
        [platform_marker + crate::history_marker::HISTORY_MARKER_PREFIX.len()..]
        .find(crate::history_marker::HISTORY_MARKER_PREFIX)
        .map(|offset| platform_marker + crate::history_marker::HISTORY_MARKER_PREFIX.len() + offset)
        .expect("Thinking Marker");
    let c2 = content.rfind("C2").expect("second Text");
    assert!(
        c1 < platform_marker
            && platform_marker < preview
            && preview < thinking_marker
            && thinking_marker < c2,
        "{body}"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn post_text_thinking_without_tool_call_gets_its_own_ordered_marker() {
    let (base_url, provider_calls) =
        serve_sse_sequence(vec![openai_sse_text_thinking_text()]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::new(std::sync::Mutex::new(Vec::new())),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "post-text-thinking", &[base_url]).await;

    let response = execute_stream(gateway, "post-text-thinking").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Post-Text Thinking stream body");
    let body = String::from_utf8(body.to_vec()).expect("UTF-8 stream");
    let content = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|event| {
            event["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<String>();

    let c1 = content.find("C1").expect("first Text");
    let preview = content.find("> R2").expect("quoted Thinking preview");
    let marker = content
        .find(crate::history_marker::HISTORY_MARKER_PREFIX)
        .expect("Thinking Marker");
    let c2 = content.rfind("C2").expect("second Text");
    assert!(c1 < preview && preview < marker && marker < c2, "{body}");
    assert_eq!(
        content
            .matches(crate::history_marker::HISTORY_MARKER_PREFIX)
            .count(),
        1,
        "{body}"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn platform_stream_projection_matrix_for_registered_generation_ingresses() {
    let protocols = [
        (
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
        ),
        (OPEN_RESPONSES_2026_04_24, "/v1/responses"),
        (ANTHROPIC_MESSAGES_2023_06_01, "/v1/messages"),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            "/v1beta/models/test:streamGenerateContent",
        ),
    ];
    let responses = protocols
        .iter()
        .flat_map(|_| {
            [
                openai_sse_projected_platform_leg(),
                openai_sse_reasoning_and_text("R2", "C2", 22, 2),
            ]
        })
        .collect::<Vec<_>>();
    let (base_url, provider_calls) = serve_sse_sequence(responses).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "platform-stream-matrix", &[base_url]).await;

    for (ingress, path) in protocols {
        let response = execute_protocol_request(
            gateway.clone(),
            "platform-stream-matrix",
            ingress,
            path,
            true,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{ingress}");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("protocol stream body");
        let body = String::from_utf8(body.to_vec()).expect("UTF-8 stream");
        let mut runs = Vec::<(&str, String)>::new();
        for event in body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        {
            let next = if ingress == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
                let delta = &event["choices"][0]["delta"];
                delta["reasoning_content"]
                    .as_str()
                    .map(|text| ("reasoning", text))
                    .or_else(|| delta["content"].as_str().map(|text| ("content", text)))
            } else if ingress == OPEN_RESPONSES_2026_04_24 {
                match event["type"].as_str() {
                    Some("response.reasoning.delta")
                    | Some("response.reasoning_text.delta")
                    | Some("response.reasoning_summary_text.delta") => {
                        event["delta"].as_str().map(|text| ("reasoning", text))
                    }
                    Some("response.output_text.delta") => {
                        event["delta"].as_str().map(|text| ("content", text))
                    }
                    _ => None,
                }
            } else if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
                match event
                    .pointer("/delta/type")
                    .and_then(serde_json::Value::as_str)
                {
                    Some("thinking_delta") => event
                        .pointer("/delta/thinking")
                        .and_then(serde_json::Value::as_str)
                        .map(|text| ("reasoning", text)),
                    Some("text_delta") => event
                        .pointer("/delta/text")
                        .and_then(serde_json::Value::as_str)
                        .map(|text| ("content", text)),
                    _ => None,
                }
            } else {
                let part = event.pointer("/candidates/0/content/parts/0");
                part.and_then(|part| {
                    part["text"].as_str().map(|text| {
                        if part["thought"].as_bool() == Some(true) {
                            ("reasoning", text)
                        } else {
                            ("content", text)
                        }
                    })
                })
            };
            let Some((kind, text)) = next.filter(|(_, text)| !text.is_empty()) else {
                continue;
            };
            if let Some((last_kind, bytes)) = runs.last_mut()
                && *last_kind == kind
            {
                bytes.push_str(text);
            } else {
                runs.push((kind, text.to_owned()));
            }
        }

        if ingress == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
            assert_eq!(
                runs.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
                vec!["reasoning", "content"],
                "{ingress}: {body}"
            );
            assert!(
                runs[1].1.contains("C1")
                    && runs[1]
                        .1
                        .contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                    && runs[1].1.contains("> R2")
                    && runs[1].1.ends_with("C2"),
                "{ingress}: {body}"
            );
        } else {
            assert_eq!(
                runs.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
                vec!["reasoning", "content", "reasoning", "content"],
                "{ingress}: {body}"
            );
            assert_eq!(runs[0].1, "R1", "{ingress}: {body}");
            assert_eq!(runs[1].1, "C1", "{ingress}: {body}");
            assert!(
                runs[2]
                    .1
                    .contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                    && runs[2].1.contains("R2"),
                "{ingress}: {body}"
            );
            assert_eq!(runs[3].1, "C2", "{ingress}: {body}");
        }
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 8);
    assert_eq!(
        tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len(),
        4
    );
}

#[tokio::test]
async fn exposed_platform_tools_preserve_visible_text_and_thinking_order() {
    let (base_url, provider_calls) =
        serve_sse_sequence(vec![openai_sse_reasoning_and_text("R1", "C1", 11, 2)]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "non-platform-stream", &[base_url]).await;

    let response = execute_stream(gateway, "non-platform-stream").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("non-Platform stream body");
    let body = String::from_utf8(body.to_vec()).expect("UTF-8 stream");
    let visible = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|event| {
            let delta = &event["choices"][0]["delta"];
            delta["reasoning_content"]
                .as_str()
                .map(|text| ("reasoning", text.to_owned()))
                .or_else(|| {
                    delta["content"]
                        .as_str()
                        .map(|text| ("content", text.to_owned()))
                })
        })
        .filter(|(_, text)| !text.is_empty())
        .collect::<Vec<_>>();

    let text_start = visible
        .iter()
        .position(|(kind, _)| *kind == "content")
        .unwrap();
    assert!(
        visible[..text_start]
            .iter()
            .all(|(kind, _)| *kind == "reasoning")
    );
    assert!(
        visible[..text_start]
            .iter()
            .any(|(_, text)| text.contains("R1"))
    );
    assert_eq!(
        &visible[text_start..],
        &[("content", "C1".to_string())],
        "{body}"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(
        tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    );
}

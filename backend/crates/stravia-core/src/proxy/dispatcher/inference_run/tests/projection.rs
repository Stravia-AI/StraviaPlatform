use super::*;

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
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
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
        vec![crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::User,
            content: crate::protocol::ir::MessageContent::Text("test".into()),
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
    assert_eq!(reasoning, "R1");
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
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
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
    let reasoning = body["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .expect("retry reasoning_content");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("retry content");
    assert_eq!(reasoning, "R1");
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
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
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
    assert_eq!(runs[0].1, "R1", "{body}");
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
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
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
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
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
            assert_eq!(runs[0].1, "R1", "{ingress}: {body}");
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
async fn exposed_platform_tools_preserve_non_platform_stream_order_and_bytes() {
    let (base_url, provider_calls) =
        serve_sse_sequence(vec![openai_sse_reasoning_and_text("R1", "C1", 11, 2)]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
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

    assert_eq!(
        visible,
        vec![
            ("reasoning", "R1".to_string()),
            ("content", "C1".to_string())
        ],
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

use super::*;

/// Regression: Open Responses replays history as separate items — a standalone
/// reasoning item, one item per function_call, then the outputs. The reasoning
/// item must not be encoded as an assistant message with neither `content` nor
/// `tool_calls`: DeepSeek (and strict chat-completions upstreams) reject it with
/// 400 "Invalid assistant message: content or tool_calls must be set". Its
/// reasoning text must merge onto the following assistant turn instead.
#[test]
fn standalone_reasoning_item_does_not_become_contentless_assistant_message() {
    let assistant_call = |id: &str, name: &str| AiItem {
        role: Role::Assistant,
        content: MessageContent::Text(String::new()),
        tool_calls: Some(vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    let tool_output = |id: &str, text: &str| AiItem {
        role: Role::Tool,
        content: MessageContent::Text(text.into()),
        tool_calls: None,
        tool_call_id: Some(id.into()),
        meta: None,
    };
    let request = AiRequest::new(
        "deepseek-flash",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("修复这个问题".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem::reasoning(Vec::new(), vec!["inspect the repo first".into()], None),
            assistant_call("call_00", "bash"),
            assistant_call("call_01", "glob"),
            tool_output("call_01", "scratch listing"),
            tool_output("call_00", "git status output"),
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    assert_replayed_reasoning_is_valid(body);

    // Release shape: replay keeps preserved native reasoning as Thinking
    // blocks (see `transform/replay.rs`), which `encode_message` strips from
    // content — the exact message DeepSeek rejected in production.
    let request = AiRequest::new(
        "deepseek-flash",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("修复这个问题".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem::thinking("inspect the repo first", None),
            assistant_call("call_00", "bash"),
            assistant_call("call_01", "glob"),
            tool_output("call_01", "scratch listing"),
            tool_output("call_00", "git status output"),
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    assert_replayed_reasoning_is_valid(body);
}

fn assert_replayed_reasoning_is_valid(body: serde_json::Value) {
    let messages = body["messages"].as_array().expect("messages array");

    let mut reasoning_merged = false;
    for message in messages {
        if message["role"] != "assistant" {
            continue;
        }
        let has_content = message.get("content").is_some_and(|c| !c.is_null());
        let has_calls = message
            .get("tool_calls")
            .is_some_and(|c| c.as_array().is_some_and(|a| !a.is_empty()));
        assert!(
            has_content || has_calls,
            "assistant message without content or tool_calls: {message}"
        );
        if message.get("reasoning_content").and_then(|c| c.as_str())
            == Some("inspect the repo first")
        {
            reasoning_merged = true;
        }
    }
    assert!(
        reasoning_merged,
        "standalone reasoning text must survive on a following assistant message: {messages:?}"
    );
}

#[test]
fn canonical_system_is_encoded_as_a_system_message() {
    let mut request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.instructions = Some("hook-system".into());

    let (body, _) = OpenAIEncoder.encode_request(&request).unwrap();

    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "hook-system");
    assert_eq!(body["messages"][1]["role"], "user");
}

#[test]
fn internal_artifact_identity_is_not_sent_upstream() {
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: Some(
                stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                    "__stravia_artifact_references": [{
                        "block_index": 0,
                        "artifact_id": "artifact_secret"
                    }],
                    "reasoning_content": "visible reasoning"
                })),
            ),
        }],
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    assert_eq!(
        body["messages"][0]["reasoning_content"],
        "visible reasoning"
    );
    assert!(
        body["messages"][0]
            .get("__stravia_artifact_references")
            .is_none()
    );
    assert!(!body.to_string().contains("artifact_secret"));
}

#[test]
fn gateway_request_state_is_not_passed_through_upstream() {
    let mut request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let ingress = &mut request.meta.vendor.ingress;
    ingress.insert(
        "__stravia_generation_session_id".into(),
        serde_json::json!("session-secret"),
    );
    ingress.insert(
        stravia_runtime_contract::protocol::ir::request::VERIFIED_HISTORY_REPLAY_META.into(),
        serde_json::Value::Bool(true),
    );
    ingress.insert("client_extension".into(), serde_json::json!("kept"));

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    assert_eq!(body["client_extension"], "kept");
    assert!(!body.to_string().contains("__stravia_"), "{body}");
}

#[test]
fn synthetic_tool_ids_are_distinct_correlated_and_skip_supplied_ids() {
    let request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                tool_calls: Some(vec![
                    ToolCall {
                        id: (String::new()).into(),
                        name: "first".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "tc_1".into(),
                        name: "external".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: (String::new()).into(),
                        name: "second".into(),
                        arguments: "{}".into(),
                    },
                ]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("first result".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("external result".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("second result".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );

    let (first, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    let (repeated, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    let result_ids: Vec<&str> = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["tool_call_id"].as_str().unwrap())
        .collect();

    assert_eq!(result_ids, ["tc_2", "tc_1", "tc_3"]);
    assert_eq!(repeated["messages"], first["messages"]);
}

#[test]
fn duplicate_external_tool_calls_remain_distinct_and_correlated() {
    let request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                tool_calls: Some(vec![
                    ToolCall {
                        id: "external".into(),
                        name: "first".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "external".into(),
                        name: "second".into(),
                        arguments: "{}".into(),
                    },
                ]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("second result".into()),
                tool_calls: None,
                tool_call_id: Some("external".into()),
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("first result".into()),
                tool_calls: None,
                tool_call_id: Some("external".into()),
                meta: None,
            },
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    let result_ids: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["tool_call_id"].as_str().unwrap())
        .collect();

    assert_eq!(result_ids, ["tc_1", "external"]);
}

#[test]
fn responses_reasoning_effort_maps_to_chat_without_loss() {
    use crate::codec::open_responses::decoder::ResponsesDecoder;

    let mut request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "input": "hello",
            "reasoning": { "effort": "xhigh", "summary": "auto" }
        }))
        .unwrap();
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "xhigh".into(),
        },
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).unwrap();

    assert_eq!(body["reasoning_effort"], "xhigh");
    assert!(body.get("reasoning").is_none());
}

#[test]
fn generic_toggle_without_provider_adapter_is_rejected() {
    let mut request = AiRequest::new(
        "custom-model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.reasoning.target_control =
        Some(stravia_runtime_contract::thinking::TargetThinkingControl::Enabled);
    request.meta.source_protocol =
        Some(stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let error = crate::transform::ProtocolTransform::global()
        .bind(
            stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap()
        .encode_request(&request)
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("cannot preserve: reasoning.target_control")
    );
}

#[test]
fn unknown_reasoning_effort_is_rejected() {
    use crate::codec::openai::compatible::decoder::OpenAIDecoder;

    for value in ["future_7", "not safe"] {
        assert!(
            OpenAIDecoder
                .decode_request(serde_json::json!({
                "model": "gpt",
                "messages": [{"role": "user", "content": "hello"}],
                "reasoning_effort": value
                    }))
                .is_err()
        );
    }
}

#[test]
fn thinking_and_reasoning_plaintext_joins_into_reasoning_content() {
    let request = AiRequest::new(
        "deepseek-flash",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Thinking {
                        thinking: "first thought".into(),
                        signature: Some("opaque-sig".into()),
                    },
                    ContentBlock::Reasoning {
                        summary: vec!["summary".into()],
                        content: vec!["detail".into()],
                        encrypted_content: Some("cipher-text".into()),
                    },
                    ContentBlock::RedactedThinking {
                        data: "redacted-payload".into(),
                    },
                    ContentBlock::Text {
                        text: "answer".into(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");

    let assistant = &body["messages"][1];
    // 明文推理按块顺序进入 reasoning_content；受保护载荷没有任何载体。
    assert_eq!(
        assistant["reasoning_content"],
        "first thought\nsummary\ndetail"
    );
    assert_eq!(
        assistant["content"],
        serde_json::json!([{"type": "text", "text": "answer"}])
    );
    let wire = body.to_string();
    assert!(!wire.contains("opaque-sig"), "{wire}");
    assert!(!wire.contains("cipher-text"), "{wire}");
    assert!(!wire.contains("redacted-payload"), "{wire}");
    assert!(!wire.contains("reasoning\""), "{wire}");
}

#[test]
fn protected_only_assistant_item_is_dropped_entirely() {
    for blocks in [
        vec![ContentBlock::RedactedThinking {
            data: "redacted-payload".into(),
        }],
        vec![ContentBlock::Reasoning {
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: Some("cipher-text".into()),
        }],
        vec![ContentBlock::Thinking {
            thinking: String::new(),
            signature: Some("opaque-sig".into()),
        }],
    ] {
        let request = AiRequest::new(
            "deepseek-flash",
            vec![
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("hello".into()),
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
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("continue".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );

        let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");

        // 条目只剩承载不了的受保护载荷：整条跳过，不能发出空 assistant 消息。
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages.iter().all(|m| m["role"] == "user"));
        let wire = body.to_string();
        assert!(!wire.contains("redacted-payload"), "{wire}");
        assert!(!wire.contains("cipher-text"), "{wire}");
        assert!(!wire.contains("opaque-sig"), "{wire}");
    }
}

#[test]
fn signed_thinking_with_tool_calls_keeps_reasoning_content_and_drops_signature() {
    let request = AiRequest::new(
        "deepseek-flash",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::Thinking {
                    thinking: "inspect the repo".into(),
                    signature: Some("opaque-sig".into()),
                }]),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "glob".into(),
                    arguments: "{}".into(),
                }]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("listing".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                meta: None,
            },
        ],
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");

    let assistant = &body["messages"][1];
    assert_eq!(assistant["reasoning_content"], "inspect the repo");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    // 有 tool_calls 时 content 可缺省，但绝不能把签名写进任何字段。
    assert!(!body.to_string().contains("opaque-sig"));
}

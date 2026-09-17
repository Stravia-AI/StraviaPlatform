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
            meta: Some(serde_json::json!({
                "__stravia_artifact_references": [{
                    "block_index": 0,
                    "artifact_id": "artifact_secret"
                }],
                "reasoning_content": "visible reasoning"
            })),
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
fn synthetic_tool_ids_are_distinct_correlated_and_skip_supplied_ids() {
    let request = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                tool_calls: Some(vec![
                    ToolCall {
                        id: String::new(),
                        name: "first".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "tc_1".into(),
                        name: "external".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: String::new(),
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
    use crate::protocol::codec::open_responses::decoder::ResponsesDecoder;

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

    let error = crate::protocol::transform::ProtocolTransform::global()
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
    use crate::protocol::codec::openai::compatible::decoder::OpenAIDecoder;

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

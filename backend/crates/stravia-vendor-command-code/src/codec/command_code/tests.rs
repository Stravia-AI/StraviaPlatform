use super::*;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::MessageContent;

#[test]
fn encodes_generate_envelope() {
    let request = AiRequest::new(
        "deepseek/deepseek-v4-flash",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hi".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let (body, headers) = CommandCodeGenerateV1.encode_request(request).unwrap();

    assert_eq!(
        CommandCodeGenerateV1.request_path("anything", false),
        "/alpha/generate"
    );
    assert!(headers.is_empty());
    assert_eq!(body["mode"], "agent");
    assert_eq!(body["permissionMode"], "standard");
    assert_eq!(body["params"]["model"], "deepseek/deepseek-v4-flash");
    assert_eq!(body["params"]["stream"], true);
    assert_eq!(body["params"]["system"][0]["text"], " ");
    assert_eq!(body["params"]["messages"][0]["content"][0]["text"], "hi");
    assert_eq!(body["params"]["tools"], json!([]));
    assert!(body.get("threadId").is_none());
}

#[test]
fn encodes_tools_without_type_and_aliases_names() {
    let mut request = AiRequest::new(
        "claude-sonnet-4-6",
        vec![AiItem {
            role: Role::System,
            content: MessageContent::Text("be brief".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.tools = Some(vec![ToolSpec {
        name: "bash_output".into(),
        description: Some("read shell output".into()),
        parameters: json!({"type": "object", "properties": {"id": {"type": "string"}}}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    request.tool_choice = Some(ToolChoice::Required);
    let (body, _) = CommandCodeGenerateV1.encode_request(request).unwrap();
    assert_eq!(body["params"]["system"][0]["text"], "be brief");
    assert_eq!(body["params"]["tools"][0]["name"], "shell_output");
    assert!(body["params"]["tools"][0].get("type").is_none());
    assert_eq!(body["params"]["tool_choice"], json!({"type": "any"}));
}

#[test]
fn owned_encoding_preserves_shared_text_and_system_order() {
    let shared = Arc::new("shared system".to_owned());
    let item = |role, text| AiItem {
        role,
        content: MessageContent::Text(text),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new(
        "model",
        vec![
            item(Role::System, shared.clone()),
            item(Role::Developer, Arc::new("last system".into())),
            item(Role::User, Arc::new("question".into())),
            item(Role::Assistant, Arc::new("answer".into())),
        ],
    );
    let (body, _) = CommandCodeGenerateV1.encode_request(request).unwrap();
    assert_eq!(shared.as_str(), "shared system");
    assert_eq!(body["params"]["system"][0]["text"], "shared system\n");
    assert_eq!(body["params"]["system"][1]["text"], "last system");
    assert_eq!(
        body["params"]["messages"][0]["content"][0]["text"],
        "question"
    );
    assert_eq!(
        body["params"]["messages"][1]["content"][0]["text"],
        "answer"
    );
}

#[test]
fn streamed_response_keeps_the_generation_chain_identity() {
    let mut parser = CommandCodeStreamParser::new();
    let mut deltas = parser
        .parse_chunk(
            "{\"type\":\"start\"}\n\
             {\"type\":\"reasoning-delta\",\"text\":\"Use the addition tool.\"}\n\
             {\"type\":\"tool-call\",\"toolCallId\":\"call-add\",\"toolName\":\"add_integers\",\"input\":{\"a\":17,\"b\":25}}\n\
             {\"type\":\"finish-step\",\"finishReason\":\"tool-calls\",\"usage\":{\"inputTokens\":3,\"outputTokens\":5}}\n",
        )
        .unwrap();
    deltas.extend(parser.finish().unwrap());
    // 宿主只在 MessageStart 上绑定已分配的 Generation Chain ID。
    for delta in &mut deltas {
        if let AiStreamDelta::MessageStart { id, model } = delta {
            *id = "response-bound".into();
            *model = "logical-model".into();
        }
    }
    let endpoint = stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
    let pair = stravia_protocol_codec::transform::ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap();
    let (_, mut encoder) = pair.stream().unwrap().into_parts();
    let events = encoder.encode_deltas(&deltas).unwrap();
    let completed = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
        .find(|body| body["type"] == "response.completed")
        .unwrap();
    assert_eq!(completed["response"]["id"], "response-bound");
    let call = completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    assert_eq!(call["call_id"], "call-add");
    assert_eq!(
        serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
        json!({"a": 17, "b": 25})
    );
}

// 上游对每个工具调用同时发 `tool-input-*` 流式事件与裸 `tool-call` 完整回显
// (实测自 /alpha/generate 抓包:3 个并行 read 各 6 个事件,end 与 echo 交错)。
// 每个调用必须只完成一次,且 index 与起始槽位一致 —— 重复完成会把别的调用的
// id/参数安到已占用的槽上,污染客户端历史(重复 function_call + 孤儿输出)。
#[test]
fn interleaved_tool_input_and_bare_tool_call_completes_once() {
    let mut parser = CommandCodeStreamParser::new();
    let deltas = parser
        .parse_chunk(
            "{\"type\":\"tool-input-start\",\"id\":\"t1\",\"toolName\":\"read\"}\n\
             {\"type\":\"tool-input-delta\",\"id\":\"t1\",\"delta\":\"{\\\"a\\\": 1}\"}\n\
             {\"type\":\"tool-input-start\",\"id\":\"t2\",\"toolName\":\"read\"}\n\
             {\"type\":\"tool-input-delta\",\"id\":\"t2\",\"delta\":\"{\\\"b\\\": 2}\"}\n\
             {\"type\":\"tool-input-start\",\"id\":\"t3\",\"toolName\":\"read\"}\n\
             {\"type\":\"tool-input-delta\",\"id\":\"t3\",\"delta\":\"{\\\"c\\\": 3}\"}\n\
             {\"type\":\"tool-input-end\",\"id\":\"t1\"}\n\
             {\"type\":\"tool-call\",\"toolCallId\":\"t1\",\"toolName\":\"read\",\"input\":{\"a\":1}}\n\
             {\"type\":\"tool-input-end\",\"id\":\"t2\"}\n\
             {\"type\":\"tool-call\",\"toolCallId\":\"t2\",\"toolName\":\"read\",\"input\":{\"b\":2}}\n\
             {\"type\":\"tool-input-end\",\"id\":\"t3\"}\n\
             {\"type\":\"tool-call\",\"toolCallId\":\"t3\",\"toolName\":\"read\",\"input\":{\"c\":3}}\n\
             {\"type\":\"finish\",\"finishReason\":\"tool-calls\",\"totalUsage\":{\"inputTokens\":1,\"outputTokens\":2}}\n",
        )
        .unwrap();
    let tool_deltas: Vec<_> = deltas
        .iter()
        .filter(|d| {
            matches!(
                d,
                AiStreamDelta::ToolCallStart { .. }
                    | AiStreamDelta::ToolCallDelta { .. }
                    | AiStreamDelta::ToolCallComplete { .. }
            )
        })
        .collect();
    // 3 调用 × (1 start + 1 delta + 1 complete),没有重复完成,没有错位 index。
    assert_eq!(tool_deltas.len(), 9, "unexpected deltas: {tool_deltas:?}");
    // 流式形态:start/delta 交错在前,end 按上游完成顺序统一在后。
    for (slot, id) in [(0usize, "t1"), (1, "t2"), (2, "t3")] {
        assert!(matches!(
            tool_deltas[slot * 2],
            AiStreamDelta::ToolCallStart { index: i, id: seen, .. }
            if *i == slot && seen == id
        ));
        assert!(matches!(
            tool_deltas[slot * 2 + 1],
            AiStreamDelta::ToolCallDelta { index: i, .. } if *i == slot
        ));
        assert!(matches!(
            tool_deltas[6 + slot],
            AiStreamDelta::ToolCallComplete { index: i, tool_call }
            if *i == slot && tool_call.id == id
        ));
    }
    // 裸 tool-call 的回显没有产生第二个 complete,参数保持流式累积原文。
    assert!(matches!(
        &tool_deltas[6],
        AiStreamDelta::ToolCallComplete { tool_call, .. } if tool_call.arguments == "{\"a\": 1}"
    ));
    assert!(parser.finish().unwrap().is_empty());
}

// 只有裸 `tool-call`(无 tool-input 流式形态)时:每个调用自成一个槽,
// 并行调用不能都挤进 index 0。
#[test]
fn bare_tool_calls_without_input_stream_get_distinct_slots() {
    let mut parser = CommandCodeStreamParser::new();
    let deltas = parser
        .parse_chunk(
            "{\"type\":\"tool-call\",\"toolCallId\":\"a\",\"toolName\":\"read\",\"input\":{\"x\":1}}\n\
             {\"type\":\"tool-call\",\"toolCallId\":\"b\",\"toolName\":\"bash\",\"input\":{\"y\":2}}\n\
             {\"type\":\"finish\",\"finishReason\":\"tool-calls\"}\n",
        )
        .unwrap();
    let slots: Vec<(usize, String)> = deltas
        .iter()
        .filter_map(|d| match d {
            AiStreamDelta::ToolCallComplete { index, tool_call } => {
                Some((*index, tool_call.id.to_string()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(slots, vec![(0, "a".into()), (1, "b".into())]);
}

// 客户端回传的明文推理和平台自身的签名推理，回放给 Command Code 时都必须仍是
// reasoning 部件：改成正文会让模型把推理当作自己说过的话。受保护载荷无法承载，只能省略。
#[test]
fn thinking_replay_keeps_readable_reasoning_native_and_omits_protected_payloads() {
    let user = |text: &str| AiItem {
        role: Role::User,
        content: MessageContent::Text(text.to_owned().into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let redacted = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
            data: "redacted-secret".into(),
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let original = AiRequest::new(
        "deepseek/deepseek-v4.1-flash",
        vec![
            user("go"),
            AiItem::reasoning(
                vec!["summary text".into()],
                vec!["client reasoning".into()],
                Some("encrypted-secret".into()),
            ),
            AiItem::thinking("signed reasoning", Some("signature-secret".into())),
            redacted,
            user("continue"),
        ],
    );

    for preserve in [false, true] {
        let mut request = original.clone();
        stravia_protocol_codec::transform::prepare_thinking_replay(&mut request, |_| preserve);
        let (body, _) = CommandCodeGenerateV1.encode_request(request).unwrap();
        let messages = body["params"]["messages"].as_array().unwrap();
        assert_eq!(
            messages[1]["content"],
            json!([
                {"type": "reasoning", "text": "summary text"},
                {"type": "reasoning", "text": "client reasoning"}
            ])
        );
        assert_eq!(
            messages[2]["content"],
            json!([{"type": "reasoning", "text": "signed reasoning"}])
        );
        // 只剩 RedactedThinking 的条目编码后无部件：本协议没有可承载的部件
        // 类型，整条跳过，body 中绝不应出现空 assistant 消息或 redacted data。
        assert!(
            messages.iter().all(|message| {
                message["role"] != "assistant" || !message["content"].as_array().unwrap().is_empty()
            }),
            "{messages:?}"
        );
        assert_eq!(messages[3]["content"][0]["text"], "continue");
        // 明文推理只能以 reasoning 部件出现，绝不降级为 text 正文。
        let text_parts: Vec<&str> = messages
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect();
        for leaked in ["summary text", "client reasoning", "signed reasoning"] {
            assert!(!text_parts.contains(&leaked), "{text_parts:?}");
        }
        let serialized = body.to_string();
        assert!(!serialized.contains("secret"), "{serialized}");
    }
}

// 裸 `tool-call` 先到、`tool-input-end` 后到:echo 已终结调用,尾随 end 不能报错。
#[test]
fn trailing_tool_input_end_after_bare_tool_call_is_tolerated() {
    let mut parser = CommandCodeStreamParser::new();
    let deltas = parser
        .parse_chunk(
            "{\"type\":\"tool-input-start\",\"id\":\"t1\",\"toolName\":\"read\"}\n\
             {\"type\":\"tool-call\",\"toolCallId\":\"t1\",\"toolName\":\"read\",\"input\":{\"a\":1}}\n\
             {\"type\":\"tool-input-end\",\"id\":\"t1\"}\n\
             {\"type\":\"finish\",\"finishReason\":\"stop\"}\n",
        )
        .unwrap();
    let completes = deltas
        .iter()
        .filter(|d| matches!(d, AiStreamDelta::ToolCallComplete { .. }))
        .count();
    assert_eq!(completes, 1);
}

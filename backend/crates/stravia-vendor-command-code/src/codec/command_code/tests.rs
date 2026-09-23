use super::*;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::MessageContent;

#[test]
fn encodes_generate_envelope() {
    let request = AiRequest::new(
        "deepseek/deepseek-v4-flash",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let (body, headers) = CommandCodeGenerateV1.encode_request(&request).unwrap();

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
            content: MessageContent::Text("be brief".into()),
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
    let (body, _) = CommandCodeGenerateV1.encode_request(&request).unwrap();
    assert_eq!(body["params"]["system"][0]["text"], "be brief");
    assert_eq!(body["params"]["tools"][0]["name"], "shell_output");
    assert!(body["params"]["tools"][0].get("type").is_none());
    assert_eq!(body["params"]["tool_choice"], json!({"type": "any"}));
}

#[test]
fn parses_ndjson_stream_events() {
    let mut parser = CommandCodeStreamParser::new();
    let deltas = parser
        .parse_chunk(
            "{\"type\":\"text-delta\",\"text\":\"hello\"}\n\
             {\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":3,\"outputTokens\":5}}\n",
        )
        .unwrap();
    assert!(matches!(&deltas[0], AiStreamDelta::TextDelta(text) if text == "hello"));
    assert!(matches!(
        deltas[1],
        AiStreamDelta::Usage(Usage {
            total_tokens: 8,
            ..
        })
    ));
    assert!(matches!(&deltas[2], AiStreamDelta::Done { stop_reason } if stop_reason == "stop"));
    assert!(parser.finish().unwrap().is_empty());
}

#[test]
fn ignores_live_envelope_events() {
    let mut parser = CommandCodeStreamParser::new();
    let deltas = parser
        .parse_chunk(
            "{\"type\":\"start\"}\n\
             {\"type\":\"start-step\"}\n\
             {\"type\":\"reasoning-start\",\"id\":\"r1\"}\n\
             {\"type\":\"reasoning-delta\",\"id\":\"r1\",\"text\":\"think\"}\n\
             {\"type\":\"reasoning-end\",\"id\":\"r1\"}\n\
             {\"type\":\"finish-step\"}\n\
             {\"type\":\"finish\",\"finishReason\":\"length\",\"totalUsage\":{\"inputTokens\":1,\"outputTokens\":2}}\n\
             {\"type\":\"provider-metadata\"}\n",
        )
        .unwrap();
    assert!(matches!(&deltas[0], AiStreamDelta::ThinkingDelta(text) if text == "think"));
    assert!(matches!(&deltas[1], AiStreamDelta::Usage(_)));
    assert!(matches!(&deltas[2], AiStreamDelta::Done { stop_reason } if stop_reason == "length"));
    assert!(parser.finish().unwrap().is_empty());
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

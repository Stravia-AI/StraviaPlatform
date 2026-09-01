use super::*;

fn make_req(messages: Vec<AiItem>) -> AiRequest {
    AiRequest::new("test", messages)
}

fn assistant_with_tool(tool_id: &str, tool_name: &str) -> AiItem {
    AiItem {
        role: Role::Assistant,
        content: MessageContent::Text(String::new()),
        tool_calls: Some(vec![ToolCall {
            id: tool_id.to_string(),
            name: tool_name.to_string(),
            arguments: "{}".to_string(),
        }]),
        tool_call_id: None,
        meta: None,
    }
}

fn tool_result_with_id(tool_call_id: &str) -> AiItem {
    AiItem {
        role: Role::Tool,
        content: MessageContent::Text("result".to_string()),
        tool_calls: None,
        tool_call_id: Some(tool_call_id.to_string()),
        meta: None,
    }
}

fn tool_result_no_id() -> AiItem {
    AiItem {
        role: Role::Tool,
        content: MessageContent::Text("result".to_string()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

#[test]
fn test_correlation_by_matching_id() {
    let mut req = make_req(vec![
        assistant_with_tool("call_1", "get_weather"),
        tool_result_with_id("call_1"),
    ]);
    normalize_request_tool_results(&mut req);

    let tool_msg = req.items.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn verified_continuation_tool_result_does_not_synthesize_inherited_function_call() {
    let mut req = make_req(vec![tool_result_with_id("call_1")]);
    req.meta.vendor.ingress.insert(
        "__stravia_verified_history_replay".into(),
        serde_json::Value::Bool(true),
    );

    normalize_request_tool_results(&mut req);

    assert_eq!(req.items.len(), 1);
    assert_eq!(req.items[0].role, Role::Tool);
    assert_eq!(req.items[0].tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn test_correlation_fifo_when_no_id() {
    let mut req = make_req(vec![
        assistant_with_tool("call_abc", "search"),
        tool_result_no_id(),
    ]);
    normalize_request_tool_results(&mut req);

    let tool_msg = req.items.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(
        tool_msg.tool_call_id.as_deref(),
        Some("call_abc"),
        "FIFO fallback should correlate to the single pending call"
    );
}

#[test]
fn test_generated_id_for_empty_tool_call_id() {
    let mut req = make_req(vec![
        AiItem {
            role: Role::Assistant,
            content: MessageContent::Text(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "".to_string(),
                name: "my_tool".to_string(),
                arguments: "{}".to_string(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        tool_result_no_id(),
    ]);
    normalize_request_tool_results(&mut req);

    let asst = req
        .items
        .iter()
        .find(|m| m.role == Role::Assistant)
        .unwrap();
    let tc_id = &asst.tool_calls.as_ref().unwrap()[0].id;
    assert!(
        !tc_id.is_empty(),
        "blank tool_call_id must be replaced with generated id"
    );

    let tool_msg = req.items.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(
        tool_msg.tool_call_id.as_deref(),
        Some(tc_id.as_str()),
        "tool result id must match the generated assistant tool_call id"
    );
}

#[test]
fn test_multiple_tool_calls_fifo_order() {
    let mut req = make_req(vec![
        AiItem {
            role: Role::Assistant,
            content: MessageContent::Text(String::new()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_1".to_string(),
                    name: "tool_a".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    name: "tool_b".to_string(),
                    arguments: "{}".to_string(),
                },
            ]),
            tool_call_id: None,
            meta: None,
        },
        tool_result_no_id(),
        tool_result_no_id(),
    ]);
    normalize_request_tool_results(&mut req);

    let tool_msgs: Vec<_> = req.items.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(
        tool_msgs[0].tool_call_id.as_deref(),
        Some("call_1"),
        "first tool result should map to call_1"
    );
    assert_eq!(
        tool_msgs[1].tool_call_id.as_deref(),
        Some("call_2"),
        "second tool result should map to call_2"
    );
}

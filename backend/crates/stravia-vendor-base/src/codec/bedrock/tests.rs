use super::*;

#[test]
fn encodes_converse_tool_config_not_chat_completions() {
    let mut request = AiRequest::new(
        "anthropic.claude",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("Hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.tools = Some(vec![ToolSpec {
        name: "weather".into(),
        description: None,
        parameters: json!({"type":"object"}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    let (body, _) = BedrockConverseV1.encode_request(&request).unwrap();
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "weather"
    );
    assert!(body.get("messages").is_some());
    assert!(body.get("choices").is_none());
}

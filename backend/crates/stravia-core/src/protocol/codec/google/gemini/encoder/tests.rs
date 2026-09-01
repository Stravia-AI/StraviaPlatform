use super::*;
#[test]
fn rejects_unrepresentable_named_tool_choice() {
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
    request.tool_choice = Some(ToolChoice::Named {
        name: "lookup".into(),
    });

    let error = GoogleEncoder
        .encode_request(&request)
        .expect_err("Gemini encoder cannot silently drop named tool choice");
    assert!(error.to_string().contains("tool_choice"));
}
#[test]
fn encodes_json_schema_response_format_in_generation_config() {
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
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: "answer".into(),
        strict: Some(true),
        schema: serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"answer": {"type": "string"}}
        }),
    });

    let (body, _) = GoogleEncoder
        .encode_request(&request)
        .expect("Gemini supports structured JSON output");

    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        body["generationConfig"]["responseSchema"]["properties"]["answer"]["type"],
        "string"
    );
    assert!(
        body["generationConfig"]["responseSchema"]
            .get("$schema")
            .is_none()
    );
}

#[test]
fn target_controls_replace_raw_gemini_thinking_config() {
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
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "high".into(),
    });
    request.meta.vendor.ingress.insert(
        "__google_generation_config".into(),
        serde_json::json!({"thinkingConfig": {"thinkingBudget": 12}}),
    );

    let (body, _) = GoogleEncoder
        .encode_request(&request)
        .expect("Gemini Thinking Level");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        serde_json::json!({"thinkingLevel": "HIGH"})
    );
}

use super::*;

#[test]
fn decodes_thought_signature_and_function_ids() {
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {
                            "text": "checked the repository",
                            "thought": true,
                            "thoughtSignature": "opaque-reasoning"
                        },
                        {
                            "functionCall": {
                                "id": "call_read",
                                "name": "read",
                                "args": {"path": "Cargo.toml"}
                            }
                        }
                    ]
                },
                {
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "id": "call_read",
                            "name": "read",
                            "response": {"result": "workspace"}
                        }
                    }]
                }
            ]
        }))
        .expect("Gemini request");

    let MessageContent::Blocks(assistant_blocks) = &request.items[0].content else {
        panic!("assistant blocks");
    };
    assert!(matches!(
        &assistant_blocks[0],
        ContentBlock::Thinking {
            thinking,
            signature: Some(signature)
        } if thinking == "checked the repository" && signature == "opaque-reasoning"
    ));
    assert!(matches!(
        &assistant_blocks[1],
        ContentBlock::ToolUse { id, name, .. }
            if id == "call_read" && name == "read"
    ));

    let MessageContent::Blocks(tool_blocks) = &request.items[1].content else {
        panic!("tool blocks");
    };
    assert!(matches!(
        &tool_blocks[0],
        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_read"
    ));
    assert_eq!(request.items[1].tool_call_id.as_deref(), Some("call_read"));
}

#[test]
fn missing_function_ids_are_compact_deterministic_and_skip_supplied_ids() {
    let body = serde_json::json!({
        "contents": [
            {
                "role": "model",
                "parts": [
                    {"functionCall": {"name": "first", "args": {}}},
                    {"functionCall": {"id": "tc_1", "name": "external", "args": {}}}
                ]
            },
            {
                "role": "model",
                "parts": [
                    {"functionCall": {"name": "second", "args": {}}}
                ]
            }
        ]
    });

    let first = GoogleDecoder
        .decode_request(body.clone())
        .expect("Gemini request");
    let repeated = GoogleDecoder.decode_request(body).expect("Gemini request");
    let first_ids: Vec<&str> = first
        .items
        .iter()
        .flat_map(|item| item.tool_calls.iter().flatten())
        .map(|call| call.id.as_str())
        .collect();
    let repeated_ids: Vec<&str> = repeated
        .items
        .iter()
        .flat_map(|item| item.tool_calls.iter().flatten())
        .map(|call| call.id.as_str())
        .collect();

    assert_eq!(first_ids, ["tc_2", "tc_1", "tc_3"]);
    assert_eq!(repeated_ids, first_ids);
}

#[test]
fn include_thoughts_enables_reasoning_without_budget() {
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
            "generationConfig": {
                "thinkingConfig": {"includeThoughts": true}
            }
        }))
        .expect("Gemini request");

    assert!(request.reasoning.enabled);
}

#[test]
fn thinking_level_decodes_case_insensitively() {
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
            "generationConfig": {
                "thinkingConfig": {"thinkingLevel": "HIGH"}
            }
        }))
        .expect("Gemini request");

    assert_eq!(
        request.reasoning.level,
        Some(stravia_runtime_contract::thinking::ThinkingLevel::High)
    );
}

#[test]
fn unknown_thinking_level_is_rejected() {
    let error = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
            "generationConfig": {
                "thinkingConfig": {"thinkingLevel": "turbo"}
            }
        }))
        .expect_err("unknown thinkingLevel must fail");

    assert!(
        error
            .to_string()
            .contains("unsupported Gemini thinkingLevel")
    );
}

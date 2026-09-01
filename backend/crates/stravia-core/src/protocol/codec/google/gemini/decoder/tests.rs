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
        Some(crate::thinking::ThinkingLevel::High)
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

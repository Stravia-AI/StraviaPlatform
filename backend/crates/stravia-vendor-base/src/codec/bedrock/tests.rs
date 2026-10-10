use super::*;

#[test]
fn encodes_converse_tool_config_not_chat_completions() {
    let mut request = AiRequest::new(
        "anthropic.claude",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("Hello".to_owned().into()),
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
    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "weather"
    );
    assert!(body.get("messages").is_some());
    assert!(body.get("choices").is_none());
}

fn user_item(text: &str) -> AiItem {
    AiItem {
        role: Role::User,
        content: MessageContent::Text(text.to_owned().into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

fn assistant_block(block: ContentBlock) -> AiItem {
    AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![block]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

#[test]
fn replays_signed_thinking_and_redacted_natively() {
    let request = AiRequest::new(
        "anthropic.claude",
        vec![
            user_item("hi"),
            assistant_block(ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: Some("sig_protected".into()),
            }),
            assistant_block(ContentBlock::RedactedThinking {
                data: "cmVkYWN0ZWQ=".into(),
            }),
            assistant_block(ContentBlock::Reasoning {
                summary: vec!["summary".into()],
                content: vec!["detail".into()],
                encrypted_content: Some("enc_protected".into()),
            }),
        ],
    );

    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[1]["content"][0],
        json!({"reasoningContent": {"reasoningText": {
            "text": "let me think",
            "signature": "sig_protected",
        }}})
    );
    assert_eq!(
        messages[2]["content"][0],
        json!({"reasoningContent": {"redactedContent": "cmVkYWN0ZWQ="}})
    );
    // encrypted_content 借 reasoningText.signature 原生承载。
    assert_eq!(
        messages[3]["content"][0],
        json!({"reasoningContent": {"reasoningText": {
            "text": "summary\ndetail",
            "signature": "enc_protected",
        }}})
    );
}

#[test]
fn downgrades_unsigned_reasoning_to_text_blocks() {
    // Claude on Bedrock 拒绝无签名 reasoningContent，明文只能走 text 块。
    let request = AiRequest::new(
        "anthropic.claude",
        vec![
            assistant_block(ContentBlock::Thinking {
                thinking: "plain thought".into(),
                signature: None,
            }),
            assistant_block(ContentBlock::Reasoning {
                summary: vec!["s1".into()],
                content: vec![String::new(), "c1".into()],
                encrypted_content: None,
            }),
        ],
    );

    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"], json!([{"text": "plain thought"}]));
    // 每段非空文本一个 text 块，保持原位置顺序。
    assert_eq!(
        messages[1]["content"],
        json!([{"text": "s1"}, {"text": "c1"}])
    );
}

#[test]
fn skips_empty_assistant_without_breaking_tool_pairing() {
    let with_call = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
            data: "cmVkYWN0ZWQ=".into(),
        }]),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            name: "weather".into(),
            arguments: "{}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new(
        "anthropic.claude",
        vec![
            user_item("a"),
            // 无签名且无明文：编码为空，整条跳过。
            assistant_block(ContentBlock::Thinking {
                thinking: String::new(),
                signature: None,
            }),
            // redacted 有原生载体，且 tool call 必须保留。
            with_call,
            AiItem::function_call_output("call_1", json!("sunny")),
            user_item("b"),
        ],
    );

    let (body, _) = BedrockConverseV1.encode_request(request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"],
        json!([
            {"reasoningContent": {"redactedContent": "cmVkYWN0ZWQ="}},
            {"toolUse": {"toolUseId": "call_1", "name": "weather", "input": {}}},
        ])
    );
    assert_eq!(
        messages[2]["content"][0]["toolResult"]["toolUseId"],
        "call_1"
    );
    assert_eq!(messages[3]["content"], json!([{"text": "b"}]));
}

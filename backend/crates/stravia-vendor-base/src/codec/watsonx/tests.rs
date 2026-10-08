use super::*;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;

#[test]
fn encodes_watsonx_model_id_and_uses_a_distinct_stream_route() {
    let request = AiRequest::new(
        "ibm/granite-4-h-small",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let (body, _) = WatsonxTextChatV1.encode_request(&request).unwrap();
    assert_eq!(body["model_id"], "ibm/granite-4-h-small");
    assert!(body.get("model").is_none());
    assert_eq!(
        WatsonxTextChatV1.request_path("ibm/granite-4-h-small", false),
        "/ml/v1/text/chat"
    );
    assert_eq!(
        WatsonxTextChatV1.request_path("ibm/granite-4-h-small", true),
        "/ml/v1/text/chat_stream"
    );
}

#[test]
fn replays_reasoning_content_and_drops_protected_only_assistant() {
    let block = |block: ContentBlock| AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![block]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let request = AiRequest::new(
        "ibm/granite-4-h-small",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            // 只含无法承载的密文载荷：整条 assistant 消息必须被丢弃。
            block(ContentBlock::RedactedThinking {
                data: "redacted".into(),
            }),
            // 明文思考进入 OpenAI 兼容载体的 reasoning_content 字段；
            // signature / encrypted_content 无载体，静默忽略。
            block(ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: Some("sig_protected".into()),
            }),
            block(ContentBlock::Text {
                text: "answer".into(),
                cache_control: None,
            }),
        ],
    );

    let (body, _) = WatsonxTextChatV1.encode_request(&request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["reasoning_content"], "let me think");
    let body_text = serde_json::to_string(&body).unwrap();
    assert!(!body_text.contains("sig_protected"));
    assert!(!body_text.contains("redacted"));
}

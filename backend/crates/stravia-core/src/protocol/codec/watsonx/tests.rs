use super::*;
use crate::protocol::ir::{AiItem, MessageContent, Role};

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

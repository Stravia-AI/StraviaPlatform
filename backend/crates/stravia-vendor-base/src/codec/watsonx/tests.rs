use super::*;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;

#[test]
fn watsonx_filters_unified_controls_without_official_equivalents() {
    let mut request = WatsonxTextChatV1
        .decode_request(serde_json::json!({
            "model_id": "ibm/granite", "messages": [{"role": "user", "content": "hello"}],
            "time_limit": 0, "logprobs": true, "top_logprobs": 0, "n": 0,
            "tool_choice_option": "none", "unknown": {"nested": true}
        }))
        .unwrap();
    request.parameters.store = Some(false);
    request.parameters.audio = Some(stravia_runtime_contract::protocol::ir::AudioOutput {
        voice: Some("alloy".into()),
        format: None,
    });
    request.parameters.top_k = Some(0);
    request.parameters.cache.key = Some("cache".into());
    let (body, _) = WatsonxTextChatV1.encode_request(request).unwrap();
    assert_eq!(body["logprobs"], true);
    assert_eq!(body["top_logprobs"], 0);
    assert_eq!(body["n"], 0);
    assert_eq!(body["tool_choice_option"], "none");
    for unsupported in [
        "time_limit",
        "store",
        "audio",
        "top_k",
        "prompt_cache_key",
        "stream",
        "tool_choice",
        "unknown",
    ] {
        assert!(body.get(unsupported).is_none(), "{unsupported}");
    }
}

#[test]
fn encodes_watsonx_model_id_and_uses_a_distinct_stream_route() {
    let request = AiRequest::new(
        "ibm/granite-4-h-small",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );

    let (body, _) = WatsonxTextChatV1.encode_request(request).unwrap();
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

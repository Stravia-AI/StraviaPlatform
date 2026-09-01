use super::*;

#[test]
fn canonical_system_is_encoded_as_a_system_message() {
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
    request.instructions = Some("hook-system".into());

    let (body, _) = OpenAIEncoder.encode_request(&request).unwrap();

    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "hook-system");
    assert_eq!(body["messages"][1]["role"], "user");
}

#[test]
fn internal_artifact_identity_is_not_sent_upstream() {
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: Some(serde_json::json!({
                "__stravia_artifact_references": [{
                    "block_index": 0,
                    "artifact_id": "artifact_secret"
                }],
                "reasoning_content": "visible reasoning"
            })),
        }],
    );

    let (body, _) = OpenAIEncoder.encode_request(&request).expect("encode");
    assert_eq!(
        body["messages"][0]["reasoning_content"],
        "visible reasoning"
    );
    assert!(
        body["messages"][0]
            .get("__stravia_artifact_references")
            .is_none()
    );
    assert!(!body.to_string().contains("artifact_secret"));
}

#[test]
fn responses_reasoning_effort_maps_to_chat_without_loss() {
    use crate::protocol::codec::open_responses::decoder::ResponsesDecoder;

    let mut request = ResponsesDecoder
        .decode_request(serde_json::json!({
            "model": "gpt",
            "input": "hello",
            "reasoning": { "effort": "xhigh", "summary": "auto" }
        }))
        .unwrap();
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "xhigh".into(),
    });

    let (body, _) = OpenAIEncoder.encode_request(&request).unwrap();

    assert_eq!(body["reasoning_effort"], "xhigh");
    assert!(body.get("reasoning").is_none());
}

#[test]
fn generic_toggle_without_provider_adapter_is_rejected() {
    let mut request = AiRequest::new(
        "custom-model",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Enabled);
    request.meta.source_protocol =
        Some(crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    let error = crate::protocol::transform::ProtocolTransform::global()
        .bind(
            crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap()
        .encode_request(&request)
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("cannot preserve: reasoning.target_control")
    );
}

#[test]
fn unknown_reasoning_effort_is_rejected() {
    use crate::protocol::codec::openai::compatible::decoder::OpenAIDecoder;

    for value in ["future_7", "not safe"] {
        assert!(
            OpenAIDecoder
                .decode_request(serde_json::json!({
                "model": "gpt",
                "messages": [{"role": "user", "content": "hello"}],
                "reasoning_effort": value
                    }))
                .is_err()
        );
    }
}

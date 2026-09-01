use super::*;

fn request_with(extra: Value) -> Value {
    let mut request = serde_json::json!({
        "model": "model",
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 1024
    });
    request
        .as_object_mut()
        .expect("request object")
        .extend(extra.as_object().expect("extra object").clone());
    request
}

#[test]
fn adaptive_thinking_uses_output_effort() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "medium"}
        })))
        .expect("adaptive thinking");

    assert!(request.reasoning.enabled);
    assert_eq!(request.reasoning.effort, Some(ReasoningEffort::Medium));
    assert_eq!(
        request.reasoning.level,
        Some(crate::thinking::ThinkingLevel::Medium)
    );
}

#[test]
fn output_effort_without_thinking_is_preserved() {
    let request = AnthropicDecoder
        .decode_request(request_with(serde_json::json!({
            "output_config": {"effort": "high"}
        })))
        .expect("output effort");

    assert!(request.reasoning.enabled);
    assert_eq!(request.reasoning.effort, Some(ReasoningEffort::High));
    assert_eq!(
        request.reasoning.level,
        Some(crate::thinking::ThinkingLevel::High)
    );
}

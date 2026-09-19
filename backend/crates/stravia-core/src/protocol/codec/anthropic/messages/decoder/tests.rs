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
        Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium)
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
        Some(stravia_runtime_contract::thinking::ThinkingLevel::High)
    );
}

#[test]
fn explicit_effort_overrides_enabled_budget_and_disabled_thinking() {
    for thinking in [
        serde_json::json!({"type": "enabled", "budget_tokens": 1024, "display": "omitted"}),
        serde_json::json!({"type": "disabled", "display": "omitted"}),
    ] {
        let request = AnthropicDecoder
            .decode_request(request_with(serde_json::json!({
                "thinking": thinking,
                "output_config": {"effort": "max"}
            })))
            .unwrap();
        assert_eq!(
            request.reasoning.level,
            Some(stravia_runtime_contract::thinking::ThinkingLevel::Max)
        );
        assert!(request.reasoning.enabled);
        assert_eq!(request.reasoning.budget_tokens, None);
        assert_eq!(request.reasoning.display.as_deref(), Some("omitted"));
    }
}

#[test]
fn explicit_effort_does_not_accept_unknown_thinking_type() {
    assert!(
        AnthropicDecoder
            .decode_request(request_with(serde_json::json!({
                "thinking": {"type": "unknown"},
                "output_config": {"effort": "high"}
            })))
            .is_err()
    );
}

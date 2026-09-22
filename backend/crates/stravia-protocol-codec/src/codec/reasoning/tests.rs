use super::*;

fn make_resp(content: &str) -> AiResponse {
    let mut r = AiResponse::new("", "");
    r.push_output_text(content.to_string());
    r
}

#[test]
fn test_split_think_tags_basic() {
    let (reasoning, text) = split_think_tags("<think>let me think</think>the answer");
    assert_eq!(reasoning.as_deref(), Some("let me think"));
    assert_eq!(text, "the answer");
}

#[test]
fn test_split_think_tags_no_tags() {
    let (reasoning, text) = split_think_tags("just text");
    assert!(reasoning.is_none());
    assert_eq!(text, "just text");
}

#[test]
fn test_split_think_tags_multiple() {
    let (reasoning, text) = split_think_tags("<think>step1</think>middle<think>step2</think>end");
    let r = reasoning.unwrap();
    assert!(r.contains("step1"), "expected step1 in reasoning: {r}");
    assert!(r.contains("step2"), "expected step2 in reasoning: {r}");
    assert_eq!(text, "middleend");
}

#[test]
fn test_split_think_tags_unclosed() {
    let (reasoning, text) = split_think_tags("<think>incomplete");
    assert!(
        reasoning.is_none(),
        "unclosed think should produce no reasoning"
    );
    assert!(
        text.contains("<think>"),
        "unclosed think tag should remain in text"
    );
}

#[test]
fn test_normalize_response_reasoning_no_op_when_already_set() {
    let mut resp = make_resp("<think>should be ignored</think>answer");
    resp.push_reasoning("existing reasoning", None);
    normalize_response_reasoning(&mut resp);
    assert_eq!(
        resp.reasoning_items().next().map(|(text, _)| text),
        Some("existing reasoning")
    );
}

#[test]
fn test_normalize_response_reasoning_extracts_think_tags() {
    let mut resp = make_resp("<think>my reasoning</think>final answer");
    normalize_response_reasoning(&mut resp);
    assert_eq!(
        resp.reasoning_items().next().map(|(text, _)| text),
        Some("my reasoning")
    );
    assert_eq!(resp.output_text(), "final answer");
}

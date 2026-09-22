use super::*;
#[test]
fn minimax_reasoning_split_fallback_think_tag() {
    let mut ai_resp = IrAiResponse::new("resp_2", "minimax-m2.7");
    ai_resp.push_output_text("<think>plan first</think>run ls".to_string());
    ai_resp.stop_reason = Some("stop".to_string());

    normalize_response_reasoning(&mut ai_resp);
    assert_eq!(
        ai_resp.reasoning_items().next().map(|(text, _)| text),
        Some("plan first")
    );
    assert_eq!(ai_resp.output_text(), "run ls");
}
#[test]
fn non_reasoning_model_no_regression() {
    let mut ai_resp = IrAiResponse::new("resp_3", "plain-model");
    ai_resp.push_output_text("hello world".to_string());
    ai_resp.stop_reason = Some("stop".to_string());

    normalize_response_reasoning(&mut ai_resp);
    assert!(ai_resp.reasoning_items().next().is_none());
    assert_eq!(ai_resp.output_text(), "hello world");
}
